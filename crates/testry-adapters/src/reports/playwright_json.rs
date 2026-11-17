//! Parser for the JSON report of Playwright Test (verified with 1.65.1).
//!
//! Shape of the report:
//!
//! ```text
//! { "config": { "rootDir": "/abs/tests/e2e", "shard": null, "grepInvert": null,
//!               "maxFailures": 0, "forbidOnly": true, ... },
//!   "suites": [                                   // one per test file
//!     { "title": "login.spec.ts", "file": "login.spec.ts",
//!       "specs":  [ { "title": "signs in", "file": "login.spec.ts",
//!                     "tests": [ { "projectName": "chromium",
//!                                  "expectedStatus": "passed" | "failed" | "skipped",
//!                                  "status": "expected" | "unexpected" | "flaky" | "skipped",
//!                                  "annotations": [ { "type": "fixme" } ],
//!                                  "results": [ { "status": "failed", "retry": 0,
//!                                                 "error": { "message": "..." } } ] } ] } ],
//!       "suites": [ { "title": "a describe block", "specs": [...], "suites": [...] } ] } ],
//!   "errors": [ { "message": "...", "location": { "file": "/abs/broken.spec.ts" } } ],
//!   "stats":  { "expected": 4, "unexpected": 1, "flaky": 1, "skipped": 2 } }
//! ```
//!
//! Observed facts the parser relies on (`tests/runner_contract.rs`):
//! - file names are relative to `config.rootDir`, which is absolute;
//! - a spec has one test per project, so the project is part of the identity
//!   of a case;
//! - a test that failed and passed on a retry has status `flaky`; a test
//!   marked with `test.fail()` that failed has status `expected` and
//!   `expectedStatus: "failed"`;
//! - tests of a project whose dependency failed are `skipped` without any
//!   attempt;
//! - a file that cannot be loaded, a focused test under `--forbid-only` and
//!   "no tests found" are entries of the top-level `errors`, without suites.
//!
//! An unknown status and totals that do not match the tests are errors. They
//! are never guessed.

use std::path::Path;

use serde::Deserialize;
use testry_application::results::ParsedReport;
use testry_domain::paths::RepoPath;
use testry_domain::results::{CaseResult, CaseStatus};

const MAX_FAILURE_CHARS: usize = 2000;

#[derive(Deserialize)]
struct Report {
    config: Config,
    #[serde(default)]
    suites: Vec<Suite>,
    #[serde(default)]
    errors: Vec<ReportError>,
    stats: Stats,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    root_dir: String,
    #[serde(default)]
    shard: Option<serde_json::Value>,
    #[serde(default)]
    grep_invert: Option<serde_json::Value>,
    #[serde(default)]
    max_failures: u64,
    #[serde(default)]
    forbid_only: bool,
}

#[derive(Deserialize)]
struct Suite {
    #[serde(default)]
    title: String,
    #[serde(default)]
    specs: Vec<Spec>,
    #[serde(default)]
    suites: Vec<Suite>,
}

#[derive(Deserialize)]
struct Spec {
    title: String,
    file: String,
    #[serde(default)]
    tests: Vec<Test>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Test {
    #[serde(default)]
    project_name: String,
    expected_status: String,
    status: String,
    #[serde(default)]
    annotations: Vec<Annotation>,
    #[serde(default)]
    results: Vec<Attempt>,
}

#[derive(Deserialize)]
struct Annotation {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct Attempt {
    #[serde(default)]
    error: Option<ErrorBody>,
}

#[derive(Deserialize)]
struct ErrorBody {
    #[serde(default)]
    message: Option<String>,
}

#[derive(Deserialize)]
struct ReportError {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    location: Option<Location>,
}

#[derive(Deserialize)]
struct Location {
    file: String,
}

#[derive(Deserialize)]
struct Stats {
    expected: u64,
    unexpected: u64,
    flaky: u64,
    skipped: u64,
}

/// A parsed report together with what it says about the configuration the
/// runner really used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaywrightReport {
    pub parsed: ParsedReport,
    /// Settings of the resolved runner configuration that make the run cover
    /// less than the whole suite. Empty when none was found.
    pub narrowing: Vec<String>,
    /// Whether focused tests were forbidden in this run.
    pub forbid_only: bool,
}

/// Removes ANSI escape sequences; Playwright colours its error messages.
fn strip_ansi(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '\u{1b}' {
            plain.push(character);
            continue;
        }
        if characters.peek() == Some(&'[') {
            characters.next();
            // Parameter and intermediate bytes, then one final byte.
            for parameter in characters.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&parameter) {
                    break;
                }
            }
        }
    }
    plain
}

