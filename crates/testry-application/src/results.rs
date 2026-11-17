//! From process and report to suite result, and from suite results to the
//! outcome of a run (spec sections 4, 8, 9 and 16). Pure functions.
//!
//! The rules never turn missing evidence into a pass:
//! - exit code 0 without a readable report is an infrastructure error;
//! - exit code 0 with a requested file missing from the report is an
//!   infrastructure error;
//! - a non-zero exit code is never a pass, even when the report lists no
//!   failure;
//! - zero executed tests are never a pass.

use std::collections::BTreeSet;
use std::time::Duration;

use serde::Serialize;
use testry_domain::environment::ProviderKind;
use testry_domain::fingerprint::{Fingerprint, FingerprintBuilder};
use testry_domain::ids::SuiteId;
use testry_domain::paths::RepoPath;
use testry_domain::plan::{ExecutionPlan, Mode, PLAN_SCHEMA_VERSION};
use testry_domain::results::{
    Assurance, CaseResult, CaseStatus, ExitCondition, ProcessExit, RunOutcome, TaskOutcome,
    TestCounts,
};
use testry_domain::selection::Selection;

use crate::ports::execution::{
    AdditionalFiles, Anomaly, CleanupReport, ExecutionDescriptor, FileAttribution, InputValidation,
    PreparedInvocation, ProcessResult, ReportContent, RunResult, ShadowGap, SuiteResult,
    SuiteRunResult,
};

/// Stable anomaly codes.
pub mod anomaly {
    pub const REPORT_MISSING: &str = "report_missing";
    pub const REPORT_TOO_LARGE: &str = "report_too_large";
    pub const REPORT_UNREADABLE: &str = "report_unreadable";
    pub const REPORT_INVALID: &str = "report_invalid";
    pub const EXPECTED_FILES_MISSING: &str = "expected_files_missing";
    pub const UNEXPECTED_FILES: &str = "unexpected_files";
    pub const NONZERO_EXIT_WITHOUT_FAILURE: &str = "nonzero_exit_without_reported_failure";
    pub const FAILURE_WITH_ZERO_EXIT: &str = "reported_failure_with_zero_exit";
    pub const NO_TESTS_EXECUTED: &str = "no_tests_executed";
    pub const TERMINATED_BY_SIGNAL: &str = "terminated_by_signal";
    pub const SPAWN_FAILED: &str = "spawn_failed";
    pub const SELECTION_WIDENED: &str = "selection_widened";
    pub const SURVIVORS_STOPPED: &str = "survivors_stopped";
    pub const RESIDUAL_PROCESSES: &str = "residual_processes";
    pub const OUTPUT_TRUNCATED: &str = "output_truncated";
    pub const OUTPUT_INCOMPLETE: &str = "output_incomplete";
    /// The configuration of the runner itself narrows what the suite runs
    /// (for example a shard). The result stands, but it does not cover the
    /// whole suite, so the run cannot claim `full_suite`.
    pub const SCOPE_LIMITED_BY_RUNNER_CONFIG: &str = "scope_limited_by_runner_config";
    /// The report contradicts itself (for example its totals do not match
    /// its cases).
    pub const REPORT_INCONSISTENT: &str = "report_inconsistent";
}

/// What a structured report says, after parsing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedReport {
    /// Every file the report mentions.
    pub files: BTreeSet<RepoPath>,
    pub cases: Vec<CaseResult>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportOutcome {
    Parsed(ParsedReport),
    /// The report is missing, unreadable, too large or invalid.
    Problem(Anomaly),
    /// The runner has no structured report (`command`).
    NotApplicable,
}

impl ReportOutcome {
    /// The problem a report content has before it is even parsed.
    pub fn content_problem(content: &ReportContent) -> Option<Self> {
        let problem = match content {
            ReportContent::Bytes(_) => return None,
            ReportContent::Missing => Anomaly {
                code: anomaly::REPORT_MISSING,
                detail: "the runner did not write its report".to_owned(),
            },
            ReportContent::TooLarge { limit_bytes } => Anomaly {
                code: anomaly::REPORT_TOO_LARGE,
                detail: format!("the report is larger than {limit_bytes} bytes"),
            },
            ReportContent::Unreadable { message } => Anomaly {
                code: anomaly::REPORT_UNREADABLE,
                detail: message.clone(),
            },
        };
        Some(Self::Problem(problem))
    }

