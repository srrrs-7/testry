//! Result assembly and aggregation (spec section 18: P08, P09, P12 and the
//! forbidden states of section 4).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use testry_domain::fingerprint::Fingerprint;
use testry_domain::ids::{BaselineId, RunId, SourceSnapshotId, TaskId, WorkspaceId};
use testry_domain::paths::RepoDir;
use testry_domain::plan::{
    CandidatePlan, CapabilityVerification, PlanDraft, SelectionReason, SuitePlan,
};
use testry_domain::resources::{ResourceBudget, ResourceRequest};
use testry_domain::selection::OmitReason;
use testry_domain::suite::RunnerKind;

use super::*;
use crate::ports::execution::{
    ArtifactRef, ProcessNotes, ReportFormat, RunMetrics, WorkspaceMapping,
};

fn path(raw: &str) -> RepoPath {
    RepoPath::try_new(raw).unwrap()
}

fn suite_id(raw: &str) -> SuiteId {
    SuiteId::try_new(raw).unwrap()
}

fn invocation(expected: Option<&[&str]>) -> PreparedInvocation {
    PreparedInvocation {
        suite_id: suite_id("api"),
        task_id: TaskId::try_new("api").unwrap(),
        requested_selection: Selection::All,
        effective_selection: Selection::All,
        widened_because: None,
        argv: vec!["bun".to_owned(), "test".to_owned()],
        env: BTreeMap::new(),
        cwd: RepoDir::root(),
        workspace_root: PathBuf::from("/workspace"),
        expected_files: expected.map(|files| files.iter().map(|file| path(file)).collect()),
        report_format: ReportFormat::BunJunitXml,
        report_path: Some(PathBuf::from("/artifacts/tasks/api/report.xml")),
        additional_files: AdditionalFiles::Unexpected,
    }
}

fn process(exit: ProcessExit) -> ProcessResult {
    ProcessResult {
        exit,
        started_at: SystemTime::UNIX_EPOCH,
        finished_at: SystemTime::UNIX_EPOCH,
        duration: Duration::from_millis(120),
        stdout: ArtifactRef {
            relative_path: PathBuf::from("tasks/api/stdout.log"),
        },
        stderr: ArtifactRef {
            relative_path: PathBuf::from("tasks/api/stderr.log"),
        },
        notes: ProcessNotes::default(),
    }
}

fn exited(code: i32) -> ProcessResult {
    process(ProcessExit::Exited { code })
}

fn case(file: &str, title: &str, status: CaseStatus) -> CaseResult {
    CaseResult {
        project: None,
        file: Some(path(file)),
        title_path: vec![title.to_owned()],
        status,
        failure: None,
    }
}

fn parsed(cases: Vec<CaseResult>) -> ReportOutcome {
    let files = cases.iter().filter_map(|case| case.file.clone()).collect();
    ReportOutcome::Parsed(ParsedReport { files, cases })
}

fn codes(result: &SuiteResult) -> Vec<&'static str> {
    result
        .anomalies
        .iter()
        .map(|anomaly| anomaly.code)
        .collect()
}

#[test]
fn a_clean_run_passes_with_counts_and_files() {
    let report = parsed(vec![
        case("a.test.ts", "one", CaseStatus::Passed),
        case("a.test.ts", "two", CaseStatus::Passed),
    ]);
    let result = assemble_suite_result(&invocation(Some(&["a.test.ts"])), &exited(0), report);
    assert_eq!(result.outcome, TaskOutcome::Passed);
    assert_eq!(
        result.counts,
        Some(TestCounts {
            passed: 2,
            failed: 0,
            skipped: 0,
            flaky: 0
        })
    );
    assert_eq!(
        result.executed_files,
        FileAttribution::Known(BTreeSet::from([path("a.test.ts")]))
    );
    assert_eq!(result.attempts, 1);
    assert!(result.anomalies.is_empty());
    assert_eq!(result.duration, Duration::from_millis(120));
}

/// P08: exit code 0 without a report is an infrastructure error, not a pass.
#[test]
fn p08_exit_zero_without_report_is_an_infrastructure_error() {
    let missing = ReportOutcome::content_problem(&ReportContent::Missing).unwrap();
    let result = assemble_suite_result(&invocation(None), &exited(0), missing.clone());
    assert_eq!(result.outcome, TaskOutcome::InfrastructureError);
    assert_eq!(codes(&result), [anomaly::REPORT_MISSING]);
    assert_eq!(result.counts, None);
    assert_eq!(result.executed_files, FileAttribution::Unknown);

    // The same holds for a failing exit code: nothing is known without a report.
    let failing = assemble_suite_result(&invocation(None), &exited(1), missing);
    assert_eq!(failing.outcome, TaskOutcome::InfrastructureError);
    assert!(failing.anomalies[0].detail.contains("exit code 1"));
}

