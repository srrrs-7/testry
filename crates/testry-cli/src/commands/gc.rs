//! `testry gc`: retention of run artifacts and plans, and recovery of what
//! interrupted runs left behind.
//!
//! `--dry-run` reads and reports; it changes nothing, not even the run
//! history. `--apply` takes the workspace lock (so no run is active), brings
//! the history in line with the run directories, stops processes that still
//! carry the marker of a run of this workspace, and removes what is older
//! than the retention period and provably owned by testry.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use testry_adapters::process::unix::{processes_of_owner, stop_processes_of_owner};
use testry_adapters::storage::gc::{GcAction, GcSurvey, apply, survey};
use testry_adapters::storage::history::HistoryDatabase;
use testry_adapters::storage::lock::{WorkspaceLock, workspace_is_busy};
use testry_adapters::storage::run_files::reconcile_history;
use testry_adapters::storage::time::rfc3339_utc;
use testry_domain::results::ExitCondition;

use crate::args::{Cli, Format, GcArgs};
use crate::composition_root::new_run_id;
use crate::config;
use crate::error::CliError;

#[derive(Debug, Serialize)]
struct RunDocument {
    run_id: String,
    state: &'static str,
    ended_at: Option<String>,
    bytes: u64,
    action: &'static str,
    reason: &'static str,
}

#[derive(Debug, Serialize)]
struct PlanDocument {
    plan_id: String,
    modified_at: String,
    bytes: u64,
    action: &'static str,
    reason: &'static str,
}

#[derive(Debug, Serialize)]
struct ProcessDocument {
    run_id: String,
    pid: i32,
    name: String,
    /// `would_stop` (dry run), `stopped` or `still_running`.
    action: &'static str,
}

#[derive(Debug, Serialize)]
struct HistoryDocument {
    completed: Vec<String>,
    interrupted: Vec<String>,
    removed: Vec<String>,
}

#[derive(Debug, Serialize)]
struct GcDocument {
    schema_version: u32,
    /// `dry_run` or `apply`.
    mode: &'static str,
    retention_days: u32,
    cutoff: String,
    /// Whether a run was active. Its processes are not leftovers.
    run_active: bool,
    runs: Vec<RunDocument>,
    plans: Vec<PlanDocument>,
    /// Entries testry does not own. They are never removed.
    unmanaged: Vec<String>,
    leftover_processes: Vec<ProcessDocument>,
    /// What bringing the history in line with the run directories changed.
    /// `null` for a dry run, which does not touch the history.
    history: Option<HistoryDocument>,
    removed_runs: Vec<String>,
    removed_plans: Vec<String>,
    freed_bytes: u64,
    failures: Vec<String>,
}

fn timestamp(seconds: i64) -> String {
    rfc3339_utc(UNIX_EPOCH + Duration::from_secs(u64::try_from(seconds).unwrap_or(0)))
}

fn document(mode: &'static str, retention_days: u32, survey: &GcSurvey) -> GcDocument {
    GcDocument {
        schema_version: 1,
        mode,
        retention_days,
        cutoff: timestamp(survey.cutoff),
        run_active: false,
        runs: survey
            .runs
            .iter()
            .map(|run| RunDocument {
                run_id: run.run_id.clone(),
                state: run.state,
                ended_at: run.ended_at.map(timestamp),
                bytes: run.bytes,
                action: run.action.as_str(),
                reason: run.reason,
            })
            .collect(),
        plans: survey
            .plans
            .iter()
            .map(|plan| PlanDocument {
                plan_id: plan.plan_id.clone(),
                modified_at: timestamp(plan.modified_at),
                bytes: plan.bytes,
                action: plan.action.as_str(),
                reason: plan.reason,
            })
            .collect(),
        unmanaged: survey.unmanaged.clone(),
        leftover_processes: Vec::new(),
        history: None,
        removed_runs: Vec::new(),
        removed_plans: Vec::new(),
        freed_bytes: 0,
        failures: Vec::new(),
    }
}

/// The runs of the survey whose directory proves they are runs of this
/// workspace. Only their processes are ever looked for.
fn verified_runs(survey: &GcSurvey) -> impl Iterator<Item = &str> {
    survey
        .runs
        .iter()
        .filter(|run| run.state != "unverified")
        .map(|run| run.run_id.as_str())
}

