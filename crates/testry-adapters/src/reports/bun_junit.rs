//! Parser for the JUnit XML report of `bun test` (verified with Bun 1.4.4).
//!
//! Shape of the report:
//!
//! ```text
//! <testsuites>
//!   <testsuite name="src/a.test.ts" file="src/a.test.ts">     one per test file
//!     <testsuite name="math" file="src/a.test.ts">            one per describe block
//!       <testcase name="adds" file="src/a.test.ts"/>
//!     </testsuite>
//!     <testcase name="fails"><failure message="..."/></testcase>
//!     <testcase name="later"><skipped/></testcase>
//!     <testcase name="todo"><skipped message="TODO"/></testcase>
//!   </testsuite>
//! </testsuites>
//! ```
//!
//! File paths are relative to the directory `bun test` ran in.
//!
//! The parser is defensive: a document type declaration is rejected (no
//! entity can be defined or expanded), nesting depth is bounded, and only
//! attributes are read. Text content is never interpreted.

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use testry_application::results::ParsedReport;
use testry_domain::paths::{RepoDir, RepoPath};
use testry_domain::results::{CaseResult, CaseStatus};

const MAX_DEPTH: usize = 64;
const MAX_FAILURE_CHARS: usize = 2000;

struct OpenCase {
    file: Option<RepoPath>,
    name: String,
    status: CaseStatus,
    failure: Option<String>,
}

fn attribute(element: &BytesStart<'_>, name: &str) -> Result<Option<String>, String> {
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|error| format!("malformed attribute: {error}"))?;
        if attribute.key.into_inner() == name {
            let value = attribute
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .map_err(|error| format!("attribute `{name}` cannot be decoded: {error}"))?;
            return Ok(Some(value.into_owned()));
        }
    }
    Ok(None)
}

fn repo_path(cwd: &RepoDir, reported: &str) -> Result<RepoPath, String> {
    cwd.join(reported).map_err(|error| {
        format!("the report names a file outside the suite directory ({reported:?}): {error}")
    })
}

