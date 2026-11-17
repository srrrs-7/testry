//! Adapter for Playwright Test (contract verified with 1.65.1 on Node 26.13.1).
//!
//! A Playwright suite always runs as a whole: positional arguments are
//! regular expressions, not paths, so testry never passes test files. The
//! projects of the configuration, their dependencies (setup projects),
//! `globalSetup`, `webServer`, retries and browser settings are left to the
//! runner; the adapter adds only the report, the output directory, the worker
//! count and the ban on focused tests.
//!
//! Contract facts, each covered by `tests/runner_contract.rs` against the
//! real runner:
//! - `--reporter=json` with `PLAYWRIGHT_JSON_OUTPUT_FILE` writes the report
//!   to that file, also when the run fails before any test starts;
//! - failing tests, a file that cannot be loaded, a focused test under
//!   `--forbid-only` and "no tests found" all exit with 1 and are in the
//!   report;
//! - tests of a project whose setup project failed are reported as skipped
//!   without an attempt;
//! - the report contains the resolved configuration, so a shard, an inverted
//!   grep or a failure limit that narrows the suite is visible after the run,
//!   and so is whether `--forbid-only` took effect;
//! - without `--output` the runner writes `test-results/` into the project.

use std::collections::BTreeMap;

use testry_application::ports::execution::{
    AdapterError, AdditionalFiles, Anomaly, ArtifactLocations, ExecutionDescriptor,
    PreparedInvocation, ProcessResult, ReportBundle, ReportContent, ReportFormat, RunnerAdapter,
    RunnerCapabilities, SuiteResult, ToolVersion,
};
use testry_application::results::{ReportOutcome, anomaly, assemble_suite_result};
use testry_domain::ids::TaskId;
use testry_domain::selection::Selection;
use testry_domain::suite::{RunnerKind, SuiteDefinition};

use super::{Reserved, find_version, reject_reserved};
use crate::reports::playwright_json;

/// Versions whose command line and report format were verified.
pub const VERIFIED_VERSIONS: [&str; 1] = ["1.65.1"];

/// Where the JSON reporter writes its report.
const REPORT_FILE_VARIABLE: &str = "PLAYWRIGHT_JSON_OUTPUT_FILE";

/// Directory below the task artifacts for traces, screenshots and videos.
const OUTPUT_DIRECTORY: &str = "output";

const RESERVED: [Reserved; 5] = [
    Reserved {
        flags: &[
            "--reporter",
            "-j",
            "--workers",
            "--project",
            "--forbid-only",
            "--list",
            "--output",
        ],
        reason: "testry owns the report, the output directory, the worker count, the project \
                 selection (use the `project` key) and the ban on focused tests",
    },
    Reserved {
        flags: &[
            "-g",
            "--grep",
            "-G",
            "--grep-invert",
            "--shard",
            "--last-failed",
            "--last-failed-file",
            "--only-changed",
            "--test-list",
            "--test-list-invert",
            "--max-failures",
            "-x",
        ],
        reason: "it narrows what the suite runs; a suite must run all of its tests",
    },
    Reserved {
        flags: &["--no-deps"],
        reason: "the projects a project depends on (for example its setup) must run",
    },
    Reserved {
        flags: &[
            "--pass-with-no-tests",
            "-u",
            "--update-snapshots",
            "--update-source-method",
            "--ignore-snapshots",
            "--repeat-each",
        ],
        reason: "it would hide missing tests, rewrite or ignore snapshots, or repeat tests in a \
                 way the result does not keep apart",
    },
    Reserved {
        flags: &["--ui", "--ui-host", "--ui-port", "--debug", "--run-agents"],
        reason: "interactive modes never finish on their own",
    },
];

#[derive(Debug, Clone, Copy, Default)]
pub struct PlaywrightAdapter;