    pub fn invalid(detail: impl Into<String>) -> Self {
        Self::Problem(Anomaly {
            code: anomaly::REPORT_INVALID,
            detail: detail.into(),
        })
    }
}

fn list(paths: impl IntoIterator<Item = impl std::fmt::Display>) -> String {
    const SHOWN: usize = 5;
    let all: Vec<String> = paths.into_iter().map(|path| path.to_string()).collect();
    let shown: Vec<&str> = all.iter().take(SHOWN).map(String::as_str).collect();
    if all.len() > SHOWN {
        format!("{} and {} more", shown.join(", "), all.len() - SHOWN)
    } else {
        shown.join(", ")
    }
}

/// Combines the exit status of a runner with what its report says.
pub fn assemble_suite_result(
    invocation: &PreparedInvocation,
    process: &ProcessResult,
    report: ReportOutcome,
) -> SuiteResult {
    let mut anomalies = Vec::new();
    let mut note = |code: &'static str, detail: String| anomalies.push(Anomaly { code, detail });

    if let Some(reason) = &invocation.widened_because {
        note(anomaly::SELECTION_WIDENED, reason.clone());
    }
    let notes = process.notes;
    if notes.stdout_truncated || notes.stderr_truncated {
        note(
            anomaly::OUTPUT_TRUNCATED,
            "log output exceeded the storage limit".to_owned(),
        );
    }
    if notes.output_incomplete {
        note(
            anomaly::OUTPUT_INCOMPLETE,
            "a process kept an output pipe open".to_owned(),
        );
    }
    if notes.stopped_survivors {
        note(
            anomaly::SURVIVORS_STOPPED,
            "the command left background processes behind; they were stopped".to_owned(),
        );
    }
    if notes.residual_processes {
        note(
            anomaly::RESIDUAL_PROCESSES,
            "processes of the command could not be confirmed gone".to_owned(),
        );
    }

    let mut cases = Vec::new();
    let mut executed_files = FileAttribution::Unknown;
    let mut counts = None;

    let outcome = match (process.exit, report) {
        (ProcessExit::TimedOut, _) => TaskOutcome::TimedOut,
        (ProcessExit::Cancelled, _) => TaskOutcome::Cancelled,
        (ProcessExit::Signaled { signal }, _) => {
            note(anomaly::TERMINATED_BY_SIGNAL, format!("signal {signal}"));
            TaskOutcome::InfrastructureError
        }
        (ProcessExit::Exited { code }, ReportOutcome::NotApplicable) => {
            if code == 0 {
                TaskOutcome::Passed
            } else {
                TaskOutcome::Failed
            }
        }
        (ProcessExit::Exited { code }, ReportOutcome::Problem(problem)) => {
            // Without a report nothing is known, whatever the exit code says.
            let detail = format!("{} (exit code {code})", problem.detail);
            note(problem.code, detail);
            TaskOutcome::InfrastructureError
        }
        (ProcessExit::Exited { code }, ReportOutcome::Parsed(parsed)) => {
            let tally = TestCounts::from_cases(&parsed.cases);
            let missing: Vec<&RepoPath> = invocation
                .expected_files
                .iter()
                .flatten()
                .filter(|file| !parsed.files.contains(*file))
                .collect();
            let extra: Vec<&RepoPath> = match &invocation.expected_files {
                Some(expected) if invocation.additional_files == AdditionalFiles::Unexpected => {
                    parsed
                        .files
                        .iter()
                        .filter(|file| !expected.contains(*file))
                        .collect()
                }
                _ => Vec::new(),
            };
            if !missing.is_empty() {
                note(
                    anomaly::EXPECTED_FILES_MISSING,
                    format!("not in the report: {}", list(&missing)),
                );
            }
            if !extra.is_empty() {
                // Running more than requested is allowed; it is only recorded.
                note(
                    anomaly::UNEXPECTED_FILES,
                    format!("also executed: {}", list(&extra)),
                );
            }

            let outcome = if code != 0 {
                if tally.failed == 0 {
                    note(
                        anomaly::NONZERO_EXIT_WITHOUT_FAILURE,
                        format!("exit code {code} although the report lists no failed test"),
                    );
                }
                TaskOutcome::Failed
            } else if tally.failed > 0 {
                note(
                    anomaly::FAILURE_WITH_ZERO_EXIT,
                    format!(
                        "{} failed test(s) although the exit code is 0",
                        tally.failed
                    ),
                );
                TaskOutcome::Failed
            } else if !missing.is_empty() {
                TaskOutcome::InfrastructureError
            } else if tally.executed() == 0 {
                if tally.skipped > 0 {
                    TaskOutcome::Skipped
                } else {
                    note(
                        anomaly::NO_TESTS_EXECUTED,
                        "the report contains no test".to_owned(),
                    );
                    TaskOutcome::InfrastructureError
                }
            } else if tally.flaky > 0 {
                TaskOutcome::Flaky
            } else {
                TaskOutcome::Passed
            };
            executed_files = FileAttribution::Known(parsed.files);
            counts = Some(tally);
            cases = parsed.cases;
            outcome
        }
    };

    SuiteResult {
        suite_id: invocation.suite_id.clone(),
        task_id: invocation.task_id.clone(),
        outcome,
        executed_files,
        counts,
        attempts: 1,
        cases,
        anomalies,
        duration: process.duration,
        artifacts: vec![process.stdout.clone(), process.stderr.clone()],
    }
}