/// P14: an empty, broken or oversized report fails safely.
#[test]
fn p14_unusable_reports_are_infrastructure_errors() {
    let problems = [
        ReportOutcome::invalid("not well-formed"),
        ReportOutcome::content_problem(&ReportContent::TooLarge { limit_bytes: 10 }).unwrap(),
        ReportOutcome::content_problem(&ReportContent::Unreadable {
            message: "denied".to_owned(),
        })
        .unwrap(),
    ];
    for problem in problems {
        let result = assemble_suite_result(&invocation(None), &exited(0), problem);
        assert_eq!(result.outcome, TaskOutcome::InfrastructureError);
        assert_eq!(result.anomalies.len(), 1);
    }
    assert_eq!(
        ReportOutcome::content_problem(&ReportContent::Bytes(Vec::new())),
        None
    );
}

/// P09: a requested file that is missing from the report is not a pass.
#[test]
fn p09_expected_file_missing_from_the_report() {
    let report = || parsed(vec![case("a.test.ts", "one", CaseStatus::Passed)]);
    let expected = invocation(Some(&["a.test.ts", "b.test.ts"]));

    let with_zero_exit = assemble_suite_result(&expected, &exited(0), report());
    assert_eq!(with_zero_exit.outcome, TaskOutcome::InfrastructureError);
    assert_eq!(codes(&with_zero_exit), [anomaly::EXPECTED_FILES_MISSING]);
    assert!(with_zero_exit.anomalies[0].detail.contains("b.test.ts"));

    // The runner itself reported a failure (for example an import error in
    // the missing file): the suite failed.
    let with_failure_exit = assemble_suite_result(&expected, &exited(1), report());
    assert_eq!(with_failure_exit.outcome, TaskOutcome::Failed);
    assert_eq!(
        codes(&with_failure_exit),
        [
            anomaly::EXPECTED_FILES_MISSING,
            anomaly::NONZERO_EXIT_WITHOUT_FAILURE
        ]
    );
}

#[test]
fn failures_decide_whatever_the_exit_code_says() {
    let report = || {
        parsed(vec![
            case("a.test.ts", "ok", CaseStatus::Passed),
            case("a.test.ts", "broken", CaseStatus::Failed),
        ])
    };
    let failed = assemble_suite_result(&invocation(None), &exited(1), report());
    assert_eq!(failed.outcome, TaskOutcome::Failed);
    assert!(failed.anomalies.is_empty());
    assert_eq!(failed.counts.unwrap().failed, 1);

    let inconsistent = assemble_suite_result(&invocation(None), &exited(0), report());
    assert_eq!(inconsistent.outcome, TaskOutcome::Failed);
    assert_eq!(codes(&inconsistent), [anomaly::FAILURE_WITH_ZERO_EXIT]);
}

#[test]
fn a_nonzero_exit_is_never_a_pass() {
    let report = parsed(vec![case("a.test.ts", "ok", CaseStatus::Passed)]);
    let result = assemble_suite_result(&invocation(None), &exited(1), report);
    assert_eq!(result.outcome, TaskOutcome::Failed);
    assert_eq!(codes(&result), [anomaly::NONZERO_EXIT_WITHOUT_FAILURE]);
}

#[test]
fn zero_executed_tests_are_never_a_pass() {
    let empty = assemble_suite_result(&invocation(None), &exited(0), parsed(Vec::new()));
    assert_eq!(empty.outcome, TaskOutcome::InfrastructureError);
    assert_eq!(codes(&empty), [anomaly::NO_TESTS_EXECUTED]);

    let only_skipped = assemble_suite_result(
        &invocation(None),
        &exited(0),
        parsed(vec![
            case("a.test.ts", "later", CaseStatus::Skipped),
            case("a.test.ts", "todo", CaseStatus::Todo),
        ]),
    );
    assert_eq!(only_skipped.outcome, TaskOutcome::Skipped);
    assert_eq!(only_skipped.counts.unwrap().skipped, 2);
}

