//! Adapter for Vitest (contract verified with Vitest 5.1.0 on Node 26.13.1).
//!
//! Contract facts, each covered by `tests/runner_contract.rs` against the
//! real runner:
//! - positional arguments are *substring filters* in every spelling (relative,
//!   `./`-prefixed and absolute): `src/a.test.ts` also runs `src/a.test.tsx`.
//!   A requested file always matches its own filter, so the executed set is a
//!   superset of the request. The application verifies that from the report;
//! - the JSON report names files by absolute path and marks a test that
//!   passed after a retry by a non-empty `failureMessages`;
//! - `--allowOnly=false` turns a focused test into a failure;
//! - one project is run per invocation (`--project <name>`), so results of
//!   different projects never share a report.

use std::collections::BTreeMap;

use testry_application::ports::execution::{
    AdapterError, AdditionalFiles, ArtifactLocations, ExecutionDescriptor, PreparedInvocation,
    ProcessResult, ReportBundle, ReportContent, ReportFormat, RunnerAdapter, RunnerCapabilities,
    SuiteResult, ToolVersion,
};
use testry_application::results::{ReportOutcome, assemble_suite_result};
use testry_domain::ids::TaskId;
use testry_domain::selection::Selection;
use testry_domain::suite::{RunnerKind, SuiteDefinition};

use super::{Reserved, find_version, reject_reserved, relative_to};
use crate::reports::vitest_json;

/// Versions whose file filter and report format were verified.
pub const VERIFIED_VERSIONS: [&str; 1] = ["5.1.0"];

const RESERVED: [Reserved; 4] = [
    Reserved {
        flags: &["--reporter", "--outputFile", "--project", "-p"],
        reason: "testry owns the report and the project selection (use the `project` key)",
    },
    Reserved {
        flags: &[
            "--maxWorkers",
            "--minWorkers",
            "--allowOnly",
            "--watch",
            "-w",
            "--ui",
        ],
        reason: "testry owns the worker count and forbids focused tests and watch mode",
    },
    Reserved {
        flags: &[
            "-t",
            "--testNamePattern",
            "--shard",
            "--changed",
            "--sinceLastRun",
            "--exclude",
            "--bail",
        ],
        reason: "it narrows what the suite runs; a suite must run all of its tests",
    },
    Reserved {
        flags: &["--passWithNoTests", "-u", "--update"],
        reason: "it would hide missing tests or rewrite snapshots",
    },
];

#[derive(Debug, Clone, Copy, Default)]
pub struct VitestAdapter;

impl RunnerAdapter for VitestAdapter {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Vitest
    }

    fn version_command(&self, suite: &SuiteDefinition) -> Option<Vec<String>> {
        version_command(suite)
    }

    fn parse_version(&self, output: &str) -> Option<ToolVersion> {
        parse_version(output)
    }

    fn capabilities(&self, version: Option<&ToolVersion>) -> RunnerCapabilities {
        let verified = is_verified(version);
        RunnerCapabilities {
            full_run: true,
            file_selection: verified,
            file_attribution: true,
            structured_report: true,
            worker_control: true,
            version_verified: verified,
        }
    }

    fn prepare(
        &self,
        suite: &SuiteDefinition,
        selection: &Selection,
        environment: &ExecutionDescriptor,
        artifacts: &ArtifactLocations,
    ) -> Result<PreparedInvocation, AdapterError> {
        prepare_run("vitest", suite, selection, environment, artifacts)
    }

    fn normalize(
        &self,
        invocation: &PreparedInvocation,
        process: &ProcessResult,
        reports: &ReportBundle,
    ) -> Result<SuiteResult, AdapterError> {
        Ok(normalize_run(invocation, process, reports))
    }
}

/// The command up to its `run` argument, followed by `--version`.
pub(crate) fn version_command(suite: &SuiteDefinition) -> Option<Vec<String>> {
    let command = suite.command();
    let end = command.iter().position(|argument| argument == "run")?;
    let mut argv: Vec<String> = command.get(..end)?.to_vec();
    if argv.is_empty() {
        return None;
    }
    argv.push("--version".to_owned());
    Some(argv)
}