/// A task that could not be started at all.
pub fn unstarted_suite_result(
    invocation: &PreparedInvocation,
    outcome: TaskOutcome,
    anomaly: Anomaly,
) -> SuiteResult {
    SuiteResult {
        suite_id: invocation.suite_id.clone(),
        task_id: invocation.task_id.clone(),
        outcome,
        executed_files: FileAttribution::Unknown,
        counts: None,
        attempts: 0,
        cases: Vec::new(),
        anomalies: vec![anomaly],
        duration: Duration::ZERO,
        artifacts: Vec::new(),
    }
}

/// Whether a task outcome stops further tasks under `fail_fast`.
pub fn is_failure(outcome: TaskOutcome) -> bool {
    matches!(
        outcome,
        TaskOutcome::Failed | TaskOutcome::TimedOut | TaskOutcome::InfrastructureError
    )
}

/// The outcome of a run. Nothing here can turn into `Passed` unless at least
/// one suite verified something and nothing went wrong around it.
pub fn aggregate_run_outcome(
    suites: &[SuiteRunResult],
    cancelled: bool,
    input_validation: InputValidation,
    cleanup: &CleanupReport,
) -> RunOutcome {
    let has =
        |wanted: fn(TaskOutcome) -> bool| suites.iter().any(|suite| wanted(suite.result.outcome));
    if cancelled || has(|outcome| outcome == TaskOutcome::Cancelled) {
        return RunOutcome::Cancelled;
    }
    if input_validation == InputValidation::Changed {
        return RunOutcome::Invalidated;
    }
    let infrastructure = has(|outcome| {
        matches!(
            outcome,
            TaskOutcome::InfrastructureError | TaskOutcome::TimedOut
        )
    });
    // A run whose cleanup failed is never a complete success.
    if infrastructure || !cleanup.is_complete() {
        return RunOutcome::InfrastructureError;
    }
    if has(|outcome| outcome == TaskOutcome::Failed) {
        return RunOutcome::Failed;
    }
    if has(|outcome| matches!(outcome, TaskOutcome::Passed | TaskOutcome::Flaky)) {
        RunOutcome::Passed
    } else {
        // Nothing ran, or everything that ran was skipped.
        RunOutcome::NoWork
    }
}

/// `full_suite` is only claimed when every registered suite was executed to
/// completion on unchanged inputs and the environment was cleaned up. The
/// tests may have failed; the claim is about the scope that was verified.
/// A suite whose runner configuration is known to narrow it did not run in
/// full, whatever testry asked for.
pub fn achieved_assurance(
    plan: &ExecutionPlan,
    outcome: RunOutcome,
    input_validation: InputValidation,
    cleanup: &CleanupReport,
    suites: &[SuiteRunResult],
) -> Assurance {
    let completed = matches!(outcome, RunOutcome::Passed | RunOutcome::Failed);
    let narrowed_by_runner = suites.iter().any(|suite| {
        suite
            .result
            .anomalies
            .iter()
            .any(|anomaly| anomaly.code == anomaly::SCOPE_LIMITED_BY_RUNNER_CONFIG)
    });
    if plan.assurance() == Assurance::FullSuite
        && completed
        && input_validation == InputValidation::Unchanged
        && cleanup.is_complete()
        && !narrowed_by_runner
    {
        Assurance::FullSuite
    } else {
        Assurance::Advisory
    }
}