#[test]
fn flaky_is_not_passed() {
    let report = parsed(vec![
        case("a.test.ts", "ok", CaseStatus::Passed),
        case("a.test.ts", "sometimes", CaseStatus::Flaky),
    ]);
    let result = assemble_suite_result(&invocation(None), &exited(0), report);
    assert_eq!(result.outcome, TaskOutcome::Flaky);
    assert_eq!(
        result.counts.unwrap(),
        TestCounts {
            passed: 1,
            failed: 0,
            skipped: 0,
            flaky: 1
        }
    );
}

#[test]
fn timeout_cancellation_and_signals_are_distinct() {
    let report = || parsed(vec![case("a.test.ts", "ok", CaseStatus::Passed)]);
    let timed_out =
        assemble_suite_result(&invocation(None), &process(ProcessExit::TimedOut), report());
    assert_eq!(timed_out.outcome, TaskOutcome::TimedOut);
    let cancelled = assemble_suite_result(
        &invocation(None),
        &process(ProcessExit::Cancelled),
        report(),
    );
    assert_eq!(cancelled.outcome, TaskOutcome::Cancelled);
    let signaled = assemble_suite_result(
        &invocation(None),
        &process(ProcessExit::Signaled { signal: 9 }),
        report(),
    );
    assert_eq!(signaled.outcome, TaskOutcome::InfrastructureError);
    assert_eq!(codes(&signaled), [anomaly::TERMINATED_BY_SIGNAL]);
}

/// P12: the command adapter knows success or failure, never a test count.
#[test]
fn p12_command_results_have_unknown_counts() {
    let passed = assemble_suite_result(&invocation(None), &exited(0), ReportOutcome::NotApplicable);
    assert_eq!(passed.outcome, TaskOutcome::Passed);
    assert_eq!(passed.counts, None);
    assert_eq!(passed.executed_files, FileAttribution::Unknown);
    assert!(passed.cases.is_empty());

    let failed = assemble_suite_result(&invocation(None), &exited(2), ReportOutcome::NotApplicable);
    assert_eq!(failed.outcome, TaskOutcome::Failed);
}

#[test]
fn running_more_files_than_requested_is_recorded_not_punished() {
    let report = parsed(vec![
        case("a.test.ts", "ok", CaseStatus::Passed),
        case("a.test.tsx", "ok", CaseStatus::Passed),
    ]);
    let exact = assemble_suite_result(
        &invocation(Some(&["a.test.ts"])),
        &exited(0),
        report.clone(),
    );
    assert_eq!(exact.outcome, TaskOutcome::Passed);
    assert_eq!(codes(&exact), [anomaly::UNEXPECTED_FILES]);

    let mut superset = invocation(Some(&["a.test.ts"]));
    superset.additional_files = AdditionalFiles::Expected;
    let result = assemble_suite_result(&superset, &exited(0), report);
    assert_eq!(result.outcome, TaskOutcome::Passed);
    assert!(result.anomalies.is_empty());
}

#[test]
fn process_notes_become_anomalies() {
    let mut process = exited(0);
    process.notes = ProcessNotes {
        stdout_truncated: true,
        stderr_truncated: false,
        stopped_survivors: true,
        residual_processes: true,
        output_incomplete: true,
    };
    let mut widened = invocation(None);
    widened.widened_because = Some("test files outside the suite directory".to_owned());
    let result = assemble_suite_result(
        &widened,
        &process,
        parsed(vec![case("a", "ok", CaseStatus::Passed)]),
    );
    assert_eq!(
        codes(&result),
        [
            anomaly::SELECTION_WIDENED,
            anomaly::OUTPUT_TRUNCATED,
            anomaly::OUTPUT_INCOMPLETE,
            anomaly::SURVIVORS_STOPPED,
            anomaly::RESIDUAL_PROCESSES,
        ]
    );
}

// ------------------------------------------------------------ aggregation --

fn suite_run(id: &str, outcome: TaskOutcome, cases: Vec<CaseResult>) -> SuiteRunResult {
    let executed_files = if cases.is_empty() {
        FileAttribution::Unknown
    } else {
        FileAttribution::Known(cases.iter().filter_map(|case| case.file.clone()).collect())
    };
    SuiteRunResult {
        result: SuiteResult {
            suite_id: suite_id(id),
            task_id: TaskId::try_new(id).unwrap(),
            outcome,
            executed_files,
            counts: (!cases.is_empty()).then(|| TestCounts::from_cases(&cases)),
            attempts: 1,
            cases,
            anomalies: Vec::new(),
            duration: Duration::from_millis(40),
            artifacts: Vec::new(),
        },
        effective_selection: Selection::All,
        runner: RunnerKind::Bun,
        findings: Vec::new(),
        report: Some(ArtifactRef {
            relative_path: PathBuf::from(format!("tasks/{id}/report.xml")),
        }),
    }
}