/// Parses a report. `cwd` is the suite directory the paths are relative to.
pub fn parse(bytes: &[u8], cwd: &RepoDir) -> Result<ParsedReport, String> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| "the report is not valid UTF-8".to_owned())?;
    if text.trim().is_empty() {
        return Err("the report is empty".to_owned());
    }
    let mut reader = Reader::from_str(text);
    // `<testcase/>` is handled like `<testcase></testcase>`.
    reader.config_mut().expand_empty_elements = true;

    let mut report = ParsedReport::default();
    let mut saw_root = false;
    let mut depth = 0usize;
    // Files of the enclosing `<testsuite>` elements, outermost first, and the
    // titles of the describe blocks among them.
    let mut suite_files: Vec<Option<RepoPath>> = Vec::new();
    let mut titles: Vec<String> = Vec::new();
    let mut case: Option<OpenCase> = None;

    loop {
        let event = reader
            .read_event()
            .map_err(|error| format!("the report is not well-formed XML: {error}"))?;
        match event {
            Event::Eof => break,
            Event::DocType(_) => {
                return Err(
                    "the report contains a document type declaration, which is not allowed"
                        .to_owned(),
                );
            }
            Event::Start(element) => {
                depth += 1;
                if depth > MAX_DEPTH {
                    return Err(format!(
                        "the report is nested deeper than {MAX_DEPTH} elements"
                    ));
                }
                match element.name().into_inner() {
                    "testsuites" if depth == 1 => saw_root = true,
                    "testsuite" => {
                        saw_root = true;
                        let name = attribute(&element, "name")?.unwrap_or_default();
                        let file = match attribute(&element, "file")? {
                            Some(file) => Some(repo_path(cwd, &file)?),
                            None => suite_files.last().cloned().flatten(),
                        };
                        if suite_files.is_empty() {
                            // The outermost suite stands for a test file.
                            let file = match file.clone() {
                                Some(file) => file,
                                None => repo_path(cwd, &name)?,
                            };
                            report.files.insert(file);
                        } else {
                            titles.push(name);
                        }
                        suite_files.push(file);
                    }
                    "testcase" => {
                        let file = match attribute(&element, "file")? {
                            Some(file) => Some(repo_path(cwd, &file)?),
                            None => suite_files.last().cloned().flatten(),
                        };
                        case = Some(OpenCase {
                            file,
                            name: attribute(&element, "name")?.unwrap_or_default(),
                            status: CaseStatus::Passed,
                            failure: None,
                        });
                    }
                    "failure" | "error" => {
                        if let Some(case) = &mut case {
                            case.status = CaseStatus::Failed;
                            let message = attribute(&element, "message")?.unwrap_or_default();
                            case.failure = Some(message.chars().take(MAX_FAILURE_CHARS).collect());
                        }
                    }
                    "skipped" => {
                        if let Some(case) = &mut case
                            && case.status != CaseStatus::Failed
                        {
                            let is_todo =
                                attribute(&element, "message")?.as_deref() == Some("TODO");
                            case.status = if is_todo {
                                CaseStatus::Todo
                            } else {
                                CaseStatus::Skipped
                            };
                        }
                    }
                    _ => {}
                }
            }
            Event::End(element) => {
                depth = depth.saturating_sub(1);
                match element.name().into_inner() {
                    "testsuite" => {
                        suite_files.pop();
                        if !suite_files.is_empty() {
                            titles.pop();
                        }
                    }
                    "testcase" => {
                        if let Some(case) = case.take() {
                            let mut title_path = titles.clone();
                            title_path.push(case.name);
                            report.cases.push(CaseResult {
                                project: None,
                                file: case.file,
                                title_path,
                                status: case.status,
                                failure: case.failure,
                            });
                        }
                    }
                    _ => {}
                }
            }
            // Text, CDATA, comments, processing instructions and entity
            // references carry nothing this parser uses.
            _ => {}
        }
    }

    if !saw_root {
        return Err("the document is not a JUnit report".to_owned());
    }
    if depth != 0 {
        return Err("the report ends inside an element".to_owned());
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use testry_domain::results::TestCounts;

    use super::*;

    const MIXED: &[u8] =
        include_bytes!("../../../../tests/fixtures/runner-reports/bun-1.4.4/mixed.xml");
    const PASSING: &[u8] =
        include_bytes!("../../../../tests/fixtures/runner-reports/bun-1.4.4/passing.xml");
    const IMPORT_ERROR: &[u8] =
        include_bytes!("../../../../tests/fixtures/runner-reports/bun-1.4.4/import-error.xml");

    fn root() -> RepoDir {
        RepoDir::root()
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
        let report = parse(MIXED, &root()).unwrap();
        assert_eq!(
            files(&report),
            [
                "src/failing.test.ts",
                "src/math.test.ts",
                "src/skips.test.ts",
                "src/with space/日本語 [x].test.ts"
            ]
        );
        assert_eq!(
            TestCounts::from_cases(&report.cases),
            TestCounts {
                passed: 5,
                failed: 1,
                skipped: 3,
                flaky: 0
            }
        );

        // describe blocks become the title path.
        assert_eq!(
            case(&report, &["math", "add", "adds"]).status,
            CaseStatus::Passed
        );
        assert_eq!(
            case(&report, &["math", "subtracts"]).status,
            CaseStatus::Passed
        );
        assert_eq!(
            case(&report, &["group", "nested skip"]).status,
            CaseStatus::Skipped
        );
        assert_eq!(case(&report, &["skipped one"]).status, CaseStatus::Skipped);
        assert_eq!(case(&report, &["todo one"]).status, CaseStatus::Todo);

        let failed = case(&report, &["fails"]);
        assert_eq!(failed.status, CaseStatus::Failed);
        assert_eq!(
            failed.file.as_ref().unwrap().as_str(),
            "src/failing.test.ts"
        );
        assert!(failed.failure.as_ref().unwrap().contains("Expected: 2"));

        // Escaped characters in names are decoded.
        assert_eq!(
            case(&report, &["passes <&\"quote\">"]).status,
            CaseStatus::Passed
        );
    }

    #[test]
    fn parses_a_real_passing_report_with_unusual_paths() {
        let report = parse(PASSING, &root()).unwrap();
        assert_eq!(
            files(&report),
            ["src/math.test.ts", "src/with space/日本語 [x].test.ts"]
        );
        assert!(
            report
                .cases
                .iter()
                .all(|case| case.status == CaseStatus::Passed)
        );
        assert_eq!(report.cases.len(), 3);
    }

    /// A file that throws while it is imported is absent from Bun's report.
    /// The caller notices through the expected files and the exit code.
    #[test]
    fn a_file_with_an_import_error_is_absent_from_the_real_report() {
        let report = parse(IMPORT_ERROR, &root()).unwrap();
        assert_eq!(files(&report), ["src/math.test.ts"]);
        assert_eq!(TestCounts::from_cases(&report.cases).failed, 0);
    }

    #[test]
    fn paths_are_relative_to_the_suite_directory() {
        let cwd = RepoDir::try_new("apps/api").unwrap();
        let report = parse(PASSING, &cwd).unwrap();
        assert_eq!(
            files(&report),
            [
                "apps/api/src/math.test.ts",
                "apps/api/src/with space/日本語 [x].test.ts"
            ]
        );
        assert_eq!(
            report.cases[0].file.as_ref().unwrap().as_str(),
            "apps/api/src/math.test.ts"
        );
    }

    /// P14: empty, broken and hostile reports fail safely.
    #[test]
    fn p14_unusable_reports_are_rejected() {
        let rejected: &[(&str, &[u8])] = &[
            ("empty", b""),
            ("whitespace", b"  \n"),
            ("not xml", b"Ran 3 tests across 1 file."),
            ("truncated", b"<testsuites><testsuite name=\"a.test.ts\" file=\"a.test.ts\"><testcase name=\"x\""),
            ("unclosed", b"<testsuites><testsuite name=\"a.test.ts\" file=\"a.test.ts\">"),
            ("mismatched", b"<testsuites><testsuite name=\"a\" file=\"a\"></testcase></testsuites>"),
            ("other document", b"<html><body/></html>"),
            ("not utf-8", b"<testsuites>\xff</testsuites>"),
            (
                "path escape",
                b"<testsuites><testsuite name=\"x\" file=\"../../etc/passwd\"></testsuite></testsuites>",
            ),
            (
                "absolute path",
                b"<testsuites><testsuite name=\"x\" file=\"/etc/passwd\"></testsuite></testsuites>",
            ),
        ];
        for (name, bytes) in rejected {
            assert!(parse(bytes, &root()).is_err(), "{name} must be rejected");
        }
    }

    /// P14: a document type declaration (the vehicle of XXE and entity
    /// expansion attacks) is rejected outright.
    #[test]
    fn p14_doctype_and_entities_are_rejected() {
        let xxe = br#"<?xml version="1.0"?>
<!DOCTYPE testsuites [ <!ENTITY xxe SYSTEM "file:///etc/passwd"> ]>
<testsuites><testsuite name="a.test.ts" file="a.test.ts"><testcase name="&xxe;"/></testsuite></testsuites>"#;
        let error = parse(xxe, &root()).unwrap_err();
        assert!(error.contains("document type"), "{error}");

        let billion_laughs =
            br#"<!DOCTYPE lolz [ <!ENTITY lol "lol"> <!ENTITY lol2 "&lol;&lol;&lol;&lol;"> ]>
<testsuites><testsuite name="&lol2;" file="a.test.ts"/></testsuites>"#;
        assert!(parse(billion_laughs, &root()).is_err());

        // An undefined entity in an attribute is an error, not an empty string.
        let undefined =
            br#"<testsuites><testsuite name="a" file="&undefined;.test.ts"/></testsuites>"#;
        assert!(parse(undefined, &root()).is_err());
    }

    #[test]
    fn nesting_depth_is_bounded() {
        let mut deep =
            String::from("<testsuites><testsuite name=\"a.test.ts\" file=\"a.test.ts\">");
        for _ in 0..100 {
            deep.push_str("<testsuite name=\"d\">");
        }
        for _ in 0..100 {
            deep.push_str("</testsuite>");
        }
        deep.push_str("</testsuite></testsuites>");
        let error = parse(deep.as_bytes(), &root()).unwrap_err();
        assert!(error.contains("nested deeper"), "{error}");
    }

    #[test]
    fn a_report_without_tests_parses_as_empty() {
        let report = parse(
            b"<testsuites name=\"bun test\" tests=\"0\"></testsuites>",
            &root(),
        )
        .unwrap();
        assert!(report.files.is_empty() && report.cases.is_empty());
    }

    #[test]
    fn a_failure_wins_over_a_skip_marker() {
        let xml = br#"<testsuites><testsuite name="a.test.ts" file="a.test.ts">
            <testcase name="odd"><failure message="boom"/><skipped/></testcase>
            <testcase name="error"><error message="crash"/></testcase>
        </testsuite></testsuites>"#;
        let report = parse(xml, &root()).unwrap();
        assert_eq!(case(&report, &["odd"]).status, CaseStatus::Failed);
        assert_eq!(case(&report, &["error"]).status, CaseStatus::Failed);
    }
}
