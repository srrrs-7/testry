//! The unified JUnit report of a run (`junit.xml`).
//!
//! One `<testsuite>` per testry suite. Test cases keep their runner project
//! and file in `classname`, so the same test in two projects stays two
//! entries. Statuses that JUnit has no element for are kept as properties:
//! a flaky case is marked `testry.flaky=true`, and a suite records its testry
//! outcome, so nothing is silently turned into a pass.

use std::fmt::Write as _;

use testry_application::ports::execution::{RunResult, SuiteRunResult};
use testry_domain::results::{CaseStatus, TaskOutcome, TestCounts};

/// Escapes text for an attribute value. Characters XML 1.0 cannot represent
/// are replaced.
fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            '\n' => escaped.push_str("&#10;"),
            '\r' => escaped.push_str("&#13;"),
            '\t' => escaped.push_str("&#9;"),
            control if (control as u32) < 0x20 || matches!(control as u32, 0xFFFE | 0xFFFF) => {
                escaped.push('\u{FFFD}');
            }
            other => escaped.push(other),
        }
    }
    escaped
}

fn outcome_name(outcome: TaskOutcome) -> &'static str {
    match outcome {
        TaskOutcome::Passed => "passed",
        TaskOutcome::Failed => "failed",
        TaskOutcome::Skipped => "skipped",
        TaskOutcome::Flaky => "flaky",
        TaskOutcome::TimedOut => "timed_out",
        TaskOutcome::Cancelled => "cancelled",
        TaskOutcome::InfrastructureError => "infrastructure_error",
    }
}

/// `suite:file`, or `suite[project]:file` for a case of a runner that runs
/// several projects in one invocation.
fn classname(suite: &str, project: Option<&str>, file: &str) -> String {
    match project {
        Some(project) => format!("{suite}[{project}]:{file}"),
        None => format!("{suite}:{file}"),
    }
}

fn seconds(duration: std::time::Duration) -> String {
    format!("{:.3}", duration.as_secs_f64())
}

fn write_suite(xml: &mut String, suite: &SuiteRunResult) {
    let result = &suite.result;
    let counts = result.counts.unwrap_or_default();
    let verified = matches!(
        result.outcome,
        TaskOutcome::Passed | TaskOutcome::Flaky | TaskOutcome::Skipped
    );
    // A suite without case data, or one that failed without a failed case
    // (timeout, missing report, policy), gets one synthetic case so the
    // failure is visible to JUnit consumers.
    let needs_synthetic = result.cases.is_empty() || (!verified && counts.failed == 0);
    let synthetic_failure = needs_synthetic && !verified;
    let tests =
        counts.passed + counts.failed + counts.skipped + counts.flaky + u64::from(needs_synthetic);
    let failures =
        counts.failed + u64::from(synthetic_failure && result.outcome == TaskOutcome::Failed);
    let errors = u64::from(synthetic_failure && result.outcome != TaskOutcome::Failed);

    let _ = writeln!(
        xml,
        "  <testsuite name=\"{}\" tests=\"{tests}\" failures=\"{failures}\" errors=\"{errors}\" skipped=\"{}\" time=\"{}\">",
        escape(result.suite_id.as_str()),
        counts.skipped,
        seconds(result.duration),
    );
    xml.push_str("    <properties>\n");
    let mut property = |name: &str, value: &str| {
        let _ = writeln!(
            xml,
            "      <property name=\"{}\" value=\"{}\"/>",
            escape(name),
            escape(value)
        );
    };
    property("testry.runner", suite.runner.as_str());
    property("testry.outcome", outcome_name(result.outcome));
    property(
        "testry.counts",
        if result.counts.is_some() {
            "known"
        } else {
            "unknown"
        },
    );
    for anomaly in &result.anomalies {
        property(&format!("testry.anomaly.{}", anomaly.code), &anomaly.detail);
    }
    for finding in &suite.findings {
        property(&format!("testry.policy.{}", finding.code), &finding.detail);
    }
    xml.push_str("    </properties>\n");

    for case in &result.cases {
        let file = case.file.as_ref().map_or("", |file| file.as_str());
        let name = if case.title_path.is_empty() {
            "(file)".to_owned()
        } else {
            case.title_path.join(" > ")
        };
        let _ = write!(
            xml,
            "    <testcase classname=\"{}\" name=\"{}\" file=\"{}\"",
            escape(&classname(
                result.suite_id.as_str(),
                case.project.as_deref(),
                file
            )),
            escape(&name),
            escape(file),
        );
        let message = escape(case.failure.as_deref().unwrap_or(""));
        match case.status {
            CaseStatus::Passed => xml.push_str("/>\n"),
            CaseStatus::Failed => {
                let _ = writeln!(
                    xml,
                    ">\n      <failure message=\"{message}\"/>\n    </testcase>"
                );
            }
            CaseStatus::Skipped => xml.push_str(">\n      <skipped/>\n    </testcase>\n"),
            CaseStatus::Todo => {
                xml.push_str(">\n      <skipped message=\"todo\"/>\n    </testcase>\n")
            }
            CaseStatus::ExpectedFailure => {
                xml.push_str(">\n      <skipped message=\"expected failure\"/>\n    </testcase>\n")
            }
            CaseStatus::Flaky => {
                let _ = writeln!(
                    xml,
                    ">\n      <properties>\n        <property name=\"testry.flaky\" value=\"true\"/>\n        <property name=\"testry.first_failure\" value=\"{message}\"/>\n      </properties>\n    </testcase>"
                );
            }
        }
    }

    if needs_synthetic {
        let _ = write!(
            xml,
            "    <testcase classname=\"{}\" name=\"{}\"",
            escape(result.suite_id.as_str()),
            if result.cases.is_empty() {
                "suite"
            } else {
                "suite outcome"
            },
        );
        if synthetic_failure {
            let detail: Vec<String> = result
                .anomalies
                .iter()
                .map(|anomaly| format!("{}: {}", anomaly.code, anomaly.detail))
                .chain(
                    suite
                        .findings
                        .iter()
                        .map(|finding| format!("{}: {}", finding.code, finding.detail)),
                )
                .collect();
            let message = escape(&format!(
                "{} {}",
                outcome_name(result.outcome),
                detail.join("; ")
            ));
            let element = if result.outcome == TaskOutcome::Failed {
                "failure"
            } else {
                "error"
            };
            let _ = writeln!(
                xml,
                ">\n      <{element} message=\"{message}\"/>\n    </testcase>"
            );
        } else {
            xml.push_str("/>\n");
        }
    }
    xml.push_str("  </testsuite>\n");
}

