//! Adapter for Storybook stories run through the Storybook Vitest addon
//! (contract verified with Storybook 10.6.7, Vitest 5.1.0 in browser mode
//! with Chromium, on Node 26.13.1).
//!
//! The addon turns stories into Vitest tests of one Vitest project. Turning
//! stories into tests and driving the browser stay with Storybook and
//! Vitest; this adapter runs exactly that one project as a whole and reads
//! the Vitest JSON report. It never selects single story files.
//!
//! Contract facts, each covered by `tests/runner_contract.rs` against the
//! real tools:
//! - `--project <name>` runs only the stories project, also when the same
//!   Vitest configuration has other projects;
//! - every story is one test of its story file, reported with the absolute
//!   path of that file; a failing `play` function is a failed test;
//! - Storybook sends usage telemetry unless it is told not to. The adapter
//!   always tells it not to: the environment of a run is an allowlist, so an
//!   opt-out the user has set would otherwise be lost.

use testry_application::ports::execution::{
    AdapterError, ArtifactLocations, ExecutionDescriptor, PreparedInvocation, ProcessResult,
    ReportBundle, RunnerAdapter, RunnerCapabilities, SuiteResult, ToolVersion,
};
use testry_domain::selection::Selection;
use testry_domain::suite::{RunnerKind, SuiteDefinition};

use super::vitest;

/// Storybook versions whose stories were run through the addon in the
/// contract tests. The runner that testry can probe is Vitest; the Storybook
/// version is recorded for the compatibility table only.
pub const VERIFIED_STORYBOOK_VERSIONS: [&str; 1] = ["10.6.7"];

/// Turns off the usage telemetry of Storybook.
const TELEMETRY_VARIABLE: &str = "STORYBOOK_DISABLE_TELEMETRY";

#[derive(Debug, Clone, Copy, Default)]
pub struct StorybookAdapter;

impl RunnerAdapter for StorybookAdapter {
    fn kind(&self) -> RunnerKind {
        RunnerKind::StorybookVitest
    }

    fn version_command(&self, suite: &SuiteDefinition) -> Option<Vec<String>> {
        vitest::version_command(suite)
    }

    fn parse_version(&self, output: &str) -> Option<ToolVersion> {
        vitest::parse_version(output)
    }

    fn capabilities(&self, version: Option<&ToolVersion>) -> RunnerCapabilities {
        RunnerCapabilities {
            full_run: true,
            // Stories always run as the whole project.
            file_selection: false,
            file_attribution: true,
            structured_report: true,
            worker_control: true,
            version_verified: vitest::is_verified(version),
        }
    }

    fn prepare(
        &self,
        suite: &SuiteDefinition,
        selection: &Selection,
        environment: &ExecutionDescriptor,
        artifacts: &ArtifactLocations,
    ) -> Result<PreparedInvocation, AdapterError> {
        if suite.project().is_none() {
            return Err(AdapterError::Configuration {
                message: "a storybook_vitest suite needs `project`: without it every Vitest \
                          project of the configuration would run"
                    .to_owned(),
            });
        }
        let (whole, widened_because) = match selection {
            Selection::Files { .. } => (
                Selection::All,
                Some("a storybook suite always runs its whole Vitest project".to_owned()),
            ),
            other => (other.clone(), None),
        };
        let mut invocation =
            vitest::prepare_run("storybook_vitest", suite, &whole, environment, artifacts)?;
        invocation.requested_selection = selection.clone();
        if widened_because.is_some() {
            invocation.widened_because = widened_because;
        }
        invocation
            .env
            .insert(TELEMETRY_VARIABLE.to_owned(), "1".to_owned());
        Ok(invocation)
    }

