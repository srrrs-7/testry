//! `testry plan`: compute, store and show a plan. Nothing is executed.

use std::path::Path;

use testry_application::commands::plan_workspace::{PlanPurpose, PlanningOutput};
use testry_domain::results::ExitCondition;

use super::plan_request;
use super::render::{diagnostic_text, fallback_text, push_list, suite_lines};
use crate::args::{Cli, Format, PlanArgs};
use crate::composition_root::{Planning, operation_context};
use crate::config;
use crate::error::CliError;

pub async fn run(cli: &Cli, args: &PlanArgs, ci: bool) -> Result<ExitCondition, CliError> {
    let loaded = config::load(&cli.config)?;
    let settings = &loaded.settings;
    let request = plan_request(&args.selection, PlanPurpose::Inspect, settings, ci)?;

    let planning = Planning::new(&loaded.workspace_root, settings);
    let context = operation_context()?;
    let output = planning
        .use_case(settings)
        .execute(&loaded.workspace_root, &request, &context)
        .await?;
    let stored = planning.plans.store(&output.plan)?;
    let stored = stored
        .strip_prefix(&loaded.workspace_root)
        .unwrap_or(&stored);

    match args.format {
        Format::Json => {
            let json = output.plan.to_json().map_err(|error| {
                CliError::storage(format!("the plan could not be serialized: {error}"))
            })?;
            // stdout carries exactly one JSON object; everything else goes to stderr.
            println!("{json}");
            eprintln!("plan stored at {}", stored.display());
            eprintln!("{}", timing_line(&output));
        }
        Format::Text => {
            for line in text(&output, stored) {
                println!("{line}");
            }
        }
    }
    // Showing a plan is a read-only success, even when it contains no work.
    Ok(ExitCondition::Success)
}

fn timing_line(output: &PlanningOutput) -> String {
    let timings = &output.timings;
    format!(
        "planning took {} ms (snapshot {} ms, inventory {} ms, analysis {} ms, selection {} ms)",
        timings.total.as_millis(),
        timings.snapshot.as_millis(),
        timings.inventory.as_millis(),
        timings.analysis.as_millis(),
        timings.selection.as_millis(),
    )
}

fn text(output: &PlanningOutput, stored: &Path) -> Vec<String> {
    let plan = &output.plan;
    let body = plan.body();
    let mut lines = Vec::new();

    lines.push(format!("Plan {}", plan.id()));
    lines.push(format!("  mode: {}", body.mode.as_str()));
    let assurance = match body.assurance {
        testry_domain::results::Assurance::FullSuite => {
            "full_suite (every configured suite runs in full)"
        }
        testry_domain::results::Assurance::Advisory => {
            "advisory (narrowed or scope-limited; not a quality gate)"
        }
    };
    lines.push(format!("  assurance: {assurance}"));
    match (&body.baseline_commit, &body.changed_paths) {
        (Some(baseline), Some(changed)) => {
            lines.push(format!("  baseline: {baseline}"));
            lines.push(format!("  changed paths: {}", changed.len()));
            push_list(
                &mut lines,
                "    ",
                changed
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .into_iter(),
            );
        }
        _ => lines.push("  baseline: none (no diff is needed in this mode)".to_owned()),
    }
    if body.scope_limited {
        lines.push("  scope: limited by --suite".to_owned());
    }

    if !body.fallbacks.is_empty() {
        lines.push(String::new());
        lines.push("Every suite in scope was widened to its full extent because:".to_owned());
        push_list(
            &mut lines,
            "  - ",
            body.fallbacks
                .iter()
                .map(fallback_text)
                .collect::<Vec<_>>()
                .into_iter(),
        );
    }

    for suite in plan.suites() {
        lines.push(String::new());
        lines.extend(suite_lines(suite));
    }

    if !body.diagnostics.is_empty() {
        lines.push(String::new());
        lines.push(format!("Diagnostics ({}):", body.diagnostics.len()));
        push_list(
            &mut lines,
            "  - ",
            body.diagnostics
                .iter()
                .map(diagnostic_text)
                .collect::<Vec<_>>()
                .into_iter(),
        );
    }

    lines.push(String::new());
    if !plan.has_work() {
        lines.push(
            "Nothing would be executed. That is \"no work\", not a successful verification."
                .to_owned(),
        );
    }
    lines.push(format!("Stored at {}", stored.display()));
    lines.push(timing_line(output));
    lines
}
