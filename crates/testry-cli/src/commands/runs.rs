//! `testry runs show` and `testry runs list`: read stored runs. Nothing is
//! modified, not even the run history.

use std::time::{Duration, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Value, json};
use testry_adapters::storage::history::{HistoryDatabase, RunRow, StoredState};
use testry_adapters::storage::lock::workspace_is_busy;
use testry_adapters::storage::run_files::{StoredRun, read_run, stored_state};
use testry_adapters::storage::time::rfc3339_utc;
use testry_domain::ids::RunId;
use testry_domain::results::ExitCondition;

use crate::args::{Cli, Format, RunsListArgs, RunsShowArgs};
use crate::config;
use crate::error::CliError;

#[derive(Debug, Serialize, PartialEq, Eq)]
struct ListedRun {
    run_id: String,
    plan_id: String,
    mode: String,
    /// `finished`, `interrupted`, `running`, or `artifacts_missing` when the
    /// directory of the run is gone.
    state: &'static str,
    /// `null` unless the run finished.
    outcome: Option<String>,
    exit_code: Option<u8>,
    assurance: Option<String>,
    started_at: String,
    finished_at: Option<String>,
    total_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
struct RunList {
    schema_version: u32,
    runs: Vec<ListedRun>,
}

fn timestamp(seconds: i64) -> String {
    rfc3339_utc(UNIX_EPOCH + Duration::from_secs(u64::try_from(seconds).unwrap_or(0)))
}

/// What is shown for one row of the index. The index may lag behind the run
/// directories (it is written after them, and repaired by the next run or
/// `gc --apply`), so the state of a row that is not finished is taken from
/// the directory: a run is never shown as finished without its terminal
/// record, and never as running when no run is active.
fn listed(row: RunRow, stored: &StoredState, run_active: bool) -> ListedRun {
    let mut run = ListedRun {
        run_id: row.run_id,
        plan_id: row.plan_id,
        mode: row.mode,
        state: "finished",
        outcome: row.outcome,
        exit_code: row.exit_code,
        assurance: row.assurance,
        started_at: timestamp(row.started_at),
        finished_at: row.finished_at.map(timestamp),
        total_ms: row.total_ms,
    };
    match (row.state.as_str(), stored) {
        (_, StoredState::Missing) => run.state = "artifacts_missing",
        ("finished", _) => {}
        (_, StoredState::Finished(finish)) => {
            run.outcome = Some(finish.outcome.clone());
            run.exit_code = Some(finish.exit_code);
            run.assurance = Some(finish.assurance.clone());
            run.finished_at = Some(timestamp(finish.finished_at));
            run.total_ms = Some(finish.total_ms);
        }
        ("running", StoredState::Unfinished) if run_active => run.state = "running",
        (_, StoredState::Unfinished) => run.state = "interrupted",
    }
    if run.state != "finished" {
        run.outcome = None;
        run.exit_code = None;
        run.assurance = None;
    }
    run
}

pub fn list(cli: &Cli, args: &RunsListArgs) -> Result<ExitCondition, CliError> {
    let loaded = config::load(&cli.config)?;
    let root = &loaded.workspace_root;
    let storage = loaded.settings.storage_directory();

    // A workspace that never ran anything has no history. None is created.
    let rows = match HistoryDatabase::existing(root, storage) {
        Some(history) => history.list_runs(args.limit)?,
        None => Vec::new(),
    };
    let run_active = workspace_is_busy(root, storage);
    let runs: Vec<ListedRun> = rows
        .into_iter()
        .map(|row| {
            let stored = stored_state(root, storage, &row.run_id);
            listed(row, &stored, run_active)
        })
        .collect();

    match args.format {
        Format::Json => {
            let document = RunList {
                schema_version: 1,
                runs,
            };
            let json = serde_json::to_string_pretty(&document).map_err(|error| {
                CliError::storage(format!("the list could not be serialized: {error}"))
            })?;
            println!("{json}");
        }
        Format::Text => {
            if runs.is_empty() {
                println!("No runs are recorded for this workspace.");
            }
            for run in &runs {
                let result = match (&run.outcome, run.state) {
                    (Some(outcome), _) => format!(
                        "{outcome} (exit {}), {}",
                        run.exit_code
                            .map_or("?".to_owned(), |code| code.to_string()),
                        run.assurance.as_deref().unwrap_or("?")
                    ),
                    (None, "running") => "running".to_owned(),
                    (None, "artifacts_missing") => "artifacts missing".to_owned(),
                    (None, _) => "INTERRUPTED (no result; nothing was verified)".to_owned(),
                };
                println!(
                    "{}  {}  {:<8}  {result}",
                    run.started_at, run.run_id, run.mode
                );
            }
        }
    }
    Ok(ExitCondition::Success)
}

pub fn show(cli: &Cli, args: &RunsShowArgs) -> Result<ExitCondition, CliError> {
    let loaded = config::load(&cli.config)?;
    let run_id = RunId::try_new(args.run_id.clone())
        .map_err(|error| CliError::configuration(format!("run id: {error}")))?;
    let stored = read_run(
        &loaded.workspace_root,
        loaded.settings.storage_directory(),
        &run_id,
    )?;

    let document: Value = match stored {
        StoredRun::Finished(result) => serde_json::from_str(&result).map_err(|error| {
            CliError::storage(format!("the stored result is not valid JSON: {error}"))
        })?,
        // A run without a terminal record did not finish. It is shown as
        // interrupted and never as a result.
        StoredRun::Interrupted(record) => {
            let record: Value = serde_json::from_str(&record).map_err(|error| {
                CliError::storage(format!("the stored run record is not valid JSON: {error}"))
            })?;
            json!({
                "schema_version": 1,
                "run_id": run_id,
                "state": "interrupted",
                "detail": "the run has no terminal result; it was interrupted and verified nothing",
                "run": record,
            })
        }
        StoredRun::NotFound => {
            return Err(CliError::configuration(format!(
                "run {run_id} is not stored in this workspace"
            )));
        }
    };

    match args.format {
        Format::Json => {
            let json = serde_json::to_string_pretty(&document).map_err(|error| {
                CliError::storage(format!("the run could not be serialized: {error}"))
            })?;
            println!("{json}");
        }
        Format::Text => {
            for line in text(&document) {
                println!("{line}");
            }
        }
    }
    // Reading a stored run is a read-only success, whatever the run's outcome was.
    Ok(ExitCondition::Success)
}

fn field<'a>(document: &'a Value, name: &str) -> &'a str {
    document.get(name).and_then(Value::as_str).unwrap_or("?")
}

