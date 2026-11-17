//! Parser for the JSON report of Vitest (verified with Vitest 5.1.0).
//!
//! Shape of the report:
//!
//! ```text
//! { "success": false,
//!   "testResults": [
//!     { "name": "/abs/path/src/a.test.ts", "status": "passed" | "failed", "message": "",
//!       "assertionResults": [
//!         { "ancestorTitles": ["math"], "title": "adds",
//!           "status": "passed" | "failed" | "skipped" | "todo",
//!           "failureMessages": [] } ] } ] }
//! ```
//!
//! Observed facts the parser relies on:
//! - file names are absolute paths in the environment the runner ran in;
//! - a test that failed and then passed on a retry has status `passed` and a
//!   non-empty `failureMessages`; it is reported as flaky, never as passed;
//! - a file that cannot be loaded (syntax error, error at import, no tests)
//!   appears with status `failed` and no assertion results.
//!
//! An unknown status is an error. It is never guessed.

use std::path::Path;

use serde::Deserialize;
use testry_application::results::ParsedReport;
use testry_domain::paths::RepoPath;
use testry_domain::results::{CaseResult, CaseStatus};

const MAX_FAILURE_CHARS: usize = 2000;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Report {
    test_results: Vec<FileResult>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileResult {
    name: String,
    status: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    assertion_results: Vec<Assertion>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Assertion {
    #[serde(default)]
    ancestor_titles: Vec<String>,
    title: String,
    status: String,
    #[serde(default)]
    failure_messages: Vec<String>,
}

fn bounded(message: &str) -> String {
    message.chars().take(MAX_FAILURE_CHARS).collect()
}

fn repo_path(workspace_root: &Path, name: &str) -> Result<RepoPath, String> {
    let relative = Path::new(name)
        .strip_prefix(workspace_root)
        .map_err(|_| format!("the report names a file outside the workspace: {name:?}"))?;
    let relative = relative
        .to_str()
        .ok_or_else(|| "the report names a file whose path is not valid UTF-8".to_owned())?;
    RepoPath::try_new(relative)
        .map_err(|error| format!("the report names an unusable path {name:?}: {error}"))
}

/// Parses a report. `workspace_root` is the workspace as the runner saw it.
pub fn parse(bytes: &[u8], workspace_root: &Path) -> Result<ParsedReport, String> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Err("the report is empty".to_owned());
    }
    let report: Report = serde_json::from_slice(bytes)
        .map_err(|error| format!("the report is not a Vitest JSON report: {error}"))?;

    let mut parsed = ParsedReport::default();
    for file_result in report.test_results {
        let file = repo_path(workspace_root, &file_result.name)?;
        parsed.files.insert(file.clone());

        let file_failed = match file_result.status.as_str() {
            "passed" => false,
            "failed" => true,
            other => return Err(format!("unknown file status {other:?} for {file}")),
        };
        let mut reported_failure = false;
        for assertion in file_result.assertion_results {
            let status = match assertion.status.as_str() {
                "passed" if assertion.failure_messages.is_empty() => CaseStatus::Passed,
                // Failed at least once, passed on a retry.
                "passed" => CaseStatus::Flaky,
                "failed" => CaseStatus::Failed,
                "skipped" | "pending" => CaseStatus::Skipped,
                "todo" => CaseStatus::Todo,
                other => return Err(format!("unknown test status {other:?} in {file}")),
            };
            reported_failure |= status == CaseStatus::Failed;
            let mut title_path = assertion.ancestor_titles;
            title_path.push(assertion.title);
            parsed.cases.push(CaseResult {
                project: None,
                file: Some(file.clone()),
                title_path,
                status,
                failure: assertion
                    .failure_messages
                    .first()
                    .map(|message| bounded(message)),
            });
        }
        if file_failed && !reported_failure {
            // The file itself failed: it could not be loaded, or it has no test.
            let message = if file_result.message.is_empty() {
                "the test file failed without a failing test (it may contain no test)".to_owned()
            } else {
                bounded(&file_result.message)
            };
            parsed.cases.push(CaseResult {
                project: None,
                file: Some(file),
                title_path: Vec::new(),
                status: CaseStatus::Failed,
                failure: Some(message),
            });
        }
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use testry_domain::results::TestCounts;

    use super::*;

    const MIXED: &[u8] =
        include_bytes!("../../../../tests/fixtures/runner-reports/vitest-5.1.0/mixed.json");
    const PASSING: &[u8] =
        include_bytes!("../../../../tests/fixtures/runner-reports/vitest-5.1.0/passing.json");
    const FLAKY: &[u8] =
        include_bytes!("../../../../tests/fixtures/runner-reports/vitest-5.1.0/flaky.json");
    const IMPORT_ERROR: &[u8] =
        include_bytes!("../../../../tests/fixtures/runner-reports/vitest-5.1.0/import-error.json");
    const NO_MATCH: &[u8] =
        include_bytes!("../../../../tests/fixtures/runner-reports/vitest-5.1.0/no-match.json");

    /// The directory the fixtures were recorded in.
    fn root() -> &'static Path {
        Path::new("/tmp/fx/vitest")
    }

    fn files(report: &ParsedReport) -> Vec<&str> {
        report.files.iter().map(RepoPath::as_str).collect()
    }

    fn case<'a>(report: &'a ParsedReport, title: &[&str]) -> &'a CaseResult {
        report
            .cases
            .iter()
            .find(|case| case.title_path == title)
            .unwrap_or_else(|| panic!("no case {title:?} in {:?}", report.cases))
    }

    #[test]
    fn parses_a_real_mixed_report() {
        let report = parse(MIXED, root()).unwrap();
        assert_eq!(
            files(&report),
            [
                "src/failing.test.ts",
                "src/flaky.test.ts",
                "src/math.test.ts",
                "src/skips.test.ts",
                "src/with space/日本語 [x].test.ts",
            ]
        );
        assert_eq!(
            TestCounts::from_cases(&report.cases),
            TestCounts {
                passed: 5,
                failed: 1,
                skipped: 3,
                flaky: 1
            }
        );
        assert_eq!(
            case(&report, &["math", "add", "adds"]).status,
            CaseStatus::Passed
        );
        assert_eq!(
            case(&report, &["group", "nested skip"]).status,
            CaseStatus::Skipped
        );
        assert_eq!(case(&report, &["todo one"]).status, CaseStatus::Todo);
        let failed = case(&report, &["fails"]);
        assert_eq!(failed.status, CaseStatus::Failed);
        assert_eq!(
            failed.file.as_ref().unwrap().as_str(),
            "src/failing.test.ts"
        );
        assert!(
            failed
                .failure
                .as_ref()
                .unwrap()
                .contains("expected 1 to be 2")
        );
    }

    /// P10: a pass after a retry is flaky, with the first failure kept.
    #[test]
    fn p10_a_pass_after_a_retry_is_flaky() {
        let report = parse(FLAKY, root()).unwrap();
        assert_eq!(report.cases.len(), 1);
        let flaky = case(&report, &["flaky"]);
        assert_eq!(flaky.status, CaseStatus::Flaky);
        assert!(
            flaky
                .failure
                .as_ref()
                .unwrap()
                .contains("first attempt fails")
        );
        assert_eq!(TestCounts::from_cases(&report.cases).passed, 0);
    }

    #[test]
    fn a_file_that_cannot_be_loaded_is_a_failure_of_that_file() {
        let report = parse(IMPORT_ERROR, root()).unwrap();
        assert_eq!(files(&report), ["src/math.test.ts", "src/throws.test.ts"]);
        let file_failure = report
            .cases
            .iter()
            .find(|case| case.title_path.is_empty())
            .expect("a file-level failure");
        assert_eq!(file_failure.status, CaseStatus::Failed);
        assert_eq!(
            file_failure.file.as_ref().unwrap().as_str(),
            "src/throws.test.ts"
        );
        assert_eq!(file_failure.failure.as_deref(), Some("boom"));
        assert_eq!(TestCounts::from_cases(&report.cases).failed, 1);
    }

    #[test]
    fn a_real_report_without_matches_is_empty() {
        let report = parse(NO_MATCH, root()).unwrap();
        assert!(report.files.is_empty() && report.cases.is_empty());
    }

    #[test]
    fn real_paths_with_spaces_and_japanese_are_mapped() {
        let report = parse(PASSING, root()).unwrap();
        assert_eq!(
            files(&report),
            ["src/math.test.ts", "src/with space/日本語 [x].test.ts"]
        );
    }

    #[test]
    fn files_outside_the_workspace_are_rejected() {
        let error = parse(PASSING, Path::new("/somewhere/else")).unwrap_err();
        assert!(error.contains("outside the workspace"), "{error}");
        let escaping = br#"{"testResults":[{"name":"/w/../etc/passwd","status":"passed","assertionResults":[]}]}"#;
        assert!(parse(escaping, Path::new("/w")).is_err());
    }

    /// P14: empty, broken and unexpected reports fail safely.
    #[test]
    fn p14_unusable_reports_are_rejected() {
        let rejected: &[(&str, &[u8])] = &[
            ("empty", b""),
            ("whitespace", b" \n"),
            ("truncated", b"{\"testResults\":[{\"name\":\"/w/a.test.ts\""),
            ("not json", b"Test Files  1 passed"),
            ("other document", b"{\"suites\":[]}"),
            ("array", b"[]"),
            ("null", b"null"),
            (
                "unknown file status",
                br#"{"testResults":[{"name":"/w/a.test.ts","status":"maybe","assertionResults":[]}]}"#,
            ),
            (
                "unknown test status",
                br#"{"testResults":[{"name":"/w/a.test.ts","status":"passed","assertionResults":[{"title":"t","status":"probably"}]}]}"#,
            ),
        ];
        for (name, bytes) in rejected {
            assert!(
                parse(bytes, Path::new("/w")).is_err(),
                "{name} must be rejected"
            );
        }
        // Deep nesting hits the recursion limit of the JSON parser.
        let deep = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
        assert!(parse(deep.as_bytes(), Path::new("/w")).is_err());
    }

    #[test]
    fn a_failed_file_with_a_failing_test_is_not_counted_twice() {
        let json = br#"{"testResults":[{"name":"/w/a.test.ts","status":"failed","message":"",
            "assertionResults":[{"ancestorTitles":[],"title":"t","status":"failed","failureMessages":["x"]}]}]}"#;
        let report = parse(json, Path::new("/w")).unwrap();
        assert_eq!(TestCounts::from_cases(&report.cases).failed, 1);
    }
}
