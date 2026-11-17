//! Result policies (spec sections 9 and 15): unexpected skips and flaky tests.
//!
//! A skipped, todo, expected-to-fail or flaky test is never folded into a
//! pass. An approved skip must be listed exactly in `known_skips`; there are
//! no wildcards.

use testry_domain::results::{CaseResult, CaseStatus, TaskOutcome};
use testry_domain::suite::{KnownSkip, SkipStatus, SuiteDefinition};

use crate::ports::execution::{PolicyFinding, SuiteResult};

pub const UNEXPECTED_SKIPS: &str = "unexpected_skips";
pub const FLAKY_TESTS: &str = "flaky_tests";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResultPolicy {
    pub fail_on_unexpected_skips: bool,
    pub fail_on_flaky: bool,
}

fn is_known(case: &CaseResult, suite: &SuiteDefinition) -> bool {
    let Some(file) = &case.file else {
        // Without a file there is no stable case identity to approve.
        return false;
    };
    suite.known_skips().iter().any(|known: &KnownSkip| {
        let status_matches = matches!(
            (known.expected_status, case.status),
            (SkipStatus::Skipped, CaseStatus::Skipped)
                | (SkipStatus::Todo, CaseStatus::Todo)
                | (SkipStatus::ExpectedFailure, CaseStatus::ExpectedFailure)
        );
        // A case of a runner with several projects per invocation carries
        // its own project; otherwise the project of the suite applies.
        let project_matches = known
            .project
            .as_ref()
            .is_none_or(|project| match &case.project {
                Some(reported) => reported == project.as_str(),
                None => Some(project) == suite.project(),
            });
        status_matches
            && project_matches
            && &known.file == file
            && known.title_path == case.title_path
    })
}

fn describe(case: &CaseResult) -> String {
    let file = case
        .file
        .as_ref()
        .map_or("<unknown file>", |file| file.as_str());
    let project = case
        .project
        .as_ref()
        .map_or_else(String::new, |project| format!("[{project}] "));
    format!("{project}{file} > {}", case.title_path.join(" > "))
}