fn clean() -> CleanupReport {
    CleanupReport::default()
}

fn outcome_of(suites: &[SuiteRunResult]) -> RunOutcome {
    aggregate_run_outcome(suites, false, InputValidation::Unchanged, &clean())
}

#[test]
fn run_outcome_follows_the_priority_order() {
    use TaskOutcome::*;
    let suite = |outcome| suite_run("s", outcome, Vec::new());
    assert_eq!(
        outcome_of(&[suite(Passed), suite(Passed)]),
        RunOutcome::Passed
    );
    assert_eq!(
        outcome_of(&[suite(Passed), suite(Failed)]),
        RunOutcome::Failed
    );
    assert_eq!(
        outcome_of(&[suite(Failed), suite(InfrastructureError)]),
        RunOutcome::InfrastructureError
    );
    assert_eq!(
        outcome_of(&[suite(Failed), suite(TimedOut)]),
        RunOutcome::InfrastructureError
    );
    assert_eq!(
        outcome_of(&[suite(InfrastructureError), suite(Cancelled)]),
        RunOutcome::Cancelled
    );
    // Flaky is visible on the suite; the run is not a failure unless a policy made it one.
    assert_eq!(outcome_of(&[suite(Flaky)]), RunOutcome::Passed);
}

/// P03: nothing executed, or everything skipped, is no work and not a pass.
#[test]
fn p03_nothing_verified_is_no_work() {
    assert_eq!(outcome_of(&[]), RunOutcome::NoWork);
    assert_eq!(
        outcome_of(&[suite_run("s", TaskOutcome::Skipped, Vec::new())]),
        RunOutcome::NoWork
    );
    assert_eq!(exit_code_of(RunOutcome::NoWork), 4);
    assert_eq!(exit_code_of(RunOutcome::Passed), 0);
}

/// R08: a run whose cleanup failed is never a complete success.
#[test]
fn r08_failed_cleanup_prevents_a_passed_run() {
    let suites = [suite_run("s", TaskOutcome::Passed, Vec::new())];
    let residual = CleanupReport {
        residual_resources: vec!["process-group:1234".to_owned()],
        ..CleanupReport::default()
    };
    let outcome = aggregate_run_outcome(&suites, false, InputValidation::Unchanged, &residual);
    assert_eq!(outcome, RunOutcome::InfrastructureError);
    assert_eq!(exit_code_of(outcome), 3);
}

/// P13: changed inputs invalidate the run whatever the tests said.
#[test]
fn p13_changed_inputs_invalidate_the_run() {
    let suites = [suite_run("s", TaskOutcome::Passed, Vec::new())];
    let outcome = aggregate_run_outcome(&suites, false, InputValidation::Changed, &clean());
    assert_eq!(outcome, RunOutcome::Invalidated);
    assert_eq!(exit_code_of(outcome), 3);

    // A cancellation still wins.
    assert_eq!(
        aggregate_run_outcome(&suites, true, InputValidation::Changed, &clean()),
        RunOutcome::Cancelled
    );
    assert_eq!(exit_code_of(RunOutcome::Cancelled), 130);
}

fn plan(mode: Mode, scope_limited: bool, candidates: &[(&str, Selection)]) -> ExecutionPlan {
    let suites = candidates
        .iter()
        .map(|(id, candidate)| SuitePlan {
            suite_id: suite_id(id),
            runner: RunnerKind::Bun,
            project: None,
            requested_selection: Selection::All,
            effective_selection: Selection::All,
            reasons: vec![SelectionReason::FullVerification],
            candidate: (mode == Mode::Shadow).then(|| CandidatePlan {
                selection: candidate.clone(),
                reasons: Vec::new(),
            }),
            capability_verification: CapabilityVerification::NotRequired,
            resources: ResourceRequest::try_new(1, 256, []).unwrap(),
        })
        .collect();
    ExecutionPlan::try_new(PlanDraft {
        workspace_id: WorkspaceId::try_new("ws").unwrap(),
        snapshot_id: SourceSnapshotId::new(Fingerprint::of_bytes(b"s")),
        baseline_commit: mode
            .needs_baseline()
            .then(|| BaselineId::try_new("a".repeat(40)).unwrap()),
        config_digest: Fingerprint::of_bytes(b"c"),
        mode,
        scope_limited,
        changed_paths: mode.needs_baseline().then(Vec::new),
        suites,
        fallbacks: Vec::new(),
        resources: ResourceBudget::try_new(2, 1024, 1).unwrap(),
        diagnostics: Vec::new(),
    })
    .unwrap()
}