    fn normalize(
        &self,
        invocation: &PreparedInvocation,
        process: &ProcessResult,
        reports: &ReportBundle,
    ) -> Result<SuiteResult, AdapterError> {
        Ok(vitest::normalize_run(invocation, process, reports))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use testry_application::ports::execution::{ReportContent, ReportFormat};
    use testry_application::results::anomaly;
    use testry_domain::results::TaskOutcome;

    use super::*;
    use crate::runners::test_support::{bundle, descriptor, files, locations, process, suite};

    const MIXED: &[u8] =
        include_bytes!("../../../../tests/fixtures/runner-reports/storybook-10.6.7/mixed.json");
    const PASSING: &[u8] =
        include_bytes!("../../../../tests/fixtures/runner-reports/storybook-10.6.7/passing.json");

    /// Recorded with a third story in `Card.stories.ts` tagged `!test`.
    const EXCLUDED_BY_TAG: &[u8] = include_bytes!(
        "../../../../tests/fixtures/runner-reports/storybook-10.6.7/excluded-by-tag.json"
    );

    const COMMAND: [&str; 3] = ["node", "./node_modules/vitest/vitest.mjs", "run"];

    fn prepare(suite: &SuiteDefinition, selection: &Selection) -> PreparedInvocation {
        StorybookAdapter
            .prepare(suite, selection, &descriptor(), &locations("report.json"))
            .unwrap()
    }

    fn recorded() -> PreparedInvocation {
        let mut invocation = prepare(
            &suite(RunnerKind::StorybookVitest, ".", &COMMAND),
            &Selection::All,
        );
        invocation.workspace_root = PathBuf::from("/tmp/fx/storybook");
        invocation
    }

    #[test]
    fn runs_exactly_one_vitest_project_without_telemetry() {
        let suite = suite(RunnerKind::StorybookVitest, ".", &COMMAND);
        let invocation = prepare(&suite, &Selection::All);
        assert_eq!(
            invocation.argv,
            [
                "node",
                "./node_modules/vitest/vitest.mjs",
                "run",
                "--project",
                "suite",
                "--reporter=json",
                "--outputFile=/workspace/.testry/runs/r/tasks/suite/report.json",
                "--maxWorkers=1",
                "--allowOnly=false",
            ]
        );
        assert_eq!(
            invocation
                .env
                .get("STORYBOOK_DISABLE_TELEMETRY")
                .map(String::as_str),
            Some("1")
        );
        assert_eq!(invocation.report_format, ReportFormat::VitestJson);
        assert_eq!(invocation.expected_files, None);
        assert!(invocation.widened_because.is_none());
    }

    /// Stories are never selected one by one.
    #[test]
    fn a_file_selection_becomes_the_whole_project() {
        let suite = suite(RunnerKind::StorybookVitest, ".", &COMMAND);
        let selection = files(&["src/Button.stories.ts"]);
        let invocation = prepare(&suite, &selection);
        assert_eq!(invocation.requested_selection, selection);
        assert_eq!(invocation.effective_selection, Selection::All);
        assert!(invocation.widened_because.is_some());
        assert_eq!(invocation.expected_files, None);
        assert!(
            !invocation
                .argv
                .iter()
                .any(|argument| argument.contains("Button.stories.ts")),
            "{:?}",
            invocation.argv
        );
    }

    #[test]
    fn the_reserved_flags_of_vitest_apply() {
        for flags in [
            &["--project", "other"][..],
            &["--reporter=verbose"],
            &["-t", "Primary"],
            &["--passWithNoTests"],
            &["--watch"],
        ] {
            let mut command = COMMAND.to_vec();
            command.extend_from_slice(flags);
            let suite = suite(RunnerKind::StorybookVitest, ".", &command);
            let error = StorybookAdapter
                .prepare(
                    &suite,
                    &Selection::All,
                    &descriptor(),
                    &locations("report.json"),
                )
                .unwrap_err();
            assert!(
                matches!(error, AdapterError::Configuration { .. }),
                "{flags:?}"
            );
            assert!(error.to_string().contains("storybook_vitest"), "{error}");
        }
    }

    #[test]
    fn the_probed_version_is_the_one_of_vitest_and_files_are_never_selected() {
        let suite = suite(RunnerKind::StorybookVitest, ".", &COMMAND);
        assert_eq!(
            StorybookAdapter.version_command(&suite).unwrap(),
            ["node", "./node_modules/vitest/vitest.mjs", "--version"]
        );
        let version = StorybookAdapter
            .parse_version("vitest/5.1.0 linux-arm64 node-v26.13.1")
            .unwrap();
        assert_eq!(
            (version.tool.as_str(), version.version.as_str()),
            ("vitest", "5.1.0")
        );
        let capabilities = StorybookAdapter.capabilities(Some(&version));
        assert!(capabilities.version_verified);
        assert!(!capabilities.file_selection);
        assert!(capabilities.worker_control && capabilities.file_attribution);
        assert!(!StorybookAdapter.capabilities(None).version_verified);
    }

    /// A known limit: a story that Storybook excludes from testing with the
    /// tag `!test` is not in the report at all, not even as skipped. testry
    /// cannot see that it exists, so the run looks like a complete pass.
    #[test]
    fn a_story_excluded_by_its_tag_is_invisible_in_the_report() {
        let result = StorybookAdapter
            .normalize(
                &recorded(),
                &process(0),
                &bundle(ReportFormat::VitestJson, EXCLUDED_BY_TAG),
            )
            .unwrap();
        assert_eq!(result.outcome, TaskOutcome::Passed);
        let counts = result.counts.unwrap();
        assert_eq!((counts.passed, counts.skipped), (3, 0));
        assert!(
            !result
                .cases
                .iter()
                .any(|case| case.title_path == ["Skipped"]),
            "the excluded story is not reported: {:?}",
            result.cases
        );
    }

    #[test]
    fn real_story_reports_normalize_to_the_right_outcome() {
        let report = |bytes| bundle(ReportFormat::VitestJson, bytes);
        let passing = StorybookAdapter
            .normalize(&recorded(), &process(0), &report(PASSING))
            .unwrap();
        assert_eq!(passing.outcome, TaskOutcome::Passed);
        assert_eq!(passing.counts.unwrap().passed, 3);

        let mixed = StorybookAdapter
            .normalize(&recorded(), &process(1), &report(MIXED))
            .unwrap();
        assert_eq!(mixed.outcome, TaskOutcome::Failed);
        let counts = mixed.counts.unwrap();
        assert_eq!((counts.passed, counts.failed), (3, 1));
        let broken = mixed
            .cases
            .iter()
            .find(|case| case.title_path == ["Broken"])
            .unwrap();
        assert_eq!(
            broken.file.as_ref().unwrap().as_str(),
            "src/Card.stories.ts"
        );
        assert!(broken.failure.as_deref().unwrap().contains("boom"));

        // A story file that cannot be loaded stops the runner before it
        // writes a report: that is not a result.
        let missing = StorybookAdapter
            .normalize(
                &recorded(),
                &process(1),
                &ReportBundle {
                    format: ReportFormat::VitestJson,
                    content: ReportContent::Missing,
                },
            )
            .unwrap();
        assert_eq!(missing.outcome, TaskOutcome::InfrastructureError);
        assert!(
            missing
                .anomalies
                .iter()
                .any(|a| a.code == anomaly::REPORT_MISSING)
        );
    }
}
