//! Adapter for `bun test` (contract verified with Bun 1.4.4).
//!
//! Contract facts, each covered by `tests/runner_contract.rs` against the
//! real runner:
//! - a positional argument that starts with `./` selects exactly that file;
//!   without the prefix it is a substring filter (`src/a.test.ts` also runs
//!   `src/a.test.tsx`). The adapter always passes `./<path>`;
//! - the JUnit report lists each executed file with paths relative to the
//!   working directory;
//! - a file with zero tests, a syntax error or an error at import is absent
//!   from the report, sometimes with exit code 0. The expected-file check in
//!   the application catches that;
//! - `test.only` silently drops the other tests of the file from the run and
//!   from the report. With `CI=true` in the environment Bun refuses `.only`,
//!   so the adapter always sets it. `CI=true` also keeps Bun from writing new
//!   snapshots.

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
use crate::reports::bun_junit;

/// Versions whose file filter and report format were verified.
pub const VERIFIED_VERSIONS: [&str; 1] = ["1.4.4"];

const RESERVED: [Reserved; 4] = [
    Reserved {
        flags: &["--reporter", "--reporter-outfile", "--dots"],
        reason: "testry owns the report of the runner",
    },
    Reserved {
        flags: &[
            "--only",
            "-t",
            "--test-name-pattern",
            "--shard",
            "--changed",
            "--path-ignore-patterns",
            "--bail",
        ],
        reason: "it narrows what the suite runs; a suite must run all of its tests",
    },
    Reserved {
        flags: &[
            "--pass-with-no-tests",
            "-u",
            "--update-snapshots",
            "--rerun-each",
            "--retry",
        ],
        reason: "it would hide missing tests, rewrite snapshots or hide flaky tests",
    },
    Reserved {
        flags: &["--parallel", "--test-worker"],
        reason: "worker processes are not covered by the CPU reservation of a bun suite in v0.1",
    },
];

#[derive(Debug, Clone, Copy, Default)]
pub struct BunAdapter;

