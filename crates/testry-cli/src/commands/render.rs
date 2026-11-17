//! Human-readable rendering of plans and explanations.

use testry_domain::plan::{FallbackReason, PlanDiagnostic, SelectionReason, SuitePlan};
use testry_domain::selection::{OmitReason, Selection};

/// How many list entries are printed before the rest is summarized. The JSON
/// output is always complete.
const MAX_LISTED: usize = 20;

pub fn selection_summary(selection: &Selection) -> String {
    match selection {
        Selection::All => "all".to_owned(),
        Selection::Files { files } => format!("{} test file(s)", files.len()),
        Selection::Omit { reason } => format!("nothing ({})", omit_reason(*reason)),
    }
}

fn omit_reason(reason: OmitReason) -> &'static str {
    match reason {
        OmitReason::NoChanges => "no changes",
        OmitReason::OnlyNonTestInputsChanged => "only reviewed non-test inputs changed",
        OmitReason::NoAffectedTests => "no test of this suite depends on the changes",
    }
}

pub fn reason_text(reason: &SelectionReason) -> String {
    match reason {
        SelectionReason::FullVerification => {
            "the mode runs every suite in scope in full".to_owned()
        }
        SelectionReason::AlwaysRun => "the suite has always_run = true".to_owned(),
        SelectionReason::WholeSuitePolicy => {
            "the suite has selection = \"whole_suite\" and a relevant change exists".to_owned()
        }
        SelectionReason::DirectChange { path } => format!("{path} itself changed"),
        SelectionReason::TransitiveDependency { dependency_path } => {
            let chain: Vec<&str> = dependency_path.iter().map(|path| path.as_str()).collect();
            format!("depends on a changed file: {}", chain.join(" -> "))
        }
        SelectionReason::DeclaredInputChanged { path, pattern } => format!(
            "{path} matches the declared input `{pattern}` and the import graph cannot tell which \
             tests use it"
        ),
        SelectionReason::ExpandedByFallback { fallback } => {
            format!("widened by a plan-wide fallback: {}", kind_name(fallback))
        }
        SelectionReason::UnsupportedCapability { capability } => {
            format!("the runner cannot honour a narrower selection ({capability})")
        }
        SelectionReason::NoChanges => "there is no change relative to the baseline".to_owned(),
        SelectionReason::OnlyNonTestInputsChanged => {
            "only reviewed non-test inputs changed".to_owned()
        }
        SelectionReason::NoAffectedTests => {
            "no test of this suite depends on the changed files".to_owned()
        }
    }
}

fn kind_name(kind: &impl serde::Serialize) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

pub fn fallback_text(fallback: &FallbackReason) -> String {
    match fallback {
        FallbackReason::GlobalInputChanged { path, pattern } => {
            format!("{path} changed and matches the global input `{pattern}`")
        }
        FallbackReason::UnknownDependency { diagnostics } => format!(
            "the dependency graph is incomplete ({diagnostics} diagnostic(s)), so no test can be \
             ruled out"
        ),
        FallbackReason::NoRelatedTests { path } => {
            format!("{path} changed and no test depends on it; it is not assumed to be irrelevant")
        }
    }
}

pub fn diagnostic_text(diagnostic: &PlanDiagnostic) -> String {
    if diagnostic.detail.is_empty() {
        format!("{} {}", diagnostic.code, diagnostic.path)
    } else {
        format!(
            "{} {}: {}",
            diagnostic.code, diagnostic.path, diagnostic.detail
        )
    }
}

/// Appends `items` as an indented list, summarizing a long tail.
pub fn push_list(
    lines: &mut Vec<String>,
    indent: &str,
    items: impl ExactSizeIterator<Item = String>,
) {
    let total = items.len();
    for item in items.take(MAX_LISTED) {
        lines.push(format!("{indent}{item}"));
    }
    if total > MAX_LISTED {
        lines.push(format!(
            "{indent}... and {} more (use --format json for all)",
            total - MAX_LISTED
        ));
    }
}

/// The lines that describe one suite of a plan.
pub fn suite_lines(suite: &SuitePlan) -> Vec<String> {
    let mut lines = Vec::new();
    let project = suite
        .project
        .as_ref()
        .map(|project| format!(", project {project}"))
        .unwrap_or_default();
    lines.push(format!(
        "Suite {} ({}{project})",
        suite.suite_id,
        suite.runner.as_str()
    ));
    lines.push(format!(
        "  runs: {}",
        selection_summary(&suite.effective_selection)
    ));
    if suite.effective_selection != suite.requested_selection {
        lines.push(format!(
            "  requested: {}",
            selection_summary(&suite.requested_selection)
        ));
    }
    if let Selection::Files { files } = &suite.effective_selection {
        push_list(
            &mut lines,
            "    ",
            files
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .into_iter(),
        );
        lines.push(
            "  note: the file filter of the runner is verified before execution; if it is not \
             exact, the whole suite runs"
                .to_owned(),
        );
    }
    lines.push("  why:".to_owned());
    push_list(
        &mut lines,
        "    - ",
        suite
            .reasons
            .iter()
            .map(reason_text)
            .collect::<Vec<_>>()
            .into_iter(),
    );
    if let Some(candidate) = &suite.candidate {
        lines.push(format!(
            "  affected candidate (recorded, not executed): {}",
            selection_summary(&candidate.selection)
        ));
        if let Selection::Files { files } = &candidate.selection {
            push_list(
                &mut lines,
                "    ",
                files
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .into_iter(),
            );
        }
        push_list(
            &mut lines,
            "    - ",
            candidate
                .reasons
                .iter()
                .map(reason_text)
                .collect::<Vec<_>>()
                .into_iter(),
        );
    }
    lines
}

#[cfg(test)]
mod tests {
    use testry_domain::paths::RepoPath;
    use testry_domain::plan::FallbackKind;

    use super::*;

    fn path(raw: &str) -> RepoPath {
        RepoPath::try_new(raw).unwrap()
    }

    #[test]
    fn dependency_paths_read_from_test_to_change() {
        let reason = SelectionReason::TransitiveDependency {
            dependency_path: vec![path("a.test.ts"), path("a.ts"), path("lib.ts")],
        };
        assert_eq!(
            reason_text(&reason),
            "depends on a changed file: a.test.ts -> a.ts -> lib.ts"
        );
    }

    #[test]
    fn fallback_kinds_use_their_contract_names() {
        let reason = SelectionReason::ExpandedByFallback {
            fallback: FallbackKind::NoRelatedTests,
        };
        assert!(reason_text(&reason).ends_with("no_related_tests"));
    }

    #[test]
    fn selections_are_never_described_as_passed() {
        let omit = Selection::Omit {
            reason: OmitReason::NoChanges,
        };
        assert_eq!(selection_summary(&omit), "nothing (no changes)");
        assert_eq!(selection_summary(&Selection::All), "all");
    }

    #[test]
    fn long_lists_are_summarized() {
        let mut lines = Vec::new();
        push_list(
            &mut lines,
            " ",
            (0..25)
                .map(|index| index.to_string())
                .collect::<Vec<_>>()
                .into_iter(),
        );
        assert_eq!(lines.len(), MAX_LISTED + 1);
        assert!(lines.last().unwrap().contains("5 more"));
    }
}