pub async fn run(cli: &Cli, args: &GcArgs) -> Result<ExitCondition, CliError> {
    let loaded = config::load(&cli.config)?;
    let settings = &loaded.settings;
    let root = &loaded.workspace_root;
    let storage = settings.storage_directory();
    let retention_days = settings.history_retention_days();
    let now = SystemTime::now();

    let report = if args.apply {
        // No run may be active while its artifacts could be removed.
        let _lock = WorkspaceLock::acquire(root, storage, &new_run_id()?)?;
        let history = HistoryDatabase::open(root, storage)?;
        let repaired = reconcile_history(root, storage, &history, None)?;

        let found = survey(root, storage, retention_days, now)?;
        let mut report = document("apply", retention_days, &found);
        report.history = Some(HistoryDocument {
            completed: repaired.completed,
            interrupted: repaired.interrupted,
            removed: repaired.removed,
        });

        // The lock is held, so no run of this workspace is running: every
        // process that still carries the marker of one of them was left
        // behind.
        for run_id in verified_runs(&found) {
            let before = processes_of_owner(run_id);
            if before.is_empty() {
                continue;
            }
            let left = stop_processes_of_owner(run_id, settings.termination_grace()).await;
            for process in before {
                let still_running = left.iter().any(|other| other.pid == process.pid);
                if still_running {
                    report.failures.push(format!(
                        "process {} ({}) of run {run_id} could not be stopped",
                        process.pid, process.name
                    ));
                }
                report.leftover_processes.push(ProcessDocument {
                    run_id: run_id.to_owned(),
                    pid: process.pid,
                    name: process.name,
                    action: if still_running {
                        "still_running"
                    } else {
                        "stopped"
                    },
                });
            }
        }

        let applied = apply(root, storage, &found);
        history.delete_runs(&applied.removed_runs)?;
        report.removed_runs = applied.removed_runs;
        report.removed_plans = applied.removed_plans;
        report.freed_bytes = applied.freed_bytes;
        report.failures.extend(applied.failures);
        report
    } else {
        let found = survey(root, storage, retention_days, now)?;
        let mut report = document("dry_run", retention_days, &found);
        report.run_active = workspace_is_busy(root, storage);
        // While a run is active, the processes of an unfinished run may be
        // its own. They are only called leftovers when no run is active.
        for run in found.runs.iter().filter(|run| run.state != "unverified") {
            if report.run_active && run.state == "interrupted" {
                continue;
            }
            for process in processes_of_owner(&run.run_id) {
                report.leftover_processes.push(ProcessDocument {
                    run_id: run.run_id.clone(),
                    pid: process.pid,
                    name: process.name,
                    action: "would_stop",
                });
            }
        }
        report.freed_bytes = found
            .runs
            .iter()
            .filter(|run| run.action == GcAction::Delete)
            .map(|run| run.bytes)
            .chain(
                found
                    .plans
                    .iter()
                    .filter(|plan| plan.action == GcAction::Delete)
                    .map(|plan| plan.bytes),
            )
            .sum();
        report
    };

    match args.format {
        Format::Json => {
            let json = serde_json::to_string_pretty(&report).map_err(|error| {
                CliError::storage(format!("the report could not be serialized: {error}"))
            })?;
            println!("{json}");
        }
        Format::Text => {
            for line in text(&report) {
                println!("{line}");
            }
        }
    }
    // Something that should have been removed or stopped is still there.
    Ok(if report.failures.is_empty() {
        ExitCondition::Success
    } else {
        ExitCondition::Infrastructure
    })
}

