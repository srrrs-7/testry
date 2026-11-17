//! Outcomes and exit codes (spec sections 4 and 14).
//!
//! Outcomes are kept distinct on purpose: a skipped, flaky, cancelled or
//! unverified result is never folded into `passed`, and "nothing to run" is
//! never the same as "verified".

use serde::{Deserialize, Serialize};

/// How a child process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProcessExit {
    /// The process exited on its own with this status code.
    Exited { code: i32 },
    /// The process was terminated by a signal testry did not send for a
    /// timeout or cancellation.
    Signaled { signal: i32 },
    /// testry stopped the process because it exceeded its timeout.
    TimedOut,
    /// testry stopped the process because the run was cancelled.
    Cancelled,
}

impl ProcessExit {
    pub fn is_success(self) -> bool {
        matches!(self, Self::Exited { code: 0 })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskOutcome {
    Passed,
    Failed,
    Skipped,
    Flaky,
    TimedOut,
    Cancelled,
    InfrastructureError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    Passed,
    Failed,
    /// Nothing had to be executed. Not a successful verification.
    NoWork,
    /// Inputs changed while the run was in progress; the result does not
    /// describe the current code.
    Invalidated,
    Cancelled,
    InfrastructureError,
}

/// What a result is allowed to claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Assurance {
    /// A narrowed or scope-limited run. Useful feedback, not a quality gate.
    Advisory,
    /// Every registered suite was executed in full. This states that the
    /// registered verification scope was completed, nothing more.
    FullSuite,
}

/// Aggregated test case counts. Absent (`None` at the use site) when a runner
/// cannot report them; counts are never invented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
pub struct TestCounts {
    pub passed: u64,
    pub failed: u64,
    pub skipped: u64,
    pub flaky: u64,
}

impl TestCounts {
    /// Number of test cases that were actually executed.
    pub fn executed(&self) -> u64 {
        self.passed
            .saturating_add(self.failed)
            .saturating_add(self.flaky)
    }

    /// Counts cases by status. `todo` cases and expected failures count as
    /// skipped: neither verified a passing behaviour. A case that passed only
    /// after a retry counts as flaky, not as passed.
    pub fn from_cases<'a>(cases: impl IntoIterator<Item = &'a CaseResult>) -> Self {
        let mut counts = Self::default();
        for case in cases {
            match case.status {
                CaseStatus::Passed => counts.passed += 1,
                CaseStatus::Failed => counts.failed += 1,
                CaseStatus::Skipped | CaseStatus::Todo | CaseStatus::ExpectedFailure => {
                    counts.skipped += 1;
                }
                CaseStatus::Flaky => counts.flaky += 1,
            }
        }
        counts
    }
}

/// The status of one test case as the runner reported it. The statuses are
/// kept apart: nothing here is folded into `Passed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseStatus {
    Passed,
    Failed,
    Skipped,
    Todo,
    /// Failed at least once and passed on a retry of the runner.
    Flaky,
    /// Declared as failing in the test code, and it failed. It ran, but it
    /// verified nothing; like a skip it needs an explicit approval.
    ExpectedFailure,
}

/// One test case of a structured runner report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseResult {
    /// The project (for example the browser) the runner reports for this
    /// case, when one invocation runs several. `None` when the whole
    /// invocation is one project, which is then the project of the suite.
    pub project: Option<String>,
    /// The test file, when the runner attributes cases to files.
    pub file: Option<crate::paths::RepoPath>,
    /// Enclosing group titles followed by the test title. Empty for a problem
    /// that concerns a whole file (for example an import error).
    pub title_path: Vec<String>,
    pub status: CaseStatus,
    /// A bounded failure message. Never used to classify the result.
    pub failure: Option<String>,
}

/// The reasons a command can end with, ordered from lowest to highest
/// priority. When several apply, the highest one decides the exit code; the
/// individual task results are kept unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ExitCondition {
    Success,
    NoWork,
    TestOrPolicyFailure,
    Configuration,
    Infrastructure,
    UserCancelled,
}

impl ExitCondition {
    pub fn exit_code(self) -> u8 {
        match self {
            Self::Success => 0,
            Self::TestOrPolicyFailure => 1,
            Self::Configuration => 2,
            Self::Infrastructure => 3,
            Self::NoWork => 4,
            Self::UserCancelled => 130,
        }
    }

