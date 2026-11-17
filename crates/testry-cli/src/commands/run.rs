//! `testry test` and `testry verify`: plan and execute.

use testry_adapters::storage::run_files::RunDirectory;
use testry_application::commands::execute_plan::{RunReport, RunRequest};
use testry_application::commands::plan_workspace::PlanPurpose;
use testry_application::ports::OperationContext;
use testry_application::ports::execution::{FileAttribution, InputValidation, SuiteRunResult};
use testry_application::results::RunResultDocument;
use testry_application::settings::EnvironmentSettings;
use testry_domain::environment::ProviderKind;
use testry_domain::results::{Assurance, ExitCondition, RunOutcome, TaskOutcome};

use super::plan_request;
use super::render::selection_summary;
use crate::args::{Cli, RunArgs, RunFormat};
use crate::composition_root::{
    Execution, SignalCancellation, new_lease_id, new_run_id, trust_approval,
};
use crate::config;
use crate::error::CliError;

pub async fn run(
    cli: &Cli,
    args: &RunArgs,
    purpose: PlanPurpose,
    ci: bool,
) -> Result<ExitCondition, CliError> {
    let loaded = config::load(&cli.config)?;
    let settings = &loaded.settings;
    let request = plan_request(&args.selection, purpose, settings, ci)?;

    let environment_name = match &args.environment {
        Some(name) => name.as_str(),
        None => settings.default_environment().as_str(),
    };
    let environment = settings
        .environments()
        .iter()
        .find(|(name, _)| name.as_str() == environment_name)
        .map(|(_, environment)| environment)
        .ok_or_else(|| {
            CliError::configuration(format!(
                "--environment `{environment_name}` is not defined under [environments]"
            ))
        })?;
    let readiness_commands = match environment {
        EnvironmentSettings::Host => Vec::new(),
        // ADR-0006: testry does not create Dev Containers. Tests run as
        // processes inside the container testry itself runs in.
        EnvironmentSettings::DevcontainerCli(_) => {
            return Err(CliError::unsupported(format!(
                "environment `{environment_name}` uses the Dev Container provider, which testry \
                 does not implement (ADR-0006). Run testry inside the project's container and \
                 use an environment with provider = \"host\"; nothing was executed or verified"
            )));
        }
    };

    let trust = trust_approval(&loaded.workspace_root)?;
    let run_id = new_run_id()?;
    let execution = Execution::new(
        &loaded.workspace_root,
        settings,
        &run_id,
        args.format == RunFormat::Jsonl,
    )?;
    let ctx = OperationContext {
        run_id,
        cancellation: SignalCancellation::install(),
        deadline: None,
    };
    let run_request = RunRequest {
        plan: request,
        provider: ProviderKind::Host,
        readiness_commands,
        lease_id: new_lease_id()?,
        trust,
    };

    let report = match execution
        .use_case(settings)
        .execute(&loaded.workspace_root, &run_request, &ctx)
        .await
    {
        Ok(report) => report,
        Err(error) => {
            // A request that never became a run leaves no run directory.
            discard_if_unstarted(&execution.run);
            return Err(error.into());
        }
    };

    let artifacts = execution
        .run
        .root()
        .strip_prefix(&loaded.workspace_root)
        .unwrap_or(execution.run.root());
    match args.format {
        RunFormat::Json => {
            let json = serde_json::to_string_pretty(&RunResultDocument::from(&report.result))
                .map_err(|error| {
                    CliError::storage(format!("the result could not be serialized: {error}"))
                })?;
            println!("{json}");
            eprintln!("artifacts: {}", artifacts.display());
        }
        // The events were written to stdout as they happened.
        RunFormat::Jsonl => eprintln!("artifacts: {}", artifacts.display()),
        RunFormat::Text => {
            for line in text(&report) {
                println!("{line}");
            }
            println!("Artifacts: {}", artifacts.display());
        }
    }
    Ok(ExitCondition::from(report.result.outcome))
}

fn discard_if_unstarted(run: &RunDirectory) {
    if !run.root().join("run.json").exists() {
        let _ = std::fs::remove_dir_all(run.root());
    }
}

fn outcome_text(outcome: TaskOutcome) -> &'static str {
    match outcome {
        TaskOutcome::Passed => "passed",
        TaskOutcome::Failed => "FAILED",
        TaskOutcome::Skipped => "skipped (no test executed)",
        TaskOutcome::Flaky => "flaky",
        TaskOutcome::TimedOut => "TIMED OUT",
        TaskOutcome::Cancelled => "cancelled",
        TaskOutcome::InfrastructureError => "INFRASTRUCTURE ERROR",
    }
}

