//! Builders for the storage tests: plans, start records and results.

use std::path::Path;
use std::time::{Duration, SystemTime};

use testry_application::ports::execution::{
    CleanupReport, FileAttribution, InputValidation, RunMetrics, RunRecord, RunResult, RunStore,
    SuiteResult, SuiteRunResult,
};
use testry_domain::environment::ProviderKind;
use testry_domain::fingerprint::Fingerprint;
use testry_domain::ids::{
    EnvironmentLeaseId, RunId, SourceSnapshotId, SuiteId, TaskId, WorkspaceId,
};
use testry_domain::paths::RepoPath;
use testry_domain::plan::{
    CapabilityVerification, ExecutionPlan, Mode, PlanDraft, SelectionReason, SuitePlan,
};
use testry_domain::resources::{ResourceBudget, ResourceRequest};
use testry_domain::results::{Assurance, RunOutcome, TaskOutcome, TestCounts};
use testry_domain::selection::Selection;
use testry_domain::suite::RunnerKind;

use super::history::HistoryDatabase;
use super::run_files::RunDirectory;

pub(crate) fn storage() -> RepoPath {
    RepoPath::try_new(".testry").unwrap()
}

/// A run ID that differs in its last digits.
pub(crate) fn run_id(number: u32) -> RunId {
    RunId::try_new(format!("00000000-0000-4000-8000-{number:012x}")).unwrap()
}

/// A valid full plan. Different seeds give different plans.
pub(crate) fn plan(seed: &str) -> ExecutionPlan {
    ExecutionPlan::try_new(PlanDraft {
        workspace_id: WorkspaceId::try_new("ws-test").unwrap(),
        snapshot_id: SourceSnapshotId::new(Fingerprint::of_bytes(seed.as_bytes())),
        baseline_commit: None,
        config_digest: Fingerprint::of_bytes(b"config"),
        mode: Mode::Full,
        scope_limited: false,
        changed_paths: None,
        suites: vec![SuitePlan {
            suite_id: SuiteId::try_new("unit").unwrap(),
            runner: RunnerKind::Bun,
            project: None,
            requested_selection: Selection::All,
            effective_selection: Selection::All,
            reasons: vec![SelectionReason::FullVerification],
            candidate: None,
            capability_verification: CapabilityVerification::NotRequired,
            resources: ResourceRequest::try_new(1, 256, []).unwrap(),
        }],
        fallbacks: Vec::new(),
        resources: ResourceBudget::try_new(2, 1024, 1).unwrap(),
        diagnostics: Vec::new(),
    })
    .unwrap()
}

/// The lease of a run in these tests: the same last digits as the run.
pub(crate) fn lease_of(run: &RunId) -> EnvironmentLeaseId {
    let digits = run.as_str().rsplit('-').next().unwrap();
    EnvironmentLeaseId::try_new(format!("11111111-1111-4111-8111-{digits}")).unwrap()
}

pub(crate) fn record(run: &RunId, plan: &ExecutionPlan) -> RunRecord {
    RunRecord {
        run_id: run.clone(),
        plan_id: *plan.id(),
        plan: plan.clone(),
        started_at: SystemTime::now(),
        mode: plan.mode(),
        scope: plan.body().scope.clone(),
        workspace_id: plan.body().workspace_id.clone(),
        lease_id: lease_of(run),
        provider: ProviderKind::Host,
    }
}

/// The result of a run with one Bun suite and one command gate.
pub(crate) fn result(run: &RunId, plan: &ExecutionPlan, outcome: RunOutcome) -> RunResult {
    let suite = |id: &str, runner: RunnerKind, counts: Option<TestCounts>| SuiteRunResult {
        result: SuiteResult {
            suite_id: SuiteId::try_new(id).unwrap(),
            task_id: TaskId::try_new(id).unwrap(),
            outcome: TaskOutcome::Passed,
            executed_files: FileAttribution::Unknown,
            counts,
            attempts: 1,
            cases: Vec::new(),
            anomalies: Vec::new(),
            duration: Duration::from_millis(120),
            artifacts: Vec::new(),
        },
        effective_selection: Selection::All,
        runner,
        findings: Vec::new(),
        report: None,
    };
    RunResult {
        run_id: run.clone(),
        plan_id: *plan.id(),
        mode: plan.mode(),
        scope: plan.body().scope.clone(),
        suites: vec![
            suite(
                "unit",
                RunnerKind::Bun,
                Some(TestCounts {
                    passed: 3,
                    failed: 0,
                    skipped: 1,
                    flaky: 0,
                }),
            ),
            suite("typecheck", RunnerKind::Command, None),
        ],
        assurance: Assurance::FullSuite,
        outcome,
        input_validation: InputValidation::Unchanged,
        environment: None,
        shadow_gaps: None,
        metrics: RunMetrics {
            total_ms: 1500,
            ..RunMetrics::default()
        },
        cleanup: CleanupReport::default(),
        exit_code: testry_application::results::exit_code_of(outcome),
    }
}

/// A run that started: its directory with the start record and the plan.
pub(crate) async fn started_run(
    workspace: &Path,
    number: u32,
    history: Option<&HistoryDatabase>,
) -> (RunDirectory, RunId, ExecutionPlan) {
    let run = run_id(number);
    let plan = plan(&format!("snapshot-{number}"));
    let mut directory = RunDirectory::create(workspace, &storage(), &run, false).unwrap();
    if let Some(history) = history {
        directory = directory.with_history(history.clone());
    }
    directory.create(&record(&run, &plan)).await.unwrap();
    (directory, run, plan)
}

/// A run that started and wrote its terminal record.
pub(crate) async fn finished_run(
    workspace: &Path,
    number: u32,
    history: Option<&HistoryDatabase>,
) -> RunId {
    let (directory, run, plan) = started_run(workspace, number, history).await;
    directory
        .finish(&result(&run, &plan, RunOutcome::Passed))
        .await
        .unwrap();
    run
}