/// Renders the unified report.
pub fn render(result: &RunResult) -> String {
    let totals = result
        .suites
        .iter()
        .filter_map(|suite| suite.result.counts)
        .fold(TestCounts::default(), |mut total, counts| {
            total.passed += counts.passed;
            total.failed += counts.failed;
            total.skipped += counts.skipped;
            total.flaky += counts.flaky;
            total
        });
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    let _ = writeln!(
        xml,
        "<testsuites name=\"testry\" tests=\"{}\" failures=\"{}\" skipped=\"{}\">",
        totals.passed + totals.failed + totals.skipped + totals.flaky,
        totals.failed,
        totals.skipped,
    );
    for suite in &result.suites {
        write_suite(&mut xml, suite);
    }
    xml.push_str("</testsuites>\n");
    xml
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use quick_xml::Reader;
    use quick_xml::events::Event;
    use testry_application::ports::execution::{
        Anomaly, ArtifactRef, CleanupReport, FileAttribution, InputValidation, PolicyFinding,
        RunMetrics, SuiteResult,
    };
    use testry_domain::fingerprint::Fingerprint;
    use testry_domain::ids::{PlanId, RunId, SuiteId, TaskId};
    use testry_domain::paths::RepoPath;
    use testry_domain::plan::Mode;
    use testry_domain::results::{Assurance, CaseResult, RunOutcome};
    use testry_domain::selection::Selection;
    use testry_domain::suite::RunnerKind;

    use super::*;

    fn case(file: &str, title: &[&str], status: CaseStatus, failure: Option<&str>) -> CaseResult {
        CaseResult {
            project: None,
            file: Some(RepoPath::try_new(file).unwrap()),
            title_path: title.iter().map(|part| (*part).to_owned()).collect(),
            status,
            failure: failure.map(str::to_owned),
        }
    }

    fn suite(
        id: &str,
        runner: RunnerKind,
        outcome: TaskOutcome,
        cases: Vec<CaseResult>,
    ) -> SuiteRunResult {
        SuiteRunResult {
            result: SuiteResult {
                suite_id: SuiteId::try_new(id).unwrap(),
                task_id: TaskId::try_new(id).unwrap(),
                outcome,
                executed_files: FileAttribution::Unknown,
                counts: (!cases.is_empty()).then(|| TestCounts::from_cases(&cases)),
                attempts: 1,
                cases,
                anomalies: Vec::new(),
                duration: Duration::from_millis(1500),
                artifacts: Vec::new(),
            },
            effective_selection: Selection::All,
            runner,
            findings: Vec::new(),
            report: Some(ArtifactRef {
                relative_path: PathBuf::from("tasks/x/report.xml"),
            }),
        }
    }

    fn run(suites: Vec<SuiteRunResult>) -> RunResult {
        RunResult {
            run_id: RunId::try_new("123e4567-e89b-42d3-a456-426614174000").unwrap(),
            plan_id: PlanId::new(Fingerprint::of_bytes(b"plan")),
            mode: Mode::Full,
            scope: Vec::new(),
            suites,
            assurance: Assurance::Advisory,
            outcome: RunOutcome::Failed,
            input_validation: InputValidation::Unchanged,
            environment: None,
            shadow_gaps: None,
            metrics: RunMetrics::default(),
            cleanup: CleanupReport::default(),
            exit_code: 1,
        }
    }

    /// Element names and attributes of the document, proving it is well-formed.
    fn elements(xml: &str) -> Vec<String> {
        let mut reader = Reader::from_str(xml);
        let mut found = Vec::new();
        loop {
            match reader
                .read_event()
                .expect("the report must be well-formed XML")
            {
                Event::Eof => break,
                Event::Start(element) | Event::Empty(element) => {
                    let attributes: Vec<String> = element
                        .attributes()
                        .map(|attribute| {
                            let attribute = attribute.unwrap();
                            let value = attribute
                                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                                .unwrap();
                            format!("{}={}", attribute.key.into_inner(), value)
                        })
                        .collect();
                    found.push(format!(
                        "{} {}",
                        element.name().into_inner(),
                        attributes.join(" ")
                    ));
                }
                _ => {}
            }
        }
        found
    }

    #[test]
    fn statuses_are_kept_apart() {
        let cases = vec![
            case("a.test.ts", &["math", "adds"], CaseStatus::Passed, None),
            case(
                "a.test.ts",
                &["fails"],
                CaseStatus::Failed,
                Some("expected 1 to be 2"),
            ),
            case("a.test.ts", &["later"], CaseStatus::Skipped, None),
            case("a.test.ts", &["someday"], CaseStatus::Todo, None),
            case(
                "a.test.ts",
                &["sometimes"],
                CaseStatus::Flaky,
                Some("first attempt"),
            ),
        ];
        let xml = render(&run(vec![suite(
            "api-unit",
            RunnerKind::Bun,
            TaskOutcome::Failed,
            cases,
        )]));
        let found = elements(&xml);

        assert!(
            found.contains(&"testsuites name=testry tests=5 failures=1 skipped=2".to_owned()),
            "{found:#?}"
        );
        assert!(found.iter().any(|e| e.starts_with(
            "testsuite name=api-unit tests=5 failures=1 errors=0 skipped=2 time=1.500"
        )));
        assert!(found.contains(
            &"testcase classname=api-unit:a.test.ts name=math > adds file=a.test.ts".to_owned()
        ));
        assert!(found.contains(&"failure message=expected 1 to be 2".to_owned()));
        assert!(found.contains(&"skipped message=todo".to_owned()));
        assert!(found.contains(&"property name=testry.flaky value=true".to_owned()));
        assert!(found.contains(&"property name=testry.outcome value=failed".to_owned()));
        // The flaky case is not a plain passing testcase.
        assert_eq!(
            xml.matches("name=\"sometimes\" file=\"a.test.ts\"/>")
                .count(),
            0
        );
    }

    /// P11: the same test in two projects stays two entries.
    #[test]
    fn p11_same_test_in_two_suites_does_not_collide() {
        let unit = suite(
            "web-unit",
            RunnerKind::Vitest,
            TaskOutcome::Passed,
            vec![case("App.test.tsx", &["renders"], CaseStatus::Passed, None)],
        );
        let browser = suite(
            "web-browser",
            RunnerKind::Vitest,
            TaskOutcome::Passed,
            vec![case("App.test.tsx", &["renders"], CaseStatus::Passed, None)],
        );
        let found = elements(&render(&run(vec![unit, browser])));
        assert!(found.contains(
            &"testcase classname=web-unit:App.test.tsx name=renders file=App.test.tsx".to_owned()
        ));
        assert!(
            found.contains(
                &"testcase classname=web-browser:App.test.tsx name=renders file=App.test.tsx"
                    .to_owned()
            )
        );
    }

    /// The browser project of a Playwright case stays part of its identity,
    /// and an expected failure is not written as a pass.
    #[test]
    fn projects_of_one_invocation_and_expected_failures_stay_visible() {
        let in_project = |project: &str, status| {
            let mut case = case("login.spec.ts", &["signs in"], status, None);
            case.project = Some(project.to_owned());
            case
        };
        let e2e = suite(
            "e2e",
            RunnerKind::Playwright,
            TaskOutcome::Passed,
            vec![
                in_project("chromium", CaseStatus::Passed),
                in_project("firefox", CaseStatus::ExpectedFailure),
            ],
        );
        let xml = render(&run(vec![e2e]));
        let found = elements(&xml);
        assert!(
            found.contains(
                &"testcase classname=e2e[chromium]:login.spec.ts name=signs in file=login.spec.ts"
                    .to_owned()
            ),
            "{found:?}"
        );
        assert!(
            found.contains(
                &"testcase classname=e2e[firefox]:login.spec.ts name=signs in file=login.spec.ts"
                    .to_owned()
            ),
            "{found:?}"
        );
        assert!(
            xml.contains("<skipped message=\"expected failure\"/>"),
            "{xml}"
        );
        assert!(
            xml.contains("tests=\"2\" failures=\"0\" errors=\"0\" skipped=\"1\""),
            "{xml}"
        );
    }

    /// P12: a command gate has no invented test count.
    #[test]
    fn p12_a_command_gate_is_one_case_with_unknown_counts() {
        let passed = suite(
            "typecheck",
            RunnerKind::Command,
            TaskOutcome::Passed,
            Vec::new(),
        );
        let found = elements(&render(&run(vec![passed])));
        assert!(found.contains(&"property name=testry.counts value=unknown".to_owned()));
        assert!(
            found
                .iter()
                .any(|e| e.starts_with("testsuite name=typecheck tests=1 failures=0 errors=0"))
        );
        assert!(found.contains(&"testcase classname=typecheck name=suite".to_owned()));

        let failed = suite(
            "typecheck",
            RunnerKind::Command,
            TaskOutcome::Failed,
            Vec::new(),
        );
        let found = elements(&render(&run(vec![failed])));
        assert!(
            found
                .iter()
                .any(|e| e.starts_with("testsuite name=typecheck tests=1 failures=1 errors=0"))
        );
        assert!(
            found
                .iter()
                .any(|e| e.starts_with("failure message=failed"))
        );
    }

    #[test]
    fn infrastructure_problems_and_policy_failures_are_visible() {
        let mut broken = suite(
            "api-unit",
            RunnerKind::Bun,
            TaskOutcome::InfrastructureError,
            Vec::new(),
        );
        broken.result.anomalies = vec![Anomaly {
            code: "report_missing",
            detail: "the runner did not write its report".to_owned(),
        }];
        let found = elements(&render(&run(vec![broken])));
        assert!(
            found
                .iter()
                .any(|e| e.starts_with("testsuite name=api-unit tests=1 failures=0 errors=1"))
        );
        assert!(
            found
                .iter()
                .any(|e| e.starts_with("error message=infrastructure_error report_missing"))
        );

        // All cases passed, but a policy turned the suite into a failure.
        let mut policy = suite(
            "web-unit",
            RunnerKind::Vitest,
            TaskOutcome::Failed,
            vec![
                case("a.test.ts", &["ok"], CaseStatus::Passed, None),
                case("a.test.ts", &["later"], CaseStatus::Skipped, None),
            ],
        );
        policy.findings = vec![PolicyFinding {
            code: "unexpected_skips",
            detail: "1 skipped test is not listed".to_owned(),
            fails_suite: true,
        }];
        let found = elements(&render(&run(vec![policy])));
        assert!(found.iter().any(|e| {
            e.starts_with("testsuite name=web-unit tests=3 failures=1 errors=0 skipped=1")
        }));
        assert!(
            found
                .iter()
                .any(|e| e.starts_with("failure message=failed unexpected_skips"))
        );
    }

    #[test]
    fn hostile_text_is_escaped_and_stays_well_formed() {
        let cases = vec![case(
            "apps/日本語 の/<a>&\"b\".test.ts",
            &["x\ny", "</testcase><evil attr='1'/>"],
            CaseStatus::Failed,
            Some("line1\nline2 \u{0} \u{1b}[31m ]]> &amp;"),
        )];
        let xml = render(&run(vec![suite(
            "api-unit",
            RunnerKind::Bun,
            TaskOutcome::Failed,
            cases,
        )]));
        let found = elements(&xml);
        assert!(
            !found.iter().any(|e| e.starts_with("evil")),
            "markup in a test name must stay text"
        );
        assert!(
            found
                .iter()
                .any(|e| e.contains("</testcase><evil attr='1'/>"))
        );
        assert!(
            found
                .iter()
                .any(|e| e.contains("apps/日本語 の/<a>&\"b\".test.ts"))
        );
        assert!(found.iter().any(|e| {
            e.starts_with("failure message=line1\nline2 \u{FFFD} \u{FFFD}[31m ]]> &amp;")
        }));
    }

    #[test]
    fn an_empty_run_is_a_valid_document() {
        let found = elements(&render(&run(Vec::new())));
        assert_eq!(
            found,
            ["testsuites name=testry tests=0 failures=0 skipped=0"]
        );
    }
}