impl RunnerAdapter for BunAdapter {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Bun
    }

    fn version_command(&self, suite: &SuiteDefinition) -> Option<Vec<String>> {
        let program = suite.command().first()?;
        Some(vec![program.clone(), "--version".to_owned()])
    }

    fn parse_version(&self, output: &str) -> Option<ToolVersion> {
        find_version(output).map(|version| ToolVersion {
            tool: "bun".to_owned(),
            version,
        })
    }

    fn capabilities(&self, version: Option<&ToolVersion>) -> RunnerCapabilities {
        let verified =
            version.is_some_and(|version| VERIFIED_VERSIONS.contains(&version.version.as_str()));
        RunnerCapabilities {
            full_run: true,
            // The meaning of positional arguments is only known for verified
            // versions. Any other version runs the whole suite.
            file_selection: verified,
            file_attribution: true,
            structured_report: true,
            worker_control: false,
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
        reject_reserved("bun", suite.command(), &RESERVED)?;
        let report_path =
            artifacts
                .report_path
                .to_str()
                .ok_or_else(|| AdapterError::Configuration {
                    message: "the report path is not valid UTF-8".to_owned(),
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
                        // `./` makes Bun treat the argument as one file, not as a filter.
                        argv.extend(relative.into_iter().map(|path| format!("./{path}")));
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
        argv.push("--reporter=junit".to_owned());
        argv.push(format!("--reporter-outfile={report_path}"));

        Ok(PreparedInvocation {
            suite_id: suite.id().clone(),
            task_id: TaskId::for_suite(suite.id()),
            requested_selection: selection.clone(),
            effective_selection,
            widened_because,
            argv,
            // Makes Bun refuse `test.only` and keeps it from writing snapshots.
            env: BTreeMap::from([("CI".to_owned(), "true".to_owned())]),
            cwd: suite.cwd().clone(),
            workspace_root: environment.workspace.execution_root.clone(),
            expected_files,
            report_format: ReportFormat::BunJunitXml,
            report_path: Some(artifacts.report_path.clone()),
            additional_files: AdditionalFiles::Unexpected,
        })
    }

    fn normalize(
        &self,
        invocation: &PreparedInvocation,
        process: &ProcessResult,
        reports: &ReportBundle,
    ) -> Result<SuiteResult, AdapterError> {
        let report = match &reports.content {
            ReportContent::Bytes(bytes) => match bun_junit::parse(bytes, &invocation.cwd) {
                Ok(parsed) => ReportOutcome::Parsed(parsed),
                Err(problem) => ReportOutcome::invalid(problem),
            },
            other => ReportOutcome::content_problem(other)
                .unwrap_or_else(|| ReportOutcome::invalid("the report has no content")),
        };
        Ok(assemble_suite_result(invocation, process, report))
    }
}

#[cfg(test)]
mod tests {
    use testry_application::ports::execution::FileAttribution;
    use testry_application::results::anomaly;
    use testry_domain::results::TaskOutcome;
    use testry_domain::selection::OmitReason;

    use super::*;
    use crate::runners::test_support::{
        bundle, descriptor, files, locations, path, process, suite,
    };

    const MIXED: &[u8] =
        include_bytes!("../../../../tests/fixtures/runner-reports/bun-1.4.4/mixed.xml");
    const PASSING: &[u8] =
        include_bytes!("../../../../tests/fixtures/runner-reports/bun-1.4.4/passing.xml");
    const IMPORT_ERROR: &[u8] =
        include_bytes!("../../../../tests/fixtures/runner-reports/bun-1.4.4/import-error.xml");

    fn prepare(suite: &SuiteDefinition, selection: &Selection) -> PreparedInvocation {
        BunAdapter
            .prepare(suite, selection, &descriptor(), &locations("report.xml"))
            .unwrap()
    }

    fn version(text: &str) -> ToolVersion {
        ToolVersion {
            tool: "bun".to_owned(),
            version: text.to_owned(),
        }
    }

    #[test]
    fn a_whole_suite_runs_without_file_arguments() {
        let suite = suite(RunnerKind::Bun, "apps/api", &["bun", "test"]);
        let invocation = prepare(&suite, &Selection::All);
        assert_eq!(
            invocation.argv,
            [
                "bun",
                "test",
                "--reporter=junit",
                "--reporter-outfile=/workspace/.testry/runs/r/tasks/suite/report.xml",
            ]
        );
        assert_eq!(invocation.effective_selection, Selection::All);
        assert_eq!(invocation.expected_files, None);
        assert_eq!(invocation.cwd.as_str(), "apps/api");
        assert_eq!(invocation.env.get("CI").map(String::as_str), Some("true"));
        assert_eq!(invocation.report_format, ReportFormat::BunJunitXml);
    }

    #[test]
    fn selected_files_are_passed_as_exact_paths() {
        let suite = suite(RunnerKind::Bun, "apps/api", &["bun", "test"]);
        let selection = files(&[
            "apps/api/src/a.test.ts",
            "apps/api/with space/日本語 [x].test.ts",
            "apps/api/-dash.test.ts",
        ]);
        let invocation = prepare(&suite, &selection);
        assert_eq!(
            invocation.argv.get(2..5).unwrap(),
            [
                "./-dash.test.ts",
                "./src/a.test.ts",
                "./with space/日本語 [x].test.ts"
            ]
        );
        assert_eq!(invocation.effective_selection, selection);
        assert_eq!(invocation.expected_files.as_ref().unwrap().len(), 3);
        assert_eq!(invocation.additional_files, AdditionalFiles::Unexpected);
        assert!(invocation.widened_because.is_none());
    }

    #[test]
    fn a_file_outside_the_suite_directory_widens_to_the_whole_suite() {
        let suite = suite(RunnerKind::Bun, "apps/api", &["bun", "test"]);
        let invocation = prepare(
            &suite,
            &files(&["apps/api/a.test.ts", "packages/x/b.test.ts"]),
        );
        assert_eq!(invocation.effective_selection, Selection::All);
        assert!(invocation.widened_because.is_some());
        assert_eq!(
            invocation.argv.len(),
            4,
            "no file argument may remain: {:?}",
            invocation.argv
        );
        assert!(
            invocation
                .effective_selection
                .covers(&invocation.requested_selection)
        );
    }

    #[test]
    fn an_omitted_suite_cannot_be_prepared() {
        let suite = suite(RunnerKind::Bun, ".", &["bun", "test"]);
        let omitted = Selection::Omit {
            reason: OmitReason::NoChanges,
        };
        assert!(
            BunAdapter
                .prepare(&suite, &omitted, &descriptor(), &locations("report.xml"))
                .is_err()
        );
    }

    #[test]
    fn commands_that_narrow_or_own_the_report_are_rejected() {
        for flags in [
            &["--only"][..],
            &["-t", "name"],
            &["--test-name-pattern=x"],
            &["--shard=1/3"],
            &["--changed"],
            &["--pass-with-no-tests"],
            &["--bail"],
            &["--reporter=dots"],
            &["--reporter-outfile=x.xml"],
            &["-u"],
            &["--retry=2"],
            &["--parallel=4"],
        ] {
            let mut command = vec!["bun", "test"];
            command.extend_from_slice(flags);
            let suite = suite(RunnerKind::Bun, ".", &command);
            let error = BunAdapter
                .prepare(
                    &suite,
                    &Selection::All,
                    &descriptor(),
                    &locations("report.xml"),
                )
                .unwrap_err();
            assert!(
                matches!(error, AdapterError::Configuration { .. }),
                "{flags:?}"
            );
        }
        // Flags that do not narrow the run stay available.
        let suite = suite(
            RunnerKind::Bun,
            ".",
            &["bun", "test", "--timeout=10000", "--coverage"],
        );
        assert!(
            BunAdapter
                .prepare(
                    &suite,
                    &Selection::All,
                    &descriptor(),
                    &locations("report.xml")
                )
                .is_ok()
        );
    }

    /// P05: only verified versions get a file selection.
    #[test]
    fn p05_file_selection_needs_a_verified_version() {
        let verified = BunAdapter.capabilities(Some(&version("1.4.4")));
        assert!(verified.file_selection && verified.version_verified && verified.full_run);
        assert!(!verified.worker_control);

        for unverified in [Some(version("1.4.5")), Some(version("2.0.0")), None] {
            let capabilities = BunAdapter.capabilities(unverified.as_ref());
            assert!(!capabilities.file_selection, "{unverified:?}");
            assert!(!capabilities.version_verified);
            assert!(capabilities.full_run);
        }
    }

    #[test]
    fn the_version_is_read_from_the_configured_program() {
        let suite = suite(RunnerKind::Bun, ".", &["/usr/local/bin/bun", "test"]);
        assert_eq!(
            BunAdapter.version_command(&suite).unwrap(),
            ["/usr/local/bin/bun", "--version"]
        );
        assert_eq!(BunAdapter.parse_version("1.4.4\n"), Some(version("1.4.4")));
        assert_eq!(BunAdapter.parse_version("command not found"), None);
    }

    #[test]
    fn a_real_passing_report_normalizes_to_passed() {
        let suite = suite(RunnerKind::Bun, ".", &["bun", "test"]);
        let invocation = prepare(
            &suite,
            &files(&["src/math.test.ts", "src/with space/日本語 [x].test.ts"]),
        );
        let result = BunAdapter
            .normalize(
                &invocation,
                &process(0),
                &bundle(ReportFormat::BunJunitXml, PASSING),
            )
            .unwrap();
        assert_eq!(result.outcome, TaskOutcome::Passed);
        assert_eq!(result.counts.unwrap().passed, 3);
        assert!(
            matches!(&result.executed_files, FileAttribution::Known(files) if files.len() == 2)
        );
        assert!(result.anomalies.is_empty());
    }

    #[test]
    fn a_real_mixed_report_normalizes_to_failed() {
        let suite = suite(RunnerKind::Bun, ".", &["bun", "test"]);
        let invocation = prepare(&suite, &Selection::All);
        let result = BunAdapter
            .normalize(
                &invocation,
                &process(1),
                &bundle(ReportFormat::BunJunitXml, MIXED),
            )
            .unwrap();
        assert_eq!(result.outcome, TaskOutcome::Failed);
        let counts = result.counts.unwrap();
        assert_eq!((counts.passed, counts.failed, counts.skipped), (5, 1, 3));
    }

    /// P09: Bun leaves a file with an import error out of its report.
    #[test]
    fn p09_a_requested_file_missing_from_the_real_report_is_not_a_pass() {
        let suite = suite(RunnerKind::Bun, ".", &["bun", "test"]);
        let invocation = prepare(&suite, &files(&["src/math.test.ts", "src/throws.test.ts"]));
        let report = bundle(ReportFormat::BunJunitXml, IMPORT_ERROR);

        let failed = BunAdapter
            .normalize(&invocation, &process(1), &report)
            .unwrap();
        assert_eq!(failed.outcome, TaskOutcome::Failed);
        assert!(
            failed
                .anomalies
                .iter()
                .any(|a| a.code == anomaly::EXPECTED_FILES_MISSING
                    && a.detail.contains("src/throws.test.ts"))
        );

        // Even if the runner had exited with 0, the missing file is not a pass.
        let silent = BunAdapter
            .normalize(&invocation, &process(0), &report)
            .unwrap();
        assert_eq!(silent.outcome, TaskOutcome::InfrastructureError);
    }

    /// P08 / P14: a missing or broken report is an infrastructure error.
    #[test]
    fn p08_p14_missing_and_broken_reports() {
        let suite = suite(RunnerKind::Bun, ".", &["bun", "test"]);
        let invocation = prepare(&suite, &Selection::All);
        let missing = ReportBundle {
            format: ReportFormat::BunJunitXml,
            content: ReportContent::Missing,
        };
        let result = BunAdapter
            .normalize(&invocation, &process(0), &missing)
            .unwrap();
        assert_eq!(result.outcome, TaskOutcome::InfrastructureError);
        assert_eq!(result.anomalies[0].code, anomaly::REPORT_MISSING);

        for broken in [
            &b""[..],
            b"<testsuites>",
            b"<!DOCTYPE x [<!ENTITY a \"b\">]><testsuites/>",
        ] {
            let result = BunAdapter
                .normalize(
                    &invocation,
                    &process(0),
                    &bundle(ReportFormat::BunJunitXml, broken),
                )
                .unwrap();
            assert_eq!(result.outcome, TaskOutcome::InfrastructureError);
            assert_eq!(result.anomalies[0].code, anomaly::REPORT_INVALID);
        }
        let _ = path("unused");
    }
}
