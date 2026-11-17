//! The `command` adapter: runs a fixed argv and knows only whether it
//! succeeded (spec section 9).
//!
//! It has no structured report, so it reports no test counts and no file
//! attribution, and it can never narrow what it runs. It is a gate such as a
//! type check, not a test suite.

use std::collections::BTreeMap;

use testry_application::ports::execution::{
    AdapterError, AdditionalFiles, ArtifactLocations, ExecutionDescriptor, PreparedInvocation,
    ProcessResult, ReportBundle, ReportFormat, RunnerAdapter, RunnerCapabilities, SuiteResult,
    ToolVersion,
};
use testry_application::results::{ReportOutcome, assemble_suite_result};
use testry_domain::ids::TaskId;
use testry_domain::selection::Selection;
use testry_domain::suite::{RunnerKind, SuiteDefinition};

#[derive(Debug, Clone, Copy, Default)]
pub struct CommandAdapter;

impl RunnerAdapter for CommandAdapter {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Command
    }

    fn version_command(&self, _suite: &SuiteDefinition) -> Option<Vec<String>> {
        None
    }

    fn parse_version(&self, _output: &str) -> Option<ToolVersion> {
        None
    }

    fn capabilities(&self, _version: Option<&ToolVersion>) -> RunnerCapabilities {
        RunnerCapabilities {
            full_run: true,
            file_selection: false,
            file_attribution: false,
            structured_report: false,
            worker_control: false,
            version_verified: false,
        }
    }

    fn prepare(
        &self,
        suite: &SuiteDefinition,
        selection: &Selection,
        environment: &ExecutionDescriptor,
        _artifacts: &ArtifactLocations,
    ) -> Result<PreparedInvocation, AdapterError> {
        let widened_because = match selection {
            Selection::All => None,
            Selection::Files { .. } => {
                Some("a command cannot run a subset; the whole command runs".to_owned())
            }
            Selection::Omit { .. } => {
                return Err(AdapterError::Configuration {
                    message: "an omitted suite has no invocation".to_owned(),
                });
            }
        };
        Ok(PreparedInvocation {
            suite_id: suite.id().clone(),
            task_id: TaskId::for_suite(suite.id()),
            requested_selection: selection.clone(),
            effective_selection: Selection::All,
            widened_because,
            argv: suite.command().to_vec(),
            env: BTreeMap::new(),
            cwd: suite.cwd().clone(),
            workspace_root: environment.workspace.execution_root.clone(),
            expected_files: None,
            report_format: ReportFormat::None,
            report_path: None,
            additional_files: AdditionalFiles::Expected,
        })
    }

    fn normalize(
        &self,
        invocation: &PreparedInvocation,
        process: &ProcessResult,
        _reports: &ReportBundle,
    ) -> Result<SuiteResult, AdapterError> {
        Ok(assemble_suite_result(
            invocation,
            process,
            ReportOutcome::NotApplicable,
        ))
    }
}

#[cfg(test)]
mod tests {
    use testry_application::ports::execution::{FileAttribution, ReportContent};
    use testry_domain::results::TaskOutcome;

    use super::*;
    use crate::runners::test_support::{descriptor, files, locations, process, suite};

    fn no_report() -> ReportBundle {
        ReportBundle {
            format: ReportFormat::None,
            content: ReportContent::Missing,
        }
    }

    /// P12: a successful command has unknown test counts.
    #[test]
    fn p12_success_has_unknown_counts_and_files() {
        let suite = suite(RunnerKind::Command, ".", &["bun", "run", "typecheck"]);
        let invocation = CommandAdapter
            .prepare(&suite, &Selection::All, &descriptor(), &locations("unused"))
            .unwrap();
        assert_eq!(invocation.argv, ["bun", "run", "typecheck"]);
        assert_eq!(invocation.report_path, None);

        let passed = CommandAdapter
            .normalize(&invocation, &process(0), &no_report())
            .unwrap();
        assert_eq!(passed.outcome, TaskOutcome::Passed);
        assert_eq!(passed.counts, None);
        assert_eq!(passed.executed_files, FileAttribution::Unknown);

        let failed = CommandAdapter
            .normalize(&invocation, &process(2), &no_report())
            .unwrap();
        assert_eq!(failed.outcome, TaskOutcome::Failed);
    }

    #[test]
    fn a_command_never_claims_to_narrow() {
        let suite = suite(RunnerKind::Command, ".", &["make", "lint"]);
        let invocation = CommandAdapter
            .prepare(
                &suite,
                &files(&["a.test.ts"]),
                &descriptor(),
                &locations("unused"),
            )
            .unwrap();
        assert_eq!(invocation.effective_selection, Selection::All);
        assert!(invocation.widened_because.is_some());
        assert_eq!(invocation.argv, ["make", "lint"]);

        let capabilities = CommandAdapter.capabilities(None);
        assert!(!capabilities.file_selection && !capabilities.structured_report);
        assert!(CommandAdapter.version_command(&suite).is_none());
    }
}