fn bounded(message: &str) -> String {
    strip_ansi(message)
        .chars()
        .take(MAX_FAILURE_CHARS)
        .collect()
}

fn repo_path(workspace_root: &Path, absolute: &Path) -> Result<RepoPath, String> {
    let relative = absolute.strip_prefix(workspace_root).map_err(|_| {
        format!(
            "the report names a file outside the workspace: {:?}",
            absolute.display().to_string()
        )
    })?;
    let relative = relative
        .to_str()
        .ok_or_else(|| "the report names a file whose path is not valid UTF-8".to_owned())?;
    RepoPath::try_new(relative).map_err(|error| {
        format!(
            "the report names an unusable path {:?}: {error}",
            absolute.display().to_string()
        )
    })
}

#[derive(Default)]
struct Totals {
    expected: u64,
    unexpected: u64,
    flaky: u64,
    skipped: u64,
}

struct Walk<'a> {
    workspace_root: &'a Path,
    root_dir: &'a Path,
    parsed: ParsedReport,
    totals: Totals,
}

impl Walk<'_> {
    fn suite(
        &mut self,
        suite: Suite,
        titles: &mut Vec<String>,
        depth: usize,
    ) -> Result<(), String> {
        // The top level is the file; only nested suites are describe blocks.
        let named = depth > 0 && !suite.title.is_empty();
        if named {
            titles.push(suite.title);
        }
        for spec in suite.specs {
            self.spec(spec, titles)?;
        }
        for child in suite.suites {
            self.suite(child, titles, depth + 1)?;
        }
        if named {
            titles.pop();
        }
        Ok(())
    }

    fn spec(&mut self, spec: Spec, titles: &[String]) -> Result<(), String> {
        let file = repo_path(self.workspace_root, &self.root_dir.join(&spec.file))?;
        self.parsed.files.insert(file.clone());
        let mut title_path = titles.to_vec();
        title_path.push(spec.title);

        for test in spec.tests {
            let first_error = test
                .results
                .iter()
                .find_map(|attempt| attempt.error.as_ref())
                .and_then(|error| error.message.as_deref())
                .map(bounded);
            let (status, failure) = match (test.status.as_str(), test.expected_status.as_str()) {
                ("expected", "passed") => {
                    self.totals.expected += 1;
                    (CaseStatus::Passed, None)
                }
                ("expected", "failed") => {
                    self.totals.expected += 1;
                    (CaseStatus::ExpectedFailure, first_error)
                }
                ("unexpected", _) => {
                    self.totals.unexpected += 1;
                    let message = first_error.unwrap_or_else(|| {
                        "the test passed although it is declared as failing, or it failed without a message"
                            .to_owned()
                    });
                    (CaseStatus::Failed, Some(message))
                }
                ("flaky", _) => {
                    self.totals.flaky += 1;
                    (CaseStatus::Flaky, first_error)
                }
                ("skipped", _) => {
                    self.totals.skipped += 1;
                    let fixme = test
                        .annotations
                        .iter()
                        .any(|annotation| annotation.kind == "fixme");
                    let failure = test.results.is_empty().then(|| {
                        "the test did not run (a project it depends on failed, or the run stopped early)"
                            .to_owned()
                    });
                    let status = if fixme {
                        CaseStatus::Todo
                    } else {
                        CaseStatus::Skipped
                    };
                    (status, failure)
                }
                (status, expected) => {
                    return Err(format!(
                        "unknown test status {status:?} (expected status {expected:?}) in {file}"
                    ));
                }
            };
            self.parsed.cases.push(CaseResult {
                project: (!test.project_name.is_empty()).then_some(test.project_name),
                file: Some(file.clone()),
                title_path: title_path.clone(),
                status,
                failure,
            });
        }
        Ok(())
    }
}