fn text(document: &Value) -> Vec<String> {
    let mut lines = vec![format!("Run {}", field(document, "run_id"))];
    if document.get("state").and_then(Value::as_str) == Some("interrupted") {
        lines.push("  state: INTERRUPTED (no terminal result; nothing was verified)".to_owned());
        if let Some(run) = document.get("run") {
            lines.push(format!("  mode: {}", field(run, "mode")));
            lines.push(format!("  started at: {}", field(run, "started_at")));
        }
        return lines;
    }
    lines.push(format!("  mode: {}", field(document, "mode")));
    lines.push(format!("  outcome: {}", field(document, "outcome")));
    lines.push(format!("  assurance: {}", field(document, "assurance")));
    lines.push(format!(
        "  exit code of the run: {}",
        document
            .get("exit_code")
            .map_or("?".to_owned(), Value::to_string)
    ));
    lines.push(format!(
        "  input validation: {}",
        field(document, "input_validation")
    ));
    for suite in document
        .get("suites")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let counts = match suite.get("counts") {
            Some(Value::Object(counts)) => format!(
                "{} passed, {} failed, {} skipped, {} flaky",
                counts
                    .get("passed")
                    .map_or("?".to_owned(), Value::to_string),
                counts
                    .get("failed")
                    .map_or("?".to_owned(), Value::to_string),
                counts
                    .get("skipped")
                    .map_or("?".to_owned(), Value::to_string),
                counts.get("flaky").map_or("?".to_owned(), Value::to_string),
            ),
            _ => "test counts unknown".to_owned(),
        };
        lines.push(format!(
            "  {}: {} | {counts}",
            field(suite, "suite_id"),
            field(suite, "outcome")
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_interrupted_run_is_never_summarized_as_a_result() {
        let document = json!({
            "run_id": "123e4567-e89b-42d3-a456-426614174000",
            "state": "interrupted",
            "run": { "mode": "full", "started_at": "2026-11-17T09:00:00Z" },
        });
        let lines = text(&document).join("\n");
        assert!(lines.contains("INTERRUPTED"));
        assert!(!lines.contains("outcome:"));
    }

    fn row(state: &str, outcome: Option<&str>) -> RunRow {
        RunRow {
            run_id: "123e4567-e89b-42d3-a456-426614174000".to_owned(),
            plan_id: "blake3:00".to_owned(),
            mode: "full".to_owned(),
            state: state.to_owned(),
            outcome: outcome.map(str::to_owned),
            exit_code: outcome.map(|_| 0),
            assurance: outcome.map(|_| "full_suite".to_owned()),
            started_at: 1_794_000_000,
            finished_at: outcome.map(|_| 1_794_000_010),
            total_ms: outcome.map(|_| 10_000),
        }
    }

    /// The index alone never makes a run look finished or running.
    #[test]
    fn a_listed_run_follows_its_directory_not_only_the_index() {
        // Indexed as running, no terminal record, no active run: interrupted.
        let interrupted = listed(row("running", None), &StoredState::Unfinished, false);
        assert_eq!(interrupted.state, "interrupted");
        assert_eq!(interrupted.outcome, None);

        // The same row while a run holds the workspace lock.
        assert_eq!(
            listed(row("running", None), &StoredState::Unfinished, true).state,
            "running"
        );

        // An interrupted row stays interrupted while another run is active.
        assert_eq!(
            listed(row("interrupted", None), &StoredState::Unfinished, true).state,
            "interrupted"
        );

        // A finished row whose artifacts are gone is not shown with a result.
        let missing = listed(
            row("finished", Some("passed")),
            &StoredState::Missing,
            false,
        );
        assert_eq!(missing.state, "artifacts_missing");
        assert_eq!(missing.outcome, None);
        assert_eq!(missing.exit_code, None);

        let finished = listed(
            row("finished", Some("passed")),
            &StoredState::Unfinished,
            false,
        );
        assert_eq!(finished.state, "finished");
        assert_eq!(finished.outcome.as_deref(), Some("passed"));
        assert_eq!(finished.started_at, "2026-11-06T21:20:00Z");
    }

    #[test]
    fn unknown_counts_are_shown_as_unknown() {
        let document = json!({
            "run_id": "r", "mode": "full", "outcome": "passed", "assurance": "full_suite",
            "exit_code": 0, "input_validation": "unchanged",
            "suites": [
                { "suite_id": "api", "outcome": "passed", "counts": { "passed": 2, "failed": 0, "skipped": 0, "flaky": 0 } },
                { "suite_id": "typecheck", "outcome": "passed", "counts": null },
            ],
        });
        let lines = text(&document);
        assert!(
            lines.contains(&"  api: passed | 2 passed, 0 failed, 0 skipped, 0 flaky".to_owned())
        );
        assert!(lines.contains(&"  typecheck: passed | test counts unknown".to_owned()));
    }
}