/// Failures of a shadow run that the affected candidate would not have
/// executed. `None` when the plan is not a shadow plan.
pub fn shadow_gaps(plan: &ExecutionPlan, suites: &[SuiteRunResult]) -> Option<Vec<ShadowGap>> {
    if plan.mode() != Mode::Shadow {
        return None;
    }
    let mut gaps = BTreeSet::new();
    for suite in suites {
        let result = &suite.result;
        let candidate = plan
            .suite(&result.suite_id)
            .and_then(|suite_plan| suite_plan.candidate.as_ref())
            .map(|candidate| &candidate.selection);
        let Some(candidate) = candidate else {
            continue;
        };
        if matches!(candidate, Selection::All) {
            continue;
        }
        let covered = |file: &RepoPath| match candidate {
            Selection::All => true,
            Selection::Files { files } => files.contains(file),
            Selection::Omit { .. } => false,
        };
        let failed_files: BTreeSet<&RepoPath> = result
            .cases
            .iter()
            .filter(|case| case.status == CaseStatus::Failed)
            .filter_map(|case| case.file.as_ref())
            .collect();
        for file in failed_files {
            if !covered(file) {
                gaps.insert(ShadowGap {
                    suite_id: result.suite_id.clone(),
                    file: Some(file.clone()),
                });
            }
        }
        // A failure that cannot be attributed to a file is reported for the
        // whole suite instead of being dropped.
        let unattributed = is_failure(result.outcome)
            && !result
                .cases
                .iter()
                .any(|case| case.status == CaseStatus::Failed && case.file.is_some());
        if unattributed {
            gaps.insert(ShadowGap {
                suite_id: result.suite_id.clone(),
                file: None,
            });
        }
    }
    Some(gaps.into_iter().collect())
}

/// Fingerprint of the environment a run executed in.
pub fn environment_fingerprint(descriptor: &ExecutionDescriptor) -> Fingerprint {
    let mut fingerprint = FingerprintBuilder::new("testry.environment.v1");
    fingerprint.text("provider", descriptor.provider.as_str());
    fingerprint.text("os", &descriptor.os);
    fingerprint.text("arch", &descriptor.arch);
    for (tool, version) in &descriptor.tool_versions {
        fingerprint.text("tool", tool);
        fingerprint.text("version", version);
    }
    fingerprint.text("image", descriptor.image.as_deref().unwrap_or(""));
    fingerprint.finish()
}

// ------------------------------------------------------- result document --