/// Parses a report. `workspace_root` is the workspace as the runner saw it.
pub fn parse(bytes: &[u8], workspace_root: &Path) -> Result<PlaywrightReport, String> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Err("the report is empty".to_owned());
    }
    let report: Report = serde_json::from_slice(bytes)
        .map_err(|error| format!("the report is not a Playwright JSON report: {error}"))?;
    let root_dir = Path::new(&report.config.root_dir);
    if !root_dir.is_absolute() {
        return Err(format!(
            "the report has a relative rootDir: {:?}",
            report.config.root_dir
        ));
    }

    let mut walk = Walk {
        workspace_root,
        root_dir,
        parsed: ParsedReport::default(),
        totals: Totals::default(),
    };
    let mut titles = Vec::new();
    for suite in report.suites {
        walk.suite(suite, &mut titles, 0)?;
    }
    let Walk {
        mut parsed, totals, ..
    } = walk;

    let stats = &report.stats;
    if (
        totals.expected,
        totals.unexpected,
        totals.flaky,
        totals.skipped,
    ) != (stats.expected, stats.unexpected, stats.flaky, stats.skipped)
    {
        return Err(format!(
            "the totals of the report ({} expected, {} unexpected, {} flaky, {} skipped) do not \
             match its tests ({} expected, {} unexpected, {} flaky, {} skipped)",
            stats.expected,
            stats.unexpected,
            stats.flaky,
            stats.skipped,
            totals.expected,
            totals.unexpected,
            totals.flaky,
            totals.skipped
        ));
    }

    // A problem outside of any test: a file that cannot be loaded, a focused
    // test that is forbidden, no test at all. Each one is a failure.
    for error in report.errors {
        let file = error
            .location
            .as_ref()
            .and_then(|location| repo_path(workspace_root, Path::new(&location.file)).ok());
        if let Some(file) = &file {
            parsed.files.insert(file.clone());
        }
        parsed.cases.push(CaseResult {
            project: None,
            file,
            title_path: Vec::new(),
            status: CaseStatus::Failed,
            failure: Some(bounded(
                error
                    .message
                    .as_deref()
                    .unwrap_or("the runner reported an error without a message"),
            )),
        });
    }

    let mut narrowing = Vec::new();
    if report.config.shard.is_some() {
        narrowing.push("the configuration runs one shard of the suite (`shard`)".to_owned());
    }
    if report.config.grep_invert.is_some() {
        narrowing.push("the configuration excludes tests by title (`grepInvert`)".to_owned());
    }
    if report.config.max_failures > 0 {
        narrowing.push(format!(
            "the configuration stops after {} failure(s) (`maxFailures`), so later tests may not run",
            report.config.max_failures
        ));
    }

    Ok(PlaywrightReport {
        parsed,
        narrowing,
        forbid_only: report.config.forbid_only,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIXED: &[u8] =
        include_bytes!("../../../../tests/fixtures/runner-reports/playwright-1.65.1/mixed.json");
    const PASSING: &[u8] =
        include_bytes!("../../../../tests/fixtures/runner-reports/playwright-1.65.1/passing.json");
    const PROJECTS: &[u8] =
        include_bytes!("../../../../tests/fixtures/runner-reports/playwright-1.65.1/projects.json");
    const SETUP_FAILED: &[u8] = include_bytes!(
        "../../../../tests/fixtures/runner-reports/playwright-1.65.1/setup-failed.json"
    );
    const LOAD_ERROR: &[u8] = include_bytes!(
        "../../../../tests/fixtures/runner-reports/playwright-1.65.1/load-error.json"
    );
    const FORBID_ONLY: &[u8] = include_bytes!(
        "../../../../tests/fixtures/runner-reports/playwright-1.65.1/forbid-only.json"
    );
    const NO_TESTS: &[u8] =
        include_bytes!("../../../../tests/fixtures/runner-reports/playwright-1.65.1/no-tests.json");
    const NARROWED: &[u8] =
        include_bytes!("../../../../tests/fixtures/runner-reports/playwright-1.65.1/narrowed.json");
    const WITHOUT_FORBID_ONLY: &[u8] = include_bytes!(
        "../../../../tests/fixtures/runner-reports/playwright-1.65.1/without-forbid-only.json"
    );

    /// Where the fixtures were recorded.
    fn recorded(bytes: &[u8]) -> PlaywrightReport {
        parse(bytes, Path::new("/tmp/fx/playwright")).unwrap()
    }

    fn case<'a>(report: &'a PlaywrightReport, title: &[&str]) -> &'a CaseResult {
        report
            .parsed
            .cases
            .iter()
            .find(|case| case.title_path == title)
            .unwrap_or_else(|| panic!("no case {title:?} in {:?}", report.parsed.cases))
    }

    fn files(report: &PlaywrightReport) -> Vec<&str> {
        report.parsed.files.iter().map(RepoPath::as_str).collect()
    }

    #[test]
    fn a_real_report_keeps_every_status_apart() {
        let report = recorded(MIXED);
        assert_eq!(
            files(&report),
            [
                "tests/e2e/auth/login.spec.ts",
                "tests/e2e/global.setup.ts",
                "tests/e2e/mixed.spec.ts"
            ],
            "files are relative to the repository, not to the test directory"
        );
        assert_eq!(report.parsed.cases.len(), 8);

        let failed = case(&report, &["fails"]);
        assert_eq!(failed.status, CaseStatus::Failed);
        assert_eq!(
            failed.file.as_ref().unwrap().as_str(),
            "tests/e2e/mixed.spec.ts"
        );
        let message = failed.failure.as_deref().unwrap();
        assert!(message.contains("expect("), "{message}");
        assert!(
            !message.contains('\u{1b}'),
            "colours are removed: {message:?}"
        );

        assert_eq!(case(&report, &["skipped"]).status, CaseStatus::Skipped);
        assert_eq!(case(&report, &["skipped"]).failure, None);
        assert_eq!(case(&report, &["fixme"]).status, CaseStatus::Todo);
        // P10: failed once, passed on the retry.
        let flaky = case(&report, &["flaky"]);
        assert_eq!(flaky.status, CaseStatus::Flaky);
        assert!(flaky.failure.as_deref().unwrap().contains("first attempt"));
        // Declared as failing and it failed: not a pass.
        assert_eq!(
            case(&report, &["expected failure"]).status,
            CaseStatus::ExpectedFailure
        );

        // Describe blocks are the title path; the file is not part of it.
        assert_eq!(
            case(&report, &["login page", "renders"]).status,
            CaseStatus::Passed
        );
        assert_eq!(
            case(&report, &["login page", "session", "setup ran first"]).status,
            CaseStatus::Passed
        );
        // The setup project is part of the report under its own name.
        let setup = case(&report, &["prepare"]);
        assert_eq!(setup.project.as_deref(), Some("setup"));
        assert_eq!(failed.project.as_deref(), Some("chromium"));

        assert!(report.narrowing.is_empty());
        assert!(report.forbid_only);
    }

    #[test]
    fn a_passing_report_has_only_passed_cases() {
        let report = recorded(PASSING);
        assert_eq!(report.parsed.cases.len(), 3);
        assert!(
            report
                .parsed
                .cases
                .iter()
                .all(|case| case.status == CaseStatus::Passed)
        );
    }

    /// P11: one spec that runs in two projects is two cases.
    #[test]
    fn p11_the_project_is_part_of_the_identity_of_a_case() {
        let report = recorded(PROJECTS);
        let renders: Vec<Option<&str>> = report
            .parsed
            .cases
            .iter()
            .filter(|case| case.title_path == ["login page", "renders"])
            .map(|case| case.project.as_deref())
            .collect();
        assert_eq!(renders, [Some("chromium"), Some("mobile")]);
        assert_eq!(report.parsed.cases.len(), 5);
    }

    /// R18: when a setup project fails, the tests that depend on it did not
    /// run. They are skipped without an attempt, never passed.
    #[test]
    fn r18_tests_behind_a_failed_setup_project_did_not_run() {
        let report = recorded(SETUP_FAILED);
        let setup = case(&report, &["prepare"]);
        assert_eq!(setup.status, CaseStatus::Failed);
        assert!(setup.failure.as_deref().unwrap().contains("setup broke"));

        let dependent = case(&report, &["login page", "renders"]);
        assert_eq!(dependent.status, CaseStatus::Skipped);
        assert!(
            dependent
                .failure
                .as_deref()
                .unwrap()
                .contains("did not run"),
            "{dependent:?}"
        );
        assert!(
            !report
                .parsed
                .cases
                .iter()
                .any(|case| case.status == CaseStatus::Passed)
        );
    }

    #[test]
    fn problems_outside_of_tests_are_failures() {
        let load_error = recorded(LOAD_ERROR);
        assert_eq!(load_error.parsed.cases.len(), 1);
        let failure = &load_error.parsed.cases[0];
        assert_eq!(failure.status, CaseStatus::Failed);
        assert!(failure.title_path.is_empty());
        assert_eq!(
            failure.file.as_ref().unwrap().as_str(),
            "tests/e2e/broken.spec.ts"
        );
        assert_eq!(files(&load_error), ["tests/e2e/broken.spec.ts"]);
        assert!(failure.failure.as_deref().unwrap().contains("SyntaxError"));

        let focused = recorded(FORBID_ONLY);
        assert_eq!(focused.parsed.cases.len(), 1);
        assert_eq!(focused.parsed.cases[0].status, CaseStatus::Failed);
        assert!(
            focused.parsed.cases[0]
                .failure
                .as_deref()
                .unwrap()
                .contains(".only"),
            "{focused:?}"
        );

        // P08: no test at all is a failure without any file.
        let none = recorded(NO_TESTS);
        assert!(none.parsed.files.is_empty());
        assert_eq!(none.parsed.cases.len(), 1);
        assert_eq!(none.parsed.cases[0].status, CaseStatus::Failed);
        assert_eq!(none.parsed.cases[0].file, None);
        assert!(
            none.parsed.cases[0]
                .failure
                .as_deref()
                .unwrap()
                .contains("No tests found")
        );
    }

    #[test]
    fn narrowing_settings_of_the_runner_are_reported() {
        let report = recorded(NARROWED);
        assert_eq!(report.narrowing.len(), 3, "{:?}", report.narrowing);
        assert!(report.narrowing[0].contains("shard"));
        assert!(report.narrowing[1].contains("grepInvert"));
        assert!(report.narrowing[2].contains("maxFailures"));
        // The narrowed file is simply absent; only the configuration tells.
        assert!(!files(&report).contains(&"tests/e2e/mixed.spec.ts"));

        assert!(!recorded(WITHOUT_FORBID_ONLY).forbid_only);
        assert!(recorded(PASSING).forbid_only);
    }

    /// P14: reports that cannot be trusted are rejected, not interpreted.
    #[test]
    fn p14_unusable_reports_are_rejected() {
        let root = Path::new("/tmp/fx/playwright");
        assert!(parse(b"", root).unwrap_err().contains("empty"));
        assert!(parse(b"  \n", root).unwrap_err().contains("empty"));
        assert!(parse(b"{", root).is_err());
        assert!(parse(b"[]", root).is_err());
        assert!(
            parse(b"{\"suites\": []}", root).is_err(),
            "a report without config and stats is not a Playwright report"
        );
        let truncated = &MIXED[..MIXED.len() / 2];
        assert!(parse(truncated, root).is_err());

        // Another workspace: the files are outside of it.
        let elsewhere = parse(MIXED, Path::new("/workspace")).unwrap_err();
        assert!(elsewhere.contains("outside the workspace"), "{elsewhere}");

        // Totals that do not match the tests.
        let mut tampered: serde_json::Value = serde_json::from_slice(PASSING).unwrap();
        tampered["stats"]["expected"] = serde_json::json!(4);
        let error = parse(tampered.to_string().as_bytes(), root).unwrap_err();
        assert!(error.contains("do not match"), "{error}");

        // An unknown status is never guessed.
        let mut unknown: serde_json::Value = serde_json::from_slice(PASSING).unwrap();
        unknown["suites"][0]["specs"][0]["tests"][0]["status"] = serde_json::json!("interrupted");
        let error = parse(unknown.to_string().as_bytes(), root).unwrap_err();
        assert!(error.contains("unknown test status"), "{error}");

        let mut relative: serde_json::Value = serde_json::from_slice(PASSING).unwrap();
        relative["config"]["rootDir"] = serde_json::json!("tests/e2e");
        assert!(parse(relative.to_string().as_bytes(), root).is_err());

        let mut escaping: serde_json::Value = serde_json::from_slice(PASSING).unwrap();
        escaping["suites"][0]["specs"][0]["file"] = serde_json::json!("../../../etc/passwd");
        assert!(parse(escaping.to_string().as_bytes(), root).is_err());
    }

    #[test]
    fn colours_are_removed_from_messages() {
        assert_eq!(
            strip_ansi("\u{1b}[2mexpect(\u{1b}[22m\u{1b}[31mreceived\u{1b}[39m)"),
            "expect(received)"
        );
        assert_eq!(strip_ansi("plain"), "plain");
        assert_eq!(strip_ansi("ends with escape \u{1b}"), "ends with escape ");
        assert_eq!(
            bounded(&"x".repeat(5000)).chars().count(),
            MAX_FAILURE_CHARS
        );
    }
}