/// `vitest/5.1.0 linux-arm64 node-v26.13.1`
pub(crate) fn parse_version(output: &str) -> Option<ToolVersion> {
    let after = output.split("vitest/").nth(1)?;
    find_version(after).map(|version| ToolVersion {
        tool: "vitest".to_owned(),
        version,
    })
}

/// Builds a `vitest run` invocation. `runner` is the name used in messages.
/// Shared with the Storybook adapter, which runs the same command for one
/// whole project.
pub(crate) fn prepare_run(
    runner: &str,
    suite: &SuiteDefinition,
    selection: &Selection,
    environment: &ExecutionDescriptor,
    artifacts: &ArtifactLocations,
) -> Result<PreparedInvocation, AdapterError> {
    reject_reserved(runner, suite.command(), &RESERVED)?;
    if !suite.command().iter().any(|argument| argument == "run") {
        return Err(AdapterError::Configuration {
            message: format!(
                "the {runner} suite command must contain `run`; watch mode never finishes"
            ),
        });
    }
    let report_path =
        artifacts
            .report_path
            .to_str()
            .ok_or_else(|| AdapterError::Configuration {
                message: "the report path is not valid UTF-8".to_owned(),
            })?;
    let workers = suite.workers().ok_or_else(|| AdapterError::Configuration {
        message: format!("a {runner} suite needs `workers`"),
    })?;

    let mut argv = suite.command().to_vec();
    let mut effective_selection = selection.clone();
    let mut widened_because = None;
    let mut expected_files = None;
    match selection {
        Selection::All => {}
        Selection::Omit { .. } => {
            return Err(AdapterError::Configuration {
                message: "an omitted suite has no invocation".to_owned(),
            });
        }
        Selection::Files { files } => {
            let relative: Option<Vec<String>> = files
                .iter()
                .map(|file| relative_to(suite.cwd(), file))
                .collect();
            match relative {
                Some(relative) => {
                    argv.extend(relative);
                    expected_files = Some(files.as_set().clone());
                }
                None => {
                    effective_selection = Selection::All;
                    widened_because = Some(
                        "a selected test file is outside the suite directory, so the whole suite runs"
                            .to_owned(),
                    );
                }
            }
        }
    }
    if let Some(project) = suite.project() {
        argv.push("--project".to_owned());
        argv.push(project.as_str().to_owned());
    }
    argv.push("--reporter=json".to_owned());
    argv.push(format!("--outputFile={report_path}"));
    argv.push(format!("--maxWorkers={workers}"));
    argv.push("--allowOnly=false".to_owned());

    Ok(PreparedInvocation {
        suite_id: suite.id().clone(),
        task_id: TaskId::for_suite(suite.id()),
        requested_selection: selection.clone(),
        effective_selection,
        widened_because,
        argv,
        env: BTreeMap::new(),
        cwd: suite.cwd().clone(),
        workspace_root: environment.workspace.execution_root.clone(),
        expected_files,
        report_format: ReportFormat::VitestJson,
        report_path: Some(artifacts.report_path.clone()),
        // A filter is a substring match and may run more files.
        additional_files: AdditionalFiles::Expected,
    })
}

pub(crate) fn normalize_run(
    invocation: &PreparedInvocation,
    process: &ProcessResult,
    reports: &ReportBundle,
) -> SuiteResult {
    let report = match &reports.content {
        ReportContent::Bytes(bytes) => {
            match vitest_json::parse(bytes, &invocation.workspace_root) {
                Ok(parsed) => ReportOutcome::Parsed(parsed),
                Err(problem) => ReportOutcome::invalid(problem),
            }
        }
        other => ReportOutcome::content_problem(other)
            .unwrap_or_else(|| ReportOutcome::invalid("the report has no content")),
    };
    assemble_suite_result(invocation, process, report)
}