impl RunnerAdapter for PlaywrightAdapter {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Playwright
    }

    /// The command up to its `test` argument, followed by `--version`.
    fn version_command(&self, suite: &SuiteDefinition) -> Option<Vec<String>> {
        let command = suite.command();
        // The program itself may be called `test`; the subcommand comes later.
        let end = command
            .iter()
            .skip(1)
            .position(|argument| argument == "test")?
            + 1;
        let mut argv: Vec<String> = command.get(..end)?.to_vec();
        argv.push("--version".to_owned());
        Some(argv)
    }

    fn parse_version(&self, output: &str) -> Option<ToolVersion> {
        // `Version 1.65.1`
        let after = output.split("Version ").nth(1)?;
        find_version(after).map(|version| ToolVersion {
            tool: "playwright".to_owned(),
            version,
        })
    }

    fn capabilities(&self, version: Option<&ToolVersion>) -> RunnerCapabilities {
        let verified =
            version.is_some_and(|version| VERIFIED_VERSIONS.contains(&version.version.as_str()));
        RunnerCapabilities {
            full_run: true,
            // Never offered: positional arguments are regular expressions.
            file_selection: false,
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
        reject_reserved("playwright", suite.command(), &RESERVED)?;
        if !suite
            .command()
            .iter()
            .skip(1)
            .any(|argument| argument == "test")
        {
            return Err(AdapterError::Configuration {
                message: "the playwright suite command must contain the `test` subcommand"
                    .to_owned(),
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
            message: "a playwright suite needs `workers`".to_owned(),
        })?;
        let output_directory = artifacts.task_directory.join(OUTPUT_DIRECTORY);
        let output_directory =
            output_directory
                .to_str()
                .ok_or_else(|| AdapterError::Configuration {
                    message: "the artifact directory is not valid UTF-8".to_owned(),
                })?;

        let (effective_selection, widened_because) = match selection {
            Selection::All => (Selection::All, None),
            Selection::Omit { .. } => {
                return Err(AdapterError::Configuration {
                    message: "an omitted suite has no invocation".to_owned(),
                });
            }
            Selection::Files { .. } => (
                Selection::All,
                Some("a playwright suite always runs as a whole".to_owned()),
            ),
        };

        let mut argv = suite.command().to_vec();
        if let Some(project) = suite.project() {
            // Playwright still runs the projects this one depends on.
            argv.push(format!("--project={}", project.as_str()));
        }
        argv.push("--reporter=json".to_owned());
        // Traces, screenshots and videos belong to the run, not to the work
        // tree: left there, they would be new untracked inputs.
        argv.push(format!("--output={output_directory}"));
        argv.push(format!("--workers={workers}"));
        argv.push("--forbid-only".to_owned());

        Ok(PreparedInvocation {
            suite_id: suite.id().clone(),
            task_id: TaskId::for_suite(suite.id()),
            requested_selection: selection.clone(),
            effective_selection,
            widened_because,
            argv,
            env: BTreeMap::from([(REPORT_FILE_VARIABLE.to_owned(), report_path.to_owned())]),
            cwd: suite.cwd().clone(),
            workspace_root: environment.workspace.execution_root.clone(),
            expected_files: None,
            report_format: ReportFormat::PlaywrightJson,
            report_path: Some(artifacts.report_path.clone()),
            // Setup projects have files of their own that are not test files
            // of the suite.
            additional_files: AdditionalFiles::Expected,
        })
    }

    fn normalize(
        &self,
        invocation: &PreparedInvocation,
        process: &ProcessResult,
        reports: &ReportBundle,
    ) -> Result<SuiteResult, AdapterError> {
        let mut scope_notes = Vec::new();
        let report = match &reports.content {
            ReportContent::Bytes(bytes) => {
                match playwright_json::parse(bytes, &invocation.workspace_root) {
                    Ok(report) => {
                        scope_notes = report.narrowing;
                        if !report.forbid_only {
                            scope_notes.push(
                                "focused tests were not forbidden in this run, so a `.only` may \
                                 have hidden other tests"
                                    .to_owned(),
                            );
                        }
                        ReportOutcome::Parsed(report.parsed)
                    }
                    Err(problem) => ReportOutcome::invalid(problem),
                }
            }
            other => ReportOutcome::content_problem(other)
                .unwrap_or_else(|| ReportOutcome::invalid("the report has no content")),
        };
        let mut result = assemble_suite_result(invocation, process, report);
        // The result stands, but it does not cover the whole suite.
        result
            .anomalies
            .extend(scope_notes.into_iter().map(|detail| Anomaly {
                code: anomaly::SCOPE_LIMITED_BY_RUNNER_CONFIG,
                detail,
            }));
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use testry_domain::ids::RunnerProject;
    use testry_domain::results::TaskOutcome;

    use super::*;
    use crate::runners::test_support::{bundle, descriptor, files, locations, process, suite};

    const MIXED: &[u8] =
        include_bytes!("../../../../tests/fixtures/runner-reports/playwright-1.65.1/mixed.json");
    const PASSING: &[u8] =
        include_bytes!("../../../../tests/fixtures/runner-reports/playwright-1.65.1/passing.json");
    const SETUP_FAILED: &[u8] = include_bytes!(
        "../../../../tests/fixtures/runner-reports/playwright-1.65.1/setup-failed.json"
    );
    const NO_TESTS: &[u8] =
        include_bytes!("../../../../tests/fixtures/runner-reports/playwright-1.65.1/no-tests.json");
    const NARROWED: &[u8] =
        include_bytes!("../../../../tests/fixtures/runner-reports/playwright-1.65.1/narrowed.json");
    const WITHOUT_FORBID_ONLY: &[u8] = include_bytes!(
        "../../../../tests/fixtures/runner-reports/playwright-1.65.1/without-forbid-only.json"
    );

    const COMMAND: [&str; 3] = ["node", "./node_modules/@playwright/test/cli.js", "test"];

    fn prepare(suite: &SuiteDefinition, selection: &Selection) -> PreparedInvocation {
        PlaywrightAdapter
            .prepare(suite, selection, &descriptor(), &locations("report.json"))
            .unwrap()
    }

    /// An invocation whose workspace is where the fixtures were recorded.
    fn recorded() -> PreparedInvocation {
        let mut invocation = prepare(
            &suite(RunnerKind::Playwright, ".", &COMMAND),
            &Selection::All,
        );
        invocation.workspace_root = PathBuf::from("/tmp/fx/playwright");
        invocation
    }

    fn normalize(bytes: &[u8], exit: i32) -> SuiteResult {
        PlaywrightAdapter
            .normalize(
                &recorded(),
                &process(exit),
                &bundle(ReportFormat::PlaywrightJson, bytes),
            )
            .unwrap()
    }

    fn scope_notes(result: &SuiteResult) -> Vec<&str> {
        result
            .anomalies
            .iter()
            .filter(|anomaly| anomaly.code == anomaly::SCOPE_LIMITED_BY_RUNNER_CONFIG)
            .map(|anomaly| anomaly.detail.as_str())
            .collect()
    }

    #[test]
    fn adds_report_output_workers_and_forbids_focus() {
        let suite = suite(RunnerKind::Playwright, ".", &COMMAND);
        let invocation = prepare(&suite, &Selection::All);
        assert_eq!(
            invocation.argv,
            [
                "node",
                "./node_modules/@playwright/test/cli.js",
                "test",
                "--reporter=json",
                "--output=/workspace/.testry/runs/r/tasks/suite/output",
                "--workers=1",
                "--forbid-only",
            ]
        );
        assert_eq!(
            invocation.env,
            BTreeMap::from([(
                "PLAYWRIGHT_JSON_OUTPUT_FILE".to_owned(),
                "/workspace/.testry/runs/r/tasks/suite/report.json".to_owned()
            )])
        );
        assert_eq!(invocation.report_format, ReportFormat::PlaywrightJson);
        assert_eq!(invocation.effective_selection, Selection::All);
        assert_eq!(invocation.expected_files, None);
        assert_eq!(invocation.additional_files, AdditionalFiles::Expected);
    }

    /// Test files are never passed: a positional argument is a regular
    /// expression, and a narrowed E2E run is not what was asked for.
    #[test]
    fn a_file_selection_becomes_the_whole_suite() {
        let suite = suite(RunnerKind::Playwright, ".", &COMMAND);
        let invocation = prepare(&suite, &files(&["tests/e2e/login.spec.ts"]));
        assert_eq!(invocation.effective_selection, Selection::All);
        assert!(invocation.widened_because.is_some());
        assert!(
            !invocation
                .argv
                .iter()
                .any(|argument| argument.contains("login.spec.ts")),
            "{:?}",
            invocation.argv
        );
    }

    /// R18: a project is selected through the `project` key, and the
    /// projects it depends on still run.
    #[test]
    fn r18_a_project_is_selected_without_dropping_its_dependencies() {
        let mut parts = testry_application::settings::test_support::suite_parts(
            "e2e",
            RunnerKind::Playwright,
            &["tests/e2e/**/*.spec.ts"],
        );
        parts.command = COMMAND
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect();
        parts.project = Some(RunnerProject::try_new("chromium").unwrap());
        let suite = SuiteDefinition::try_new(parts).unwrap();
        let invocation = prepare(&suite, &Selection::All);
        assert!(invocation.argv.contains(&"--project=chromium".to_owned()));
        assert!(
            !invocation
                .argv
                .iter()
                .any(|argument| argument == "--no-deps")
        );
    }

    #[test]
    fn commands_that_narrow_hide_or_own_the_run_are_rejected() {
        for flags in [
            &["--reporter=line"][..],
            &["--workers=4"],
            &["-j", "4"],
            &["-j4"],
            &["--project", "chromium"],
            &["--project=chromium"],
            &["--forbid-only"],
            &["--list"],
            &["--output=elsewhere"],
            &["-g", "smoke"],
            &["--grep=smoke"],
            &["-G", "slow"],
            &["--grep-invert=slow"],
            &["--shard=1/3"],
            &["--last-failed"],
            &["--only-changed"],
            &["--only-changed=main"],
            &["--test-list=list.txt"],
            &["--max-failures=1"],
            &["-x"],
            &["--no-deps"],
            &["--pass-with-no-tests"],
            &["-u"],
            &["--update-snapshots"],
            &["--ignore-snapshots"],
            &["--repeat-each=3"],
            &["--ui"],
            &["--debug"],
        ] {
            let mut command = COMMAND.to_vec();
            command.extend_from_slice(flags);
            let suite = suite(RunnerKind::Playwright, ".", &command);
            let error = PlaywrightAdapter
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

        // What the project decides for itself stays: the configuration file,
        // retries, timeouts, traces and the browser.
        for flags in [
            &["--config=playwright.e2e.config.ts"][..],
            &["--retries=2"],
            &["--timeout=60000"],
            &["--trace=on"],
            &["--headed"],
        ] {
            let mut command = COMMAND.to_vec();
            command.extend_from_slice(flags);
            let suite = suite(RunnerKind::Playwright, ".", &command);
            assert!(
                PlaywrightAdapter
                    .prepare(
                        &suite,
                        &Selection::All,
                        &descriptor(),
                        &locations("report.json")
                    )
                    .is_ok(),
                "{flags:?}"
            );
        }

        let without_test = suite(
            RunnerKind::Playwright,
            ".",
            &["node", "./node_modules/@playwright/test/cli.js"],
        );
        let error = PlaywrightAdapter
            .prepare(
                &without_test,
                &Selection::All,
                &descriptor(),
                &locations("report.json"),
            )
            .unwrap_err();
        assert!(error.to_string().contains("`test`"), "{error}");
    }

    #[test]
    fn the_version_command_stops_before_test() {
        let suite = suite(RunnerKind::Playwright, ".", &COMMAND);
        assert_eq!(
            PlaywrightAdapter.version_command(&suite).unwrap(),
            [
                "node",
                "./node_modules/@playwright/test/cli.js",
                "--version"
            ]
        );
        assert_eq!(
            PlaywrightAdapter.parse_version("Version 1.65.1\n"),
            Some(ToolVersion {
                tool: "playwright".to_owned(),
                version: "1.65.1".to_owned()
            })
        );
        assert_eq!(PlaywrightAdapter.parse_version("v26.13.1"), None);

        let verified = ToolVersion {
            tool: "playwright".to_owned(),
            version: "1.65.1".to_owned(),
        };
        let capabilities = PlaywrightAdapter.capabilities(Some(&verified));
        assert!(capabilities.version_verified && capabilities.worker_control);
        assert!(!capabilities.file_selection, "never by file");
        assert!(!PlaywrightAdapter.capabilities(None).version_verified);
    }

    #[test]
    fn real_reports_normalize_to_the_right_outcome() {
        let passing = normalize(PASSING, 0);
        assert_eq!(passing.outcome, TaskOutcome::Passed);
        assert!(passing.anomalies.is_empty(), "{:?}", passing.anomalies);
        let counts = passing.counts.unwrap();
        assert_eq!((counts.passed, counts.failed), (3, 0));

        let mixed = normalize(MIXED, 1);
        assert_eq!(mixed.outcome, TaskOutcome::Failed);
        let counts = mixed.counts.unwrap();
        assert_eq!(
            (counts.passed, counts.failed, counts.skipped, counts.flaky),
            (3, 1, 3, 1),
            "skip, fixme and the expected failure are not passed"
        );

        // R18: the setup project failed, so nothing behind it counts as run.
        let setup_failed = normalize(SETUP_FAILED, 1);
        assert_eq!(setup_failed.outcome, TaskOutcome::Failed);
        assert_eq!(setup_failed.counts.unwrap().passed, 0);

        // P08: no test at all.
        let none = normalize(NO_TESTS, 1);
        assert_eq!(none.outcome, TaskOutcome::Failed);

        // A report that says "all passed" with a failing exit code is not a pass.
        let contradicting = normalize(PASSING, 1);
        assert_eq!(contradicting.outcome, TaskOutcome::Failed);
        assert!(
            contradicting
                .anomalies
                .iter()
                .any(|a| a.code == anomaly::NONZERO_EXIT_WITHOUT_FAILURE)
        );

        let missing = PlaywrightAdapter
            .normalize(
                &recorded(),
                &process(0),
                &ReportBundle {
                    format: ReportFormat::PlaywrightJson,
                    content: ReportContent::Missing,
                },
            )
            .unwrap();
        assert_eq!(missing.outcome, TaskOutcome::InfrastructureError);
    }

    /// A configuration that narrows the suite is stated with the result.
    #[test]
    fn narrowing_by_the_runner_configuration_is_recorded() {
        let narrowed = normalize(NARROWED, 0);
        assert_eq!(
            narrowed.outcome,
            TaskOutcome::Passed,
            "the result itself stands"
        );
        assert_eq!(scope_notes(&narrowed).len(), 3, "{:?}", narrowed.anomalies);

        let unguarded = normalize(WITHOUT_FORBID_ONLY, 0);
        let notes = scope_notes(&unguarded);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains(".only"), "{notes:?}");

        assert!(scope_notes(&normalize(PASSING, 0)).is_empty());
    }
}
