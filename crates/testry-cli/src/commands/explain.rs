//! `testry explain`: why one suite of a stored plan has its selection.
//!
//! This only reads a plan document. The document is validated (schema
//! version, plan ID, invariants) and nothing in it is ever executed.

use testry_adapters::storage::plan_files::read_plan;
use testry_application::ports::PortError;
use testry_application::queries::explain::{Explanation, explain};
use testry_domain::ids::SuiteId;
use testry_domain::results::ExitCondition;

use super::render::{diagnostic_text, fallback_text, push_list, suite_lines};
use crate::args::{ExplainArgs, Format};
use crate::error::CliError;

pub fn run(args: &ExplainArgs) -> Result<ExitCondition, CliError> {
    let suite = SuiteId::try_new(args.suite.clone())
        .map_err(|error| CliError::configuration(format!("--suite: {error}")))?;
    let plan = read_plan(&args.plan)?;
    let explanation = explain(&plan, &suite).map_err(PortError::from)?;

    match args.format {
        Format::Json => {
            let json = serde_json::to_string_pretty(&explanation).map_err(|error| {
                CliError::storage(format!("the explanation could not be serialized: {error}"))
            })?;
            println!("{json}");
        }
        Format::Text => {
            for line in text(&explanation) {
                println!("{line}");
            }
        }
    }
    Ok(ExitCondition::Success)
}

fn text(explanation: &Explanation<'_>) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!(
        "Plan {} (mode {})",
        explanation.plan_id,
        explanation.mode.as_str()
    ));
    match explanation.baseline_commit {
        Some(baseline) => lines.push(format!("  baseline: {baseline}")),
        None => lines.push("  baseline: none".to_owned()),
    }
    lines.push(String::new());
    lines.extend(suite_lines(explanation.suite));

    if !explanation.fallbacks.is_empty() {
        lines.push(String::new());
        lines.push("Plan-wide fallbacks:".to_owned());
        push_list(
            &mut lines,
            "  - ",
            explanation
                .fallbacks
                .iter()
                .map(fallback_text)
                .collect::<Vec<_>>()
                .into_iter(),
        );
    }
    if !explanation.diagnostics.is_empty() {
        lines.push(String::new());
        lines.push(format!("Diagnostics ({}):", explanation.diagnostics.len()));
        push_list(
            &mut lines,
            "  - ",
            explanation
                .diagnostics
                .iter()
                .map(diagnostic_text)
                .collect::<Vec<_>>()
                .into_iter(),
        );
    }
    lines
}