#[test]
fn full_suite_assurance_needs_a_completed_unscoped_full_run() {
    let full = plan(Mode::Full, false, &[("api", Selection::All)]);
    let unchanged = InputValidation::Unchanged;
    assert_eq!(
        achieved_assurance(&full, RunOutcome::Passed, unchanged, &clean(), &[]),
        Assurance::FullSuite
    );
    // A failed run still verified the full scope.
    assert_eq!(
        achieved_assurance(&full, RunOutcome::Failed, unchanged, &clean(), &[]),
        Assurance::FullSuite
    );

    for outcome in [
        RunOutcome::Cancelled,
        RunOutcome::InfrastructureError,
        RunOutcome::Invalidated,
        RunOutcome::NoWork,
    ] {
        assert_eq!(
            achieved_assurance(&full, outcome, unchanged, &clean(), &[]),
            Assurance::Advisory
        );
    }
    assert_eq!(
        achieved_assurance(
            &full,
            RunOutcome::Passed,
            InputValidation::NotChecked,
            &clean(),
            &[]
        ),
        Assurance::Advisory
    );
    let scoped = plan(Mode::Full, true, &[("api", Selection::All)]);
    assert_eq!(
        achieved_assurance(&scoped, RunOutcome::Passed, unchanged, &clean(), &[]),
        Assurance::Advisory
    );
}

/// A suite that its own runner configuration narrowed did not run in full,
/// so the run does not claim the full scope even though every suite passed.
#[test]
fn a_suite_narrowed_by_its_runner_configuration_is_not_full_suite() {
    let full = plan(Mode::Full, false, &[("e2e", Selection::All)]);
    let complete = suite_run("e2e", TaskOutcome::Passed, Vec::new());
    assert_eq!(
        achieved_assurance(
            &full,
            RunOutcome::Passed,
            InputValidation::Unchanged,
            &clean(),
            std::slice::from_ref(&complete)
        ),
        Assurance::FullSuite
    );

    let mut narrowed = complete;
    narrowed.result.anomalies.push(Anomaly {
        code: anomaly::SCOPE_LIMITED_BY_RUNNER_CONFIG,
        detail: "the runner configuration sets a shard".to_owned(),
    });
    assert_eq!(
        achieved_assurance(
            &full,
            RunOutcome::Passed,
            InputValidation::Unchanged,
            &clean(),
            &[narrowed]
        ),
        Assurance::Advisory
    );
}

/// P06: a failure outside the affected candidate is recorded as a gap candidate.
#[test]
fn p06_shadow_records_failures_outside_the_candidate() {
    let candidate = Selection::files([path("api/a.test.ts")]).unwrap();
    let shadow = plan(
        Mode::Shadow,
        false,
        &[
            ("api", candidate),
            (
                "web",
                Selection::Omit {
                    reason: OmitReason::NoAffectedTests,
                },
            ),
            ("e2e", Selection::All),
            (
                "gate",
                Selection::Omit {
                    reason: OmitReason::NoAffectedTests,
                },
            ),
        ],
    );
    let suites = [
        suite_run(
            "api",
            TaskOutcome::Failed,
            vec![
                case("api/a.test.ts", "inside candidate", CaseStatus::Failed),
                case("api/b.test.ts", "outside candidate", CaseStatus::Failed),
                case("api/c.test.ts", "passing", CaseStatus::Passed),
            ],
        ),
        suite_run(
            "web",
            TaskOutcome::Failed,
            vec![case("web/w.test.ts", "omitted", CaseStatus::Failed)],
        ),
        suite_run(
            "e2e",
            TaskOutcome::Failed,
            vec![case("e2e/x.spec.ts", "whole suite", CaseStatus::Failed)],
        ),
        // A failure without file attribution is reported for the suite.
        suite_run("gate", TaskOutcome::Failed, Vec::new()),
    ];
    let gaps = shadow_gaps(&shadow, &suites).unwrap();
    assert_eq!(
        gaps,
        vec![
            ShadowGap {
                suite_id: suite_id("api"),
                file: Some(path("api/b.test.ts"))
            },
            ShadowGap {
                suite_id: suite_id("gate"),
                file: None
            },
            ShadowGap {
                suite_id: suite_id("web"),
                file: Some(path("web/w.test.ts"))
            },
        ]
    );

    let passing = [suite_run(
        "api",
        TaskOutcome::Passed,
        vec![case("api/b.test.ts", "ok", CaseStatus::Passed)],
    )];
    assert_eq!(shadow_gaps(&shadow, &passing), Some(Vec::new()));
    assert_eq!(
        shadow_gaps(
            &plan(Mode::Full, false, &[("api", Selection::All)]),
            &suites
        ),
        None
    );
}