/// Applies the policies to a suite result. A violated policy turns a result
/// that would otherwise count as verified into `Failed`; it never improves an
/// outcome.
pub fn apply_result_policy(
    result: &mut SuiteResult,
    suite: &SuiteDefinition,
    policy: ResultPolicy,
) -> Vec<PolicyFinding> {
    let mut findings = Vec::new();
    let verified = |outcome: TaskOutcome| {
        matches!(
            outcome,
            TaskOutcome::Passed | TaskOutcome::Flaky | TaskOutcome::Skipped
        )
    };

    let unexpected: Vec<&CaseResult> = result
        .cases
        .iter()
        .filter(|case| {
            matches!(
                case.status,
                CaseStatus::Skipped | CaseStatus::Todo | CaseStatus::ExpectedFailure
            )
        })
        .filter(|case| !is_known(case, suite))
        .collect();
    if let Some(first) = unexpected.first() {
        findings.push(PolicyFinding {
            code: UNEXPECTED_SKIPS,
            detail: format!(
                "{} skipped, todo or expected-to-fail test(s) are not listed in known_skips (first: {})",
                unexpected.len(),
                describe(first)
            ),
            fails_suite: policy.fail_on_unexpected_skips,
        });
        if policy.fail_on_unexpected_skips && verified(result.outcome) {
            result.outcome = TaskOutcome::Failed;
        }
    }

    let flaky: Vec<&CaseResult> = result
        .cases
        .iter()
        .filter(|case| case.status == CaseStatus::Flaky)
        .collect();
    if let Some(first) = flaky.first() {
        findings.push(PolicyFinding {
            code: FLAKY_TESTS,
            detail: format!(
                "{} test(s) passed only after a retry (first: {})",
                flaky.len(),
                describe(first)
            ),
            fails_suite: policy.fail_on_flaky,
        });
        if policy.fail_on_flaky && verified(result.outcome) {
            result.outcome = TaskOutcome::Failed;
        }
    }
    findings
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use testry_domain::ids::{RunnerProject, SuiteId, TaskId};
    use testry_domain::paths::RepoPath;
    use testry_domain::results::TestCounts;
    use testry_domain::suite::RunnerKind;

    use super::*;
    use crate::ports::execution::FileAttribution;
    use crate::settings::test_support::suite_parts;

    fn path(raw: &str) -> RepoPath {
        RepoPath::try_new(raw).unwrap()
    }

    fn case(file: &str, title: &[&str], status: CaseStatus) -> CaseResult {
        CaseResult {
            project: None,
            file: Some(path(file)),
            title_path: title.iter().map(|part| (*part).to_owned()).collect(),
            status,
            failure: None,
        }
    }

    fn result(outcome: TaskOutcome, cases: Vec<CaseResult>) -> SuiteResult {
        SuiteResult {
            suite_id: SuiteId::try_new("web").unwrap(),
            task_id: TaskId::try_new("web").unwrap(),
            outcome,
            executed_files: FileAttribution::Unknown,
            counts: Some(TestCounts::from_cases(&cases)),
            attempts: 1,
            cases,
            anomalies: Vec::new(),
            duration: Duration::ZERO,
            artifacts: Vec::new(),
        }
    }

    fn suite(known_skips: Vec<KnownSkip>) -> SuiteDefinition {
        let mut parts = suite_parts("web", RunnerKind::Vitest, &["**/*.test.ts"]);
        parts.known_skips = known_skips;
        SuiteDefinition::try_new(parts).unwrap()
    }

    const STRICT: ResultPolicy = ResultPolicy {
        fail_on_unexpected_skips: true,
        fail_on_flaky: true,
    };
    const LENIENT: ResultPolicy = ResultPolicy {
        fail_on_unexpected_skips: false,
        fail_on_flaky: false,
    };

    /// P10: a new, unapproved skip or todo violates the policy.
    #[test]
    fn p10_unexpected_skips_fail_the_suite() {
        for status in [
            CaseStatus::Skipped,
            CaseStatus::Todo,
            CaseStatus::ExpectedFailure,
        ] {
            let mut result = result(
                TaskOutcome::Passed,
                vec![
                    case("a.test.ts", &["ok"], CaseStatus::Passed),
                    case("a.test.ts", &["later"], status),
                ],
            );
            let findings = apply_result_policy(&mut result, &suite(Vec::new()), STRICT);
            assert_eq!(result.outcome, TaskOutcome::Failed, "{status:?}");
            assert_eq!(findings.len(), 1);
            assert_eq!(findings[0].code, UNEXPECTED_SKIPS);
            assert!(findings[0].fails_suite);
            assert!(findings[0].detail.contains("a.test.ts > later"));
        }
    }

    #[test]
    fn an_exactly_listed_skip_is_accepted() {
        let known = KnownSkip {
            project: None,
            file: path("a.test.ts"),
            title_path: vec!["group".to_owned(), "later".to_owned()],
            expected_status: SkipStatus::Skipped,
        };
        let cases = vec![
            case("a.test.ts", &["ok"], CaseStatus::Passed),
            case("a.test.ts", &["group", "later"], CaseStatus::Skipped),
        ];
        let mut accepted = result(TaskOutcome::Passed, cases.clone());
        assert!(apply_result_policy(&mut accepted, &suite(vec![known.clone()]), STRICT).is_empty());
        assert_eq!(accepted.outcome, TaskOutcome::Passed);
        // The skip stays visible in the counts.
        assert_eq!(accepted.counts.unwrap().skipped, 1);

        // Any difference in status, title path, file or project is unexpected.
        let variants = [
            KnownSkip {
                expected_status: SkipStatus::Todo,
                ..known.clone()
            },
            KnownSkip {
                title_path: vec!["later".to_owned()],
                ..known.clone()
            },
            KnownSkip {
                file: path("b.test.ts"),
                ..known.clone()
            },
            KnownSkip {
                project: Some(RunnerProject::try_new("other").unwrap()),
                ..known.clone()
            },
        ];
        for variant in variants {
            let mut rejected = result(TaskOutcome::Passed, cases.clone());
            let findings = apply_result_policy(&mut rejected, &suite(vec![variant]), STRICT);
            assert_eq!(rejected.outcome, TaskOutcome::Failed);
            assert_eq!(findings.len(), 1);
        }

        let same_project = KnownSkip {
            project: Some(RunnerProject::try_new("web").unwrap()),
            ..known
        };
        let mut accepted = result(TaskOutcome::Passed, cases);
        assert!(apply_result_policy(&mut accepted, &suite(vec![same_project]), STRICT).is_empty());
    }

    /// An expected failure ran but verified nothing. It is approved like a
    /// skip, and an approval of a skip does not cover it.
    #[test]
    fn an_expected_failure_needs_its_own_approval() {
        let cases = vec![
            case("a.spec.ts", &["ok"], CaseStatus::Passed),
            case("a.spec.ts", &["known bug"], CaseStatus::ExpectedFailure),
        ];
        let approval = |expected_status| KnownSkip {
            project: None,
            file: path("a.spec.ts"),
            title_path: vec!["known bug".to_owned()],
            expected_status,
        };

        let mut accepted = result(TaskOutcome::Passed, cases.clone());
        let known = suite(vec![approval(SkipStatus::ExpectedFailure)]);
        assert!(apply_result_policy(&mut accepted, &known, STRICT).is_empty());
        assert_eq!(accepted.outcome, TaskOutcome::Passed);
        let counts = accepted.counts.unwrap();
        assert_eq!(
            (counts.passed, counts.skipped),
            (1, 1),
            "an expected failure is never counted as passed"
        );

        let mut rejected = result(TaskOutcome::Passed, cases);
        let as_skip = suite(vec![approval(SkipStatus::Skipped)]);
        let findings = apply_result_policy(&mut rejected, &as_skip, STRICT);
        assert_eq!(rejected.outcome, TaskOutcome::Failed);
        assert_eq!(findings[0].code, UNEXPECTED_SKIPS);
    }

    /// A runner that runs several projects in one invocation reports the
    /// project of each case; an approval for one project does not cover the
    /// same test in another.
    #[test]
    fn a_skip_is_approved_for_the_project_the_case_ran_in() {
        let skipped_in = |project: &str| {
            let mut case = case("a.spec.ts", &["later"], CaseStatus::Skipped);
            case.project = Some(project.to_owned());
            case
        };
        let known = KnownSkip {
            project: Some(RunnerProject::try_new("firefox").unwrap()),
            file: path("a.spec.ts"),
            title_path: vec!["later".to_owned()],
            expected_status: SkipStatus::Skipped,
        };

        let mut firefox = result(TaskOutcome::Passed, vec![skipped_in("firefox")]);
        assert!(apply_result_policy(&mut firefox, &suite(vec![known.clone()]), STRICT).is_empty());

        let mut both = result(
            TaskOutcome::Passed,
            vec![skipped_in("firefox"), skipped_in("chromium")],
        );
        let findings = apply_result_policy(&mut both, &suite(vec![known.clone()]), STRICT);
        assert_eq!(both.outcome, TaskOutcome::Failed);
        assert!(
            findings[0].detail.contains("[chromium] a.spec.ts > later"),
            "{}",
            findings[0].detail
        );

        // Without a project the approval covers the test in every project.
        let any_project = KnownSkip {
            project: None,
            ..known
        };
        let mut both = result(
            TaskOutcome::Passed,
            vec![skipped_in("firefox"), skipped_in("chromium")],
        );
        assert!(apply_result_policy(&mut both, &suite(vec![any_project]), STRICT).is_empty());
    }

    #[test]
    fn a_skip_without_file_attribution_cannot_be_approved() {
        let mut unattributed = case("a.test.ts", &["later"], CaseStatus::Skipped);
        unattributed.file = None;
        let mut result = result(TaskOutcome::Passed, vec![unattributed]);
        let findings = apply_result_policy(&mut result, &suite(Vec::new()), STRICT);
        assert_eq!(findings.len(), 1);
        assert_eq!(result.outcome, TaskOutcome::Failed);
    }

    /// P10: a test that passed after a retry stays flaky and fails by policy.
    #[test]
    fn p10_flaky_tests_are_kept_and_fail_by_policy() {
        let cases = vec![case("a.test.ts", &["sometimes"], CaseStatus::Flaky)];
        let mut strict = result(TaskOutcome::Flaky, cases.clone());
        let findings = apply_result_policy(&mut strict, &suite(Vec::new()), STRICT);
        assert_eq!(strict.outcome, TaskOutcome::Failed);
        assert_eq!(findings[0].code, FLAKY_TESTS);
        assert_eq!(strict.counts.unwrap().flaky, 1);

        let mut lenient = result(TaskOutcome::Flaky, cases);
        let findings = apply_result_policy(&mut lenient, &suite(Vec::new()), LENIENT);
        assert_eq!(
            lenient.outcome,
            TaskOutcome::Flaky,
            "flaky is never reported as passed"
        );
        assert_eq!(findings.len(), 1);
        assert!(!findings[0].fails_suite);
    }

    #[test]
    fn lenient_policies_report_without_failing() {
        let mut result = result(
            TaskOutcome::Passed,
            vec![
                case("a.test.ts", &["ok"], CaseStatus::Passed),
                case("a.test.ts", &["later"], CaseStatus::Todo),
            ],
        );
        let findings = apply_result_policy(&mut result, &suite(Vec::new()), LENIENT);
        assert_eq!(result.outcome, TaskOutcome::Passed);
        assert_eq!(findings.len(), 1);
        assert!(!findings[0].fails_suite);
    }

    #[test]
    fn a_policy_never_improves_an_outcome() {
        for outcome in [
            TaskOutcome::Failed,
            TaskOutcome::TimedOut,
            TaskOutcome::Cancelled,
            TaskOutcome::InfrastructureError,
        ] {
            let mut result = result(
                outcome,
                vec![case("a.test.ts", &["later"], CaseStatus::Skipped)],
            );
            apply_result_policy(&mut result, &suite(Vec::new()), STRICT);
            assert_eq!(result.outcome, outcome);
        }
    }
}