fn text(report: &GcDocument) -> Vec<String> {
    let dry_run = report.mode == "dry_run";
    let mut lines = vec![format!(
        "testry gc ({}): retention {} day(s), removing what ended before {}",
        if dry_run {
            "dry run, nothing is changed"
        } else {
            "apply"
        },
        report.retention_days,
        report.cutoff
    )];
    if report.run_active {
        lines.push(
            "  a run is active in this workspace; `gc --apply` would be refused now".to_owned(),
        );
    }

    let verb = if dry_run { "would remove" } else { "removed" };
    lines.push(String::new());
    lines.push(format!("Runs ({}):", report.runs.len()));
    for run in &report.runs {
        let action = match (run.action, dry_run) {
            ("delete", true) => "would remove".to_owned(),
            ("delete", false) if report.removed_runs.contains(&run.run_id) => "removed".to_owned(),
            ("delete", false) => "NOT removed".to_owned(),
            _ => "keep".to_owned(),
        };
        lines.push(format!(
            "  {} | {} | {} | {} bytes | {action} ({})",
            run.run_id,
            run.state,
            run.ended_at.as_deref().unwrap_or("-"),
            run.bytes,
            run.reason
        ));
    }
    let plans_to_remove = report
        .plans
        .iter()
        .filter(|plan| plan.action == "delete")
        .count();
    lines.push(format!(
        "Plans: {} stored, {verb} {plans_to_remove}",
        report.plans.len()
    ));
    if !report.unmanaged.is_empty() {
        lines.push(format!(
            "Not managed by testry, left alone: {}",
            report.unmanaged.join(", ")
        ));
    }
    if !report.leftover_processes.is_empty() {
        lines.push("Processes left behind by runs:".to_owned());
        for process in &report.leftover_processes {
            lines.push(format!(
                "  pid {} ({}) of run {}: {}",
                process.pid,
                process.name,
                process.run_id,
                process.action.replace('_', " ")
            ));
        }
    }
    if let Some(history) = &report.history
        && !(history.completed.is_empty()
            && history.interrupted.is_empty()
            && history.removed.is_empty())
    {
        lines.push(format!(
            "History repaired: {} finished run(s) indexed, {} marked interrupted, {} row(s) without a directory removed",
            history.completed.len(),
            history.interrupted.len(),
            history.removed.len()
        ));
    }
    lines.push(format!(
        "{} {} bytes",
        if dry_run { "Would free" } else { "Freed" },
        report.freed_bytes
    ));
    for failure in &report.failures {
        lines.push(format!("FAILED: {failure}"));
    }
    lines
}

#[cfg(test)]
mod tests {
    use testry_adapters::storage::gc::{GcPlanFile, GcRun};

    use super::*;

    fn survey_of(runs: Vec<GcRun>, plans: Vec<GcPlanFile>) -> GcSurvey {
        GcSurvey {
            cutoff: 1_794_000_000,
            runs,
            plans,
            unmanaged: vec!["runs/notes.txt".to_owned()],
        }
    }

    fn run(id: &str, state: &'static str, action: GcAction) -> GcRun {
        GcRun {
            run_id: id.to_owned(),
            state,
            ended_at: (state != "unverified").then_some(1_793_000_000),
            plan_id: None,
            bytes: 100,
            action,
            reason: match action {
                GcAction::Delete => "older_than_retention",
                GcAction::Keep => "within_retention",
            },
        }
    }

    #[test]
    fn a_dry_run_says_what_it_would_do_and_claims_nothing() {
        let found = survey_of(
            vec![
                run("old", "finished", GcAction::Delete),
                run("new", "interrupted", GcAction::Keep),
            ],
            Vec::new(),
        );
        let report = document("dry_run", 14, &found);
        let lines = text(&report).join("\n");
        assert!(lines.contains("dry run, nothing is changed"), "{lines}");
        assert!(lines.contains("old | finished"), "{lines}");
        assert!(
            lines.contains("would remove (older_than_retention)"),
            "{lines}"
        );
        assert!(lines.contains("new | interrupted"), "{lines}");
        assert!(lines.contains("keep (within_retention)"), "{lines}");
        assert!(lines.contains("left alone: runs/notes.txt"), "{lines}");
        assert!(!lines.contains("removed ("), "{lines}");
        assert!(report.history.is_none());
    }

    #[test]
    fn a_run_that_could_not_be_removed_is_not_reported_as_removed() {
        let found = survey_of(vec![run("old", "finished", GcAction::Delete)], Vec::new());
        let mut report = document("apply", 14, &found);
        report
            .failures
            .push("run old could not be removed".to_owned());
        let lines = text(&report).join("\n");
        assert!(lines.contains("NOT removed"), "{lines}");
        assert!(
            lines.contains("FAILED: run old could not be removed"),
            "{lines}"
        );
    }

    #[test]
    fn timestamps_are_utc() {
        assert_eq!(timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(timestamp(-5), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn only_verified_runs_have_their_processes_looked_for() {
        let found = survey_of(
            vec![
                run("a", "finished", GcAction::Keep),
                run("b", "unverified", GcAction::Keep),
                run("c", "interrupted", GcAction::Delete),
            ],
            Vec::new(),
        );
        assert_eq!(verified_runs(&found).collect::<Vec<_>>(), ["a", "c"]);
    }
}