#[test]
fn the_result_document_matches_the_contract() {
    let descriptor = ExecutionDescriptor {
        provider: ProviderKind::Host,
        os: "linux".to_owned(),
        arch: "aarch64".to_owned(),
        tool_versions: BTreeMap::from([("bun".to_owned(), "1.4.4".to_owned())]),
        workspace: WorkspaceMapping {
            local_root: PathBuf::from("/workspace"),
            execution_root: PathBuf::from("/workspace"),
        },
        image: None,
        capabilities: BTreeSet::new(),
    };
    let mut command = suite_run("typecheck", TaskOutcome::Passed, Vec::new());
    command.runner = RunnerKind::Command;
    command.report = None;
    let result = RunResult {
        run_id: RunId::try_new("123e4567-e89b-42d3-a456-426614174000").unwrap(),
        plan_id: *plan(Mode::Full, false, &[("api", Selection::All)]).id(),
        mode: Mode::Full,
        scope: vec![suite_id("api"), suite_id("typecheck")],
        suites: vec![
            suite_run(
                "api",
                TaskOutcome::Passed,
                vec![case("api/a.test.ts", "ok", CaseStatus::Passed)],
            ),
            command,
        ],
        assurance: Assurance::FullSuite,
        outcome: RunOutcome::Passed,
        input_validation: InputValidation::Unchanged,
        environment: Some(descriptor.clone()),
        shadow_gaps: None,
        metrics: RunMetrics {
            planning_ms: 12,
            total_ms: 315,
            ..RunMetrics::default()
        },
        cleanup: clean(),
        exit_code: 0,
    };
    let json = serde_json::to_value(RunResultDocument::from(&result)).unwrap();

    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["run_id"], "123e4567-e89b-42d3-a456-426614174000");
    assert!(json["plan_id"].as_str().unwrap().starts_with("blake3:"));
    assert_eq!(json["mode"], "full");
    assert_eq!(json["assurance"], "full_suite");
    assert_eq!(json["outcome"], "passed");
    assert_eq!(json["exit_code"], 0);
    assert_eq!(json["input_validation"], "unchanged");
    assert_eq!(json["environment"]["provider"], "host");
    assert_eq!(json["environment"]["tool_versions"]["bun"], "1.4.4");
    assert!(
        json["environment"]["fingerprint"]
            .as_str()
            .unwrap()
            .starts_with("blake3:")
    );
    assert_eq!(json["shadow"], serde_json::Value::Null);
    assert_eq!(json["metrics"]["first_failure_ms"], serde_json::Value::Null);
    assert_eq!(json["metrics"]["total_ms"], 315);
    assert_eq!(
        json["cleanup"],
        serde_json::json!({ "outcome": "succeeded", "residual_resources": [] })
    );

    let api = &json["suites"][0];
    assert_eq!(api["suite_id"], "api");
    assert_eq!(api["outcome"], "passed");
    assert_eq!(api["executed_files"], serde_json::json!(["api/a.test.ts"]));
    assert_eq!(api["file_attribution"], "known");
    assert_eq!(
        api["counts"],
        serde_json::json!({ "passed": 1, "failed": 0, "skipped": 0, "flaky": 0 })
    );
    assert_eq!(api["attempts"], 1);
    assert_eq!(api["report"], "tasks/api/report.xml");
    assert_eq!(api["duration_ms"], 40);

    // P12: unknown counts and files are explicit, never zero or an empty list.
    let gate = &json["suites"][1];
    assert_eq!(gate["runner"], "command");
    assert_eq!(gate["counts"], serde_json::Value::Null);
    assert_eq!(gate["executed_files"], serde_json::Value::Null);
    assert_eq!(gate["file_attribution"], "unknown");
    assert_eq!(gate["report"], serde_json::Value::Null);

    // The fingerprint follows the tool versions.
    let mut other = descriptor.clone();
    other
        .tool_versions
        .insert("bun".to_owned(), "1.4.5".to_owned());
    assert_ne!(
        environment_fingerprint(&descriptor),
        environment_fingerprint(&other)
    );
}