    /// The condition with the highest priority; `Success` for an empty input.
    pub fn dominant(conditions: impl IntoIterator<Item = Self>) -> Self {
        conditions.into_iter().max().unwrap_or(Self::Success)
    }
}

impl From<RunOutcome> for ExitCondition {
    fn from(outcome: RunOutcome) -> Self {
        match outcome {
            RunOutcome::Passed => Self::Success,
            RunOutcome::Failed => Self::TestOrPolicyFailure,
            RunOutcome::NoWork => Self::NoWork,
            RunOutcome::Invalidated | RunOutcome::InfrastructureError => Self::Infrastructure,
            RunOutcome::Cancelled => Self::UserCancelled,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_match_the_cli_contract() {
        assert_eq!(ExitCondition::Success.exit_code(), 0);
        assert_eq!(ExitCondition::TestOrPolicyFailure.exit_code(), 1);
        assert_eq!(ExitCondition::Configuration.exit_code(), 2);
        assert_eq!(ExitCondition::Infrastructure.exit_code(), 3);
        assert_eq!(ExitCondition::NoWork.exit_code(), 4);
        assert_eq!(ExitCondition::UserCancelled.exit_code(), 130);
    }

    #[test]
    fn priority_is_cancel_infra_config_failure_no_work_success() {
        use ExitCondition::*;
        let ordered = [
            Success,
            NoWork,
            TestOrPolicyFailure,
            Configuration,
            Infrastructure,
            UserCancelled,
        ];
        for pair in ordered.windows(2) {
            assert!(
                pair[0] < pair[1],
                "{:?} must rank below {:?}",
                pair[0],
                pair[1]
            );
        }
        assert_eq!(
            ExitCondition::dominant([TestOrPolicyFailure, Infrastructure, NoWork]),
            Infrastructure
        );
        assert_eq!(
            ExitCondition::dominant([Configuration, UserCancelled]),
            UserCancelled
        );
        assert_eq!(ExitCondition::dominant([NoWork, Success]), NoWork);
        assert_eq!(ExitCondition::dominant([]), Success);
    }

    /// P03: no work is not a pass.
    #[test]
    fn p03_no_work_is_distinct_from_passed() {
        assert_ne!(RunOutcome::NoWork, RunOutcome::Passed);
        assert_eq!(ExitCondition::from(RunOutcome::NoWork).exit_code(), 4);
        assert_eq!(ExitCondition::from(RunOutcome::Passed).exit_code(), 0);
    }

    #[test]
    fn invalidated_and_cancelled_runs_are_not_successes() {
        assert_eq!(ExitCondition::from(RunOutcome::Invalidated).exit_code(), 3);
        assert_eq!(
            ExitCondition::from(RunOutcome::InfrastructureError).exit_code(),
            3
        );
        assert_eq!(ExitCondition::from(RunOutcome::Cancelled).exit_code(), 130);
        assert_eq!(ExitCondition::from(RunOutcome::Failed).exit_code(), 1);
    }

    #[test]
    fn only_exit_zero_is_a_process_success() {
        assert!(ProcessExit::Exited { code: 0 }.is_success());
        assert!(!ProcessExit::Exited { code: 1 }.is_success());
        assert!(!ProcessExit::Signaled { signal: 9 }.is_success());
        assert!(!ProcessExit::TimedOut.is_success());
        assert!(!ProcessExit::Cancelled.is_success());
    }

    #[test]
    fn outcomes_serialize_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&TaskOutcome::InfrastructureError).unwrap(),
            "\"infrastructure_error\""
        );
        assert_eq!(
            serde_json::to_string(&RunOutcome::NoWork).unwrap(),
            "\"no_work\""
        );
        assert_eq!(
            serde_json::to_string(&Assurance::FullSuite).unwrap(),
            "\"full_suite\""
        );
        assert_eq!(
            serde_json::to_string(&ProcessExit::TimedOut).unwrap(),
            "{\"kind\":\"timed_out\"}"
        );
    }

    #[test]
    fn skipped_tests_are_not_counted_as_executed() {
        let counts = TestCounts {
            passed: 3,
            failed: 1,
            skipped: 5,
            flaky: 2,
        };
        assert_eq!(counts.executed(), 6);
    }
}