fn suite_lines(suite: &SuiteRunResult) -> Vec<String> {
    let result = &suite.result;
    let counts = match result.counts {
        Some(counts) => format!(
            "{} passed, {} failed, {} skipped, {} flaky",
            counts.passed, counts.failed, counts.skipped, counts.flaky
        ),
        None => "test counts unknown".to_owned(),
    };
    let files = match &result.executed_files {
        FileAttribution::Known(files) => format!("{} file(s)", files.len()),
        FileAttribution::Unknown => "files unknown".to_owned(),
    };
    let mut lines = vec![format!(
        "  {} ({}): {} | {} | {} | ran {} | {} ms",
        result.suite_id,
        suite.runner.as_str(),
        outcome_text(result.outcome),
        counts,
        files,
        selection_summary(&suite.effective_selection),
        result.duration.as_millis()
    )];
    for case in result
        .cases
        .iter()
        .filter(|case| case.status == testry_domain::results::CaseStatus::Failed)
        .take(10)
    {
        let file = case.file.as_ref().map_or("", |file| file.as_str());
        let title = if case.title_path.is_empty() {
            "(file)".to_owned()
        } else {
            case.title_path.join(" > ")
        };
        lines.push(format!("      failed: {file} > {title}"));
    }
    for finding in &suite.findings {
        lines.push(format!("      policy {}: {}", finding.code, finding.detail));
    }
    for anomaly in &result.anomalies {
        lines.push(format!("      note {}: {}", anomaly.code, anomaly.detail));
    }
    lines
}

fn text(report: &RunReport) -> Vec<String> {
    let result = &report.result;
    let mut lines = vec![format!("Run {} (plan {})", result.run_id, result.plan_id)];
    lines.push(format!("  mode: {}", result.mode.as_str()));

    lines.push(String::new());
    lines.push("Suites:".to_owned());
    for suite in &result.suites {
        lines.extend(suite_lines(suite));
    }
    for planned in report.plan.suites() {
        let ran = result
            .suites
            .iter()
            .any(|suite| suite.result.suite_id == planned.suite_id);
        if !ran {
            let why = if planned.effective_selection.runs_anything() {
                "not started".to_owned()
            } else {
                format!(
                    "not run: {}",
                    selection_summary(&planned.effective_selection)
                )
            };
            lines.push(format!(
                "  {} ({}): {why}",
                planned.suite_id,
                planned.runner.as_str()
            ));
        }
    }

    if let Some(gaps) = &result.shadow_gaps {
        lines.push(String::new());
        if gaps.is_empty() {
            lines.push("Shadow: no failure outside the affected candidate.".to_owned());
        } else {
            lines.push(format!(
                "Shadow: {} failure(s) outside the affected candidate (possible selection gaps, not proof):",
                gaps.len()
            ));
            for gap in gaps {
                let file = gap
                    .file
                    .as_ref()
                    .map_or("(file unknown)", |file| file.as_str());
                lines.push(format!("  {}: {file}", gap.suite_id));
            }
        }
    }

    lines.push(String::new());
    if result.input_validation == InputValidation::Changed {
        lines.push("Inputs changed while the run was in progress: the result does not describe the current code.".to_owned());
    }
    if !result.cleanup.is_complete() {
        lines.push(format!(
            "Cleanup incomplete: {}",
            result.cleanup.residual_resources.join(", ")
        ));
    }
    let outcome = match result.outcome {
        RunOutcome::Passed => "PASSED",
        RunOutcome::Failed => "FAILED",
        RunOutcome::NoWork => "NO WORK (nothing was verified; this is not a pass)",
        RunOutcome::Invalidated => "INVALIDATED (inputs changed)",
        RunOutcome::Cancelled => "CANCELLED",
        RunOutcome::InfrastructureError => "INFRASTRUCTURE ERROR",
    };
    let assurance = match result.assurance {
        Assurance::FullSuite => "full_suite (every configured suite ran in full)",
        Assurance::Advisory => "advisory (not a quality gate)",
    };
    lines.push(format!("Outcome: {outcome}"));
    lines.push(format!("Assurance: {assurance}"));
    lines.push(format!(
        "Time: {} ms total (planning {} ms, environment {} ms, verification {} ms, cleanup {} ms)",
        result.metrics.total_ms,
        result.metrics.planning_ms,
        result.metrics.environment_ready_ms,
        result.metrics.verification_ms,
        result.metrics.cleanup_ms
    ));
    lines
}