/// Whether file selection and the report format were verified for a version.
pub(crate) fn is_verified(version: Option<&ToolVersion>) -> bool {
    version.is_some_and(|version| VERIFIED_VERSIONS.contains(&version.version.as_str()))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use testry_application::results::anomaly;
    use testry_domain::results::TaskOutcome;

    use super::*;
    use crate::runners::test_support::{bundle, descriptor, files, locations, process, suite};

    const MIXED: &[u8] =
        include_bytes!("../../../../tests/fixtures/runner-reports/vitest-5.1.0/mixed.json");
    const PASSING: &[u8] =
        include_bytes!("../../../../tests/fixtures/runner-reports/vitest-5.1.0/passing.json");
    const FLAKY: &[u8] =
        include_bytes!("../../../../tests/fixtures/runner-reports/vitest-5.1.0/flaky.json");
    const NO_MATCH: &[u8] =
        include_bytes!("../../../../tests/fixtures/runner-reports/vitest-5.1.0/no-match.json");

    const COMMAND: [&str; 3] = ["node", "./node_modules/vitest/vitest.mjs", "run"];

    fn prepare(suite: &SuiteDefinition, selection: &Selection) -> PreparedInvocation {
        VitestAdapter
            .prepare(suite, selection, &descriptor(), &locations("report.json"))
            .unwrap()
    }

    /// An invocation whose workspace is where the fixtures were recorded.
    fn recorded(selection: &Selection) -> PreparedInvocation {
        let mut invocation = prepare(&suite(RunnerKind::Vitest, ".", &COMMAND), selection);
        invocation.workspace_root = PathBuf::from("/tmp/fx/vitest");
        invocation
    }

    fn version(text: &str) -> ToolVersion {
        ToolVersion {
            tool: "vitest".to_owned(),
            version: text.to_owned(),
        }
    }

    #[test]
    fn adds_project_report_workers_and_forbids_focus() {
        let suite = suite(RunnerKind::Vitest, "apps/web", &COMMAND);
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
        assert!(invocation.env.is_empty());
        assert_eq!(invocation.expected_files, None);
    }

    #[test]
    fn selected_files_become_filters_relative_to_the_suite_directory() {
        let suite = suite(RunnerKind::Vitest, "apps/web", &COMMAND);
        let selection = files(&["apps/web/src/a.test.ts", "apps/web/src/日本語 [x].test.ts"]);
        let invocation = prepare(&suite, &selection);
        assert_eq!(
            invocation.argv.get(3..5).unwrap(),
            ["src/a.test.ts", "src/日本語 [x].test.ts"]
        );
        assert_eq!(invocation.additional_files, AdditionalFiles::Expected);
        assert_eq!(invocation.expected_files.as_ref().unwrap().len(), 2);

        let outside = prepare(
            &suite,
            &files(&["apps/web/a.test.ts", "apps/api/b.test.ts"]),
        );
        assert_eq!(outside.effective_selection, Selection::All);
        assert!(outside.widened_because.is_some());
    }

    #[test]
    fn commands_that_narrow_watch_or_own_the_report_are_rejected() {
        for flags in [
            &["--reporter=verbose"][..],
            &["--outputFile=x.json"],
            &["--project", "other"],
            &["-p", "other"],
            &["--maxWorkers=8"],
            &["--allowOnly"],
            &["--watch"],
            &["-t", "name"],
            &["--shard=1/2"],
            &["--changed"],
            &["--passWithNoTests"],
            &["--exclude", "**/slow/**"],
            &["-u"],
            &["--bail=1"],
        ] {
            let mut command = COMMAND.to_vec();
            command.extend_from_slice(flags);
            let suite = suite(RunnerKind::Vitest, ".", &command);
            let error = VitestAdapter
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
        }

        let watch = suite(
            RunnerKind::Vitest,
            ".",
            &["node", "./node_modules/vitest/vitest.mjs"],
        );
        let error = VitestAdapter
            .prepare(
                &watch,
                &Selection::All,
                &descriptor(),
                &locations("report.json"),
            )
            .unwrap_err();
        assert!(error.to_string().contains("run"), "{error}");

        // The runner's own retries are respected; attempts stay visible as flaky.
        let mut with_retry = COMMAND.to_vec();
        with_retry.push("--retry=2");
        let suite = suite(RunnerKind::Vitest, ".", &with_retry);
        assert!(
            VitestAdapter
                .prepare(
                    &suite,
                    &Selection::All,
                    &descriptor(),
                    &locations("report.json")
                )
                .is_ok()
        );
    }

    #[test]
    fn the_version_command_stops_before_run() {
        let suite = suite(RunnerKind::Vitest, ".", &COMMAND);
        assert_eq!(
            VitestAdapter.version_command(&suite).unwrap(),
            ["node", "./node_modules/vitest/vitest.mjs", "--version"]
        );
        assert_eq!(
            VitestAdapter.parse_version("vitest/5.1.0 linux-arm64 node-v26.13.1\n"),
            Some(version("5.1.0"))
        );
        // The Node version on the same line is not mistaken for the runner version.
        assert_eq!(VitestAdapter.parse_version("node-v26.13.1"), None);
    }

    /// P05: only verified versions get a file selection.
    #[test]
    fn p05_file_selection_needs_a_verified_version() {
        assert!(
            VitestAdapter
                .capabilities(Some(&version("5.1.0")))
                .file_selection
        );
        assert!(
            VitestAdapter
                .capabilities(Some(&version("5.1.0")))
                .worker_control
        );
        assert!(
            !VitestAdapter
                .capabilities(Some(&version("4.0.0")))
                .file_selection
        );
        assert!(!VitestAdapter.capabilities(None).file_selection);
        assert!(VitestAdapter.capabilities(None).full_run);
    }

    #[test]
    fn real_reports_normalize_to_the_right_outcome() {
        let all = recorded(&Selection::All);
        let adapter = VitestAdapter;
        let report = |bytes| bundle(ReportFormat::VitestJson, bytes);

        let passing = adapter
            .normalize(&all, &process(0), &report(PASSING))
            .unwrap();
        assert_eq!(passing.outcome, TaskOutcome::Passed);

        let mixed = adapter
            .normalize(&all, &process(1), &report(MIXED))
            .unwrap();
        assert_eq!(mixed.outcome, TaskOutcome::Failed);
        let counts = mixed.counts.unwrap();
        assert_eq!(
            (counts.passed, counts.failed, counts.skipped, counts.flaky),
            (5, 1, 3, 1)
        );

        // P10: retried and passed is flaky, not passed.
        let flaky = adapter
            .normalize(&all, &process(0), &report(FLAKY))
            .unwrap();
        assert_eq!(flaky.outcome, TaskOutcome::Flaky);

        // A filter without a match: non-zero exit, empty report.
        let no_match = adapter
            .normalize(
                &recorded(&files(&["src/does-not-exist.test.ts"])),
                &process(1),
                &report(NO_MATCH),
            )
            .unwrap();
        assert_eq!(no_match.outcome, TaskOutcome::Failed);
        assert!(
            no_match
                .anomalies
                .iter()
                .any(|a| a.code == anomaly::EXPECTED_FILES_MISSING)
        );
    }

    /// P09: the filter is a substring match, so a superset is fine, but every
    /// requested file must be in the report.
    #[test]
    fn p09_requested_files_must_be_in_the_report() {
        let report = bundle(ReportFormat::VitestJson, PASSING);
        let subset = recorded(&files(&["src/math.test.ts"]));
        let result = VitestAdapter
            .normalize(&subset, &process(0), &report)
            .unwrap();
        assert_eq!(
            result.outcome,
            TaskOutcome::Passed,
            "running more files is allowed"
        );
        assert!(result.anomalies.is_empty());

        let missing = recorded(&files(&["src/math.test.ts", "src/other.test.ts"]));
        let result = VitestAdapter
            .normalize(&missing, &process(0), &report)
            .unwrap();
        assert_eq!(result.outcome, TaskOutcome::InfrastructureError);
    }

    /// P08: exit code 0 without a report is not a pass.
    #[test]
    fn p08_a_missing_report_is_an_infrastructure_error() {
        let missing = ReportBundle {
            format: ReportFormat::VitestJson,
            content: ReportContent::Missing,
        };
        let result = VitestAdapter
            .normalize(&recorded(&Selection::All), &process(0), &missing)
            .unwrap();
        assert_eq!(result.outcome, TaskOutcome::InfrastructureError);
    }
}