/// The public result document (spec section 16, `schema_version` 1).
/// Paths are repository-relative, durations are integer milliseconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunResultDocument {
    pub schema_version: u32,
    pub run_id: String,
    pub plan_id: String,
    pub mode: Mode,
    pub scope: Vec<SuiteId>,
    pub assurance: Assurance,
    pub outcome: RunOutcome,
    pub exit_code: u8,
    pub input_validation: &'static str,
    pub environment: Option<EnvironmentDocument>,
    pub suites: Vec<SuiteDocument>,
    /// Present for shadow runs only.
    pub shadow: Option<ShadowDocument>,
    pub metrics: MetricsDocument,
    pub cleanup: CleanupDocument,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EnvironmentDocument {
    pub provider: ProviderKind,
    pub os: String,
    pub arch: String,
    pub fingerprint: Fingerprint,
    pub tool_versions: std::collections::BTreeMap<String, String>,
    pub image: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SuiteDocument {
    pub suite_id: SuiteId,
    pub runner: &'static str,
    pub outcome: TaskOutcome,
    pub selection: Selection,
    /// `known` or `unknown`. Unknown is never written as an empty list.
    pub file_attribution: &'static str,
    pub executed_files: Option<Vec<RepoPath>>,
    /// `null` when the runner cannot report counts.
    pub counts: Option<TestCounts>,
    pub attempts: u32,
    pub duration_ms: u64,
    pub report: Option<String>,
    pub anomalies: Vec<NoteDocument>,
    pub policy_findings: Vec<FindingDocument>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NoteDocument {
    pub code: &'static str,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FindingDocument {
    pub code: &'static str,
    pub detail: String,
    pub fails_suite: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShadowDocument {
    /// Failures outside the affected candidate. A hint, not a proof.
    pub gap_candidates: Vec<GapDocument>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GapDocument {
    pub suite_id: SuiteId,
    /// `null` when the failure cannot be attributed to a file.
    pub file: Option<RepoPath>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MetricsDocument {
    pub planning_ms: u64,
    pub environment_ready_ms: u64,
    pub first_failure_ms: Option<u64>,
    pub verification_ms: u64,
    pub cleanup_ms: u64,
    pub total_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CleanupDocument {
    pub outcome: &'static str,
    pub residual_resources: Vec<String>,
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

impl From<&RunResult> for RunResultDocument {
    fn from(result: &RunResult) -> Self {
        Self {
            schema_version: PLAN_SCHEMA_VERSION,
            run_id: result.run_id.to_string(),
            plan_id: result.plan_id.to_string(),
            mode: result.mode,
            scope: result.scope.clone(),
            assurance: result.assurance,
            outcome: result.outcome,
            exit_code: result.exit_code,
            input_validation: match result.input_validation {
                InputValidation::Unchanged => "unchanged",
                InputValidation::Changed => "changed",
                InputValidation::NotChecked => "not_checked",
            },
            environment: result
                .environment
                .as_ref()
                .map(|descriptor| EnvironmentDocument {
                    provider: descriptor.provider,
                    os: descriptor.os.clone(),
                    arch: descriptor.arch.clone(),
                    fingerprint: environment_fingerprint(descriptor),
                    tool_versions: descriptor.tool_versions.clone(),
                    image: descriptor.image.clone(),
                }),
            suites: result.suites.iter().map(suite_document).collect(),
            shadow: result.shadow_gaps.as_ref().map(|gaps| ShadowDocument {
                gap_candidates: gaps
                    .iter()
                    .map(|gap| GapDocument {
                        suite_id: gap.suite_id.clone(),
                        file: gap.file.clone(),
                    })
                    .collect(),
            }),
            metrics: MetricsDocument {
                planning_ms: result.metrics.planning_ms,
                environment_ready_ms: result.metrics.environment_ready_ms,
                first_failure_ms: result.metrics.first_failure_ms,
                verification_ms: result.metrics.verification_ms,
                cleanup_ms: result.metrics.cleanup_ms,
                total_ms: result.metrics.total_ms,
            },
            cleanup: CleanupDocument {
                outcome: if result.cleanup.is_complete() {
                    "succeeded"
                } else {
                    "failed"
                },
                residual_resources: result.cleanup.residual_resources.clone(),
            },
        }
    }
}

fn suite_document(suite: &SuiteRunResult) -> SuiteDocument {
    let result = &suite.result;
    let (file_attribution, executed_files) = match &result.executed_files {
        FileAttribution::Known(files) => ("known", Some(files.iter().cloned().collect())),
        FileAttribution::Unknown => ("unknown", None),
    };
    SuiteDocument {
        suite_id: result.suite_id.clone(),
        runner: suite.runner.as_str(),
        outcome: result.outcome,
        selection: suite.effective_selection.clone(),
        file_attribution,
        executed_files,
        counts: result.counts,
        attempts: result.attempts,
        duration_ms: millis(result.duration),
        report: suite
            .report
            .as_ref()
            .map(|report| report.relative_path.display().to_string()),
        anomalies: result
            .anomalies
            .iter()
            .map(|anomaly| NoteDocument {
                code: anomaly.code,
                detail: anomaly.detail.clone(),
            })
            .collect(),
        policy_findings: suite
            .findings
            .iter()
            .map(|finding| FindingDocument {
                code: finding.code,
                detail: finding.detail.clone(),
                fails_suite: finding.fails_suite,
            })
            .collect(),
    }
}

/// The exit code of a finished run.
pub fn exit_code_of(outcome: RunOutcome) -> u8 {
    ExitCondition::from(outcome).exit_code()
}

#[cfg(test)]
mod tests;
