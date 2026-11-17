//! Acceptance tests of the execution use case with fake ports
//! (spec section 18: P05, P06, P08-P13, R06, R08, R13 and the forbidden
//! states of section 4). No process is started here; the real process control
//! and the real runners are covered in `testry-adapters` and `testry-cli`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use testry_application::commands::execute_plan::{ExecutePlan, RunReport, RunRequest};
use testry_application::commands::plan_workspace::{PlanPurpose, PlanRequest, PlanWorkspace};
use testry_application::ports::execution::{
    CleanupReport, InputValidation, ReleaseDisposition, ReportContent, RunEventPayload,
    RunnerAdapter, ShadowGap, SuiteRunResult, TrustApproval,
};
use testry_application::ports::{Cancellation, OperationContext, PortError};
use testry_application::results::anomaly;
use testry_application::settings::Settings;
use testry_application::settings::test_support::{settings_parts, suite_parts};
use testry_application::testing::execution::{FakeArtifacts, FakeRunner, FakeRuntime};
use testry_application::testing::{
    FakeDependencyAnalyzer, FakeSourceRepository, InMemoryRunStore, RecordingEventSink,
    StaticCatalog, context,
};
use testry_domain::environment::ProviderKind;
use testry_domain::fingerprint::Fingerprint;
use testry_domain::graph::DependencyGraph;
use testry_domain::ids::{
    BaselineId, EnvironmentLeaseId, ExclusiveKey, RunnerProject, SuiteId, WorkspaceId,
};
use testry_domain::paths::RepoPath;
use testry_domain::plan::Mode;
use testry_domain::resources::ResourceRequest;
use testry_domain::results::{Assurance, ProcessExit, RunOutcome, TaskOutcome};
use testry_domain::selection::Selection;
use testry_domain::snapshot::{
    ChangeSource, CurrentFile, FileKind, SourceSnapshot, SourceSnapshotParts,
};
use testry_domain::suite::{KnownSkip, RunnerKind, SkipStatus, SuiteDefinition};

fn path(raw: &str) -> RepoPath {
    RepoPath::try_new(raw).unwrap()
}

fn suite_id(raw: &str) -> SuiteId {
    SuiteId::try_new(raw).unwrap()
}

const FILES: &[&str] = &[
    "apps/api/user.test.ts",
    "apps/api/order.test.ts",
    "apps/api/user.ts",
    "apps/api/order.ts",
    "apps/web/App.test.tsx",
    "apps/web/App.tsx",
    "testry.toml",
];

fn snapshot_with(changed: &[&str], revision: &str) -> SourceSnapshot {
    let current: BTreeMap<RepoPath, CurrentFile> = FILES
        .iter()
        .map(|name| {
            let content = format!("{name}@{revision}");
            (
                path(name),
                CurrentFile {
                    kind: FileKind::File,
                    digest: Fingerprint::of_bytes(content.as_bytes()),
                },
            )
        })
        .collect();
    SourceSnapshot::new(
        WorkspaceId::try_new("ws-fixture").unwrap(),
        SourceSnapshotParts {
            baseline: Some(BaselineId::try_new("c".repeat(40)).unwrap()),
            changes: changed
                .iter()
                .map(|name| (path(name), BTreeSet::from([ChangeSource::Unstaged])))
                .collect(),
            current,
            ..SourceSnapshotParts::default()
        },
    )
}

fn snapshot(changed: &[&str]) -> SourceSnapshot {
    snapshot_with(changed, "r1")
}

fn graph() -> DependencyGraph {
    let mut builder = DependencyGraph::builder();
    for (consumer, dependency) in [
        ("apps/api/user.test.ts", "apps/api/user.ts"),
        ("apps/api/order.test.ts", "apps/api/order.ts"),
        ("apps/web/App.test.tsx", "apps/web/App.tsx"),
    ] {
        builder.edge(path(consumer), path(dependency));
    }
    builder.build()
}

fn api_suite() -> SuiteDefinition {
    SuiteDefinition::try_new(suite_parts(
        "api-unit",
        RunnerKind::Bun,
        &["apps/api/**/*.test.ts"],
    ))
    .unwrap()
}

fn web_suite() -> SuiteDefinition {
    SuiteDefinition::try_new(suite_parts(
        "web-unit",
        RunnerKind::Vitest,
        &["apps/web/**/*.test.tsx"],
    ))
    .unwrap()
}

fn gate_suite() -> SuiteDefinition {
    let mut parts = suite_parts("typecheck", RunnerKind::Command, &[]);
    parts.always_run = true;
    SuiteDefinition::try_new(parts).unwrap()
}

const API_PASSING: &str =
    "apps/api/user.test.ts|user|passed\napps/api/order.test.ts|order|passed\n";
const WEB_PASSING: &str = "apps/web/App.test.tsx|app|passed\n";

struct Harness {
    settings: Settings,
    source: FakeSourceRepository,
    analyzer: FakeDependencyAnalyzer,
    runtime: FakeRuntime,
    artifacts: FakeArtifacts,
    events: RecordingEventSink,
    store: InMemoryRunStore,
    bun: FakeRunner,
    vitest: FakeRunner,
    command: FakeRunner,
    trust: Option<TrustApproval>,
}

impl Harness {
    fn new(snapshots: Vec<SourceSnapshot>) -> Self {
        let settings =
            Settings::try_new(settings_parts(vec![api_suite(), web_suite(), gate_suite()]))
                .unwrap();
        let trust = Some(TrustApproval {
            approved_config_digest: *settings.config_digest(),
            allow_initialize_command: false,
        });
        Self {
            settings,
            source: FakeSourceRepository::sequence(snapshots),
            analyzer: FakeDependencyAnalyzer::returning(graph(), graph()),
            runtime: FakeRuntime::default(),
            artifacts: FakeArtifacts::default()
                .report("api-unit", API_PASSING)
                .report("web-unit", WEB_PASSING),
            events: RecordingEventSink::default(),
            store: InMemoryRunStore::default(),
            bun: FakeRunner::verified(RunnerKind::Bun),
            vitest: FakeRunner::verified(RunnerKind::Vitest),
            command: FakeRunner::command(),
            trust,
        }
    }

    /// A workspace whose inputs never change.
    fn stable(changed: &[&str]) -> Self {
        Self::new(vec![snapshot(changed)])
    }

    fn with_suites(mut self, suites: Vec<SuiteDefinition>) -> Self {
        self.settings = Settings::try_new(settings_parts(suites)).unwrap();
        self.trust = Some(TrustApproval {
            approved_config_digest: *self.settings.config_digest(),
            allow_initialize_command: false,
        });
        self
    }

    async fn run_in(
        &self,
        request: PlanRequest,
        ctx: &OperationContext,
    ) -> Result<RunReport, PortError> {
        let use_case = ExecutePlan {
            planning: PlanWorkspace {
                settings: &self.settings,
                source: &self.source,
                analyzer: &self.analyzer,
                catalog: &StaticCatalog,
            },
            runners: BTreeMap::from([
                (RunnerKind::Bun, &self.bun as &dyn RunnerAdapter),
                (RunnerKind::Vitest, &self.vitest as &dyn RunnerAdapter),
                (RunnerKind::Command, &self.command as &dyn RunnerAdapter),
            ]),
            runtime: &self.runtime,
            artifacts: &self.artifacts,
            events: &self.events,
            store: &self.store,
        };
        let request = RunRequest {
            plan: request,
            provider: ProviderKind::Host,
            readiness_commands: Vec::new(),
            lease_id: EnvironmentLeaseId::try_new("11111111-1111-4111-8111-111111111111").unwrap(),
            trust: self.trust.clone(),
        };
        use_case
            .execute(Path::new("/workspace"), &request, ctx)
            .await
    }

    async fn run(&self, request: PlanRequest) -> Result<RunReport, PortError> {
        self.run_in(request, &context()).await
    }

    fn event_types(&self) -> Vec<&'static str> {
        self.events
            .events()
            .iter()
            .map(|event| event.payload.event_type())
            .collect()
    }
}

fn request(purpose: PlanPurpose, mode: Mode) -> PlanRequest {
    PlanRequest {
        purpose,
        mode,
        base_ref: None,
        scope: None,
        ci: false,
    }
}

fn affected() -> PlanRequest {
    request(PlanPurpose::Test, Mode::Affected)
}

fn full() -> PlanRequest {
    request(PlanPurpose::Verify, Mode::Full)
}

fn suite<'a>(report: &'a RunReport, id: &str) -> &'a SuiteRunResult {
    report
        .result
        .suites
        .iter()
        .find(|suite| suite.result.suite_id.as_str() == id)
        .unwrap_or_else(|| panic!("suite {id} has no result"))
}

fn anomaly_codes(suite: &SuiteRunResult) -> Vec<&'static str> {
    suite
        .result
        .anomalies
        .iter()
        .map(|anomaly| anomaly.code)
        .collect()
}

#[tokio::test]
async fn a_full_run_executes_every_suite_and_claims_full_suite_assurance() {
    let harness = Harness::stable(&[]);
    let report = harness.run(full()).await.unwrap();

    assert_eq!(report.result.outcome, RunOutcome::Passed);
    assert_eq!(report.result.exit_code, 0);
    assert_eq!(report.result.assurance, Assurance::FullSuite);
    assert_eq!(report.result.input_validation, InputValidation::Unchanged);
    assert_eq!(
        harness.runtime.started_suites(),
        ["api-unit", "typecheck", "web-unit"]
    );
    assert_eq!(suite(&report, "api-unit").result.counts.unwrap().passed, 2);
    assert_eq!(suite(&report, "typecheck").result.counts, None);
    assert!(report.result.cleanup.is_complete());
    assert_eq!(harness.runtime.released(), [ReleaseDisposition::Completed]);
    assert!(harness.store.unfinished().is_empty());

    // The observed runner versions are recorded with the result.
    let tools = &report.result.environment.as_ref().unwrap().tool_versions;
    assert_eq!(tools.get("bun").map(String::as_str), Some("1.0.0"));

    assert_eq!(
        harness.event_types(),
        [
            "run_started",
            "plan_created",
            "environment_preparing",
            "environment_ready",
            "task_started",
            "task_finished",
            "task_started",
            "task_finished",
            "task_started",
            "task_finished",
            "cleanup_started",
            "cleanup_finished",
            "run_finished",
        ]
    );
}

#[tokio::test]
async fn an_affected_run_executes_only_what_the_plan_selected() {
    let harness = Harness::stable(&["apps/api/order.ts"]);
    let report = harness.run(affected()).await.unwrap();

    assert_eq!(report.result.outcome, RunOutcome::Passed);
    assert_eq!(
        report.result.assurance,
        Assurance::Advisory,
        "a narrowed run is never a gate"
    );
    // web-unit has no affected test; typecheck always runs.
    assert_eq!(harness.runtime.started_suites(), ["api-unit", "typecheck"]);
    let api_command = &harness.runtime.tasks()[0];
    assert_eq!(api_command.args, ["test", "apps/api/order.test.ts"]);
    assert_eq!(
        suite(&report, "api-unit").effective_selection,
        Selection::files([path("apps/api/order.test.ts")]).unwrap()
    );
}

/// P03: a run without work is `no_work` (exit 4), and nothing is prepared.
#[tokio::test]
async fn p03_no_work_prepares_no_environment_and_is_not_a_pass() {
    let harness = Harness::stable(&[]).with_suites(vec![api_suite(), web_suite()]);
    let report = harness.run(affected()).await.unwrap();
    assert_eq!(report.result.outcome, RunOutcome::NoWork);
    assert_eq!(report.result.exit_code, 4);
    assert!(harness.runtime.prepared().is_empty());
    assert!(harness.runtime.tasks().is_empty());
    assert_eq!(
        harness.event_types(),
        ["run_started", "plan_created", "run_finished"]
    );
    assert!(harness.store.unfinished().is_empty());
}

/// P05: an unverified runner version runs the whole suite and says so.
#[tokio::test]
async fn p05_unverified_runner_version_widens_to_the_whole_suite() {
    let mut harness = Harness::stable(&["apps/api/order.ts"]);
    harness.bun.capabilities.file_selection = false;
    let report = harness.run(affected()).await.unwrap();

    assert_eq!(
        suite(&report, "api-unit").effective_selection,
        Selection::All
    );
    assert_eq!(
        harness.runtime.tasks()[0].args,
        ["test"],
        "no file argument may be passed"
    );
    let expanded = harness
        .events
        .events()
        .into_iter()
        .find_map(|event| match event.payload {
            RunEventPayload::SelectionExpanded {
                suite_id,
                from,
                to,
                reason_code,
            } => Some((suite_id, from, to, reason_code)),
            _ => None,
        })
        .expect("a selection_expanded event");
    assert_eq!(
        expanded,
        (
            suite_id("api-unit"),
            "files",
            "all",
            "unsupported_runner_version".to_owned()
        )
    );

    // A version that cannot be read is treated the same way.
    let mut unknown = Harness::stable(&["apps/api/order.ts"]);
    unknown.artifacts.version_output = String::new();
    let report = unknown.run(affected()).await.unwrap();
    assert_eq!(
        suite(&report, "api-unit").effective_selection,
        Selection::All
    );
    assert!(unknown.events.events().iter().any(|event| matches!(
        &event.payload,
        RunEventPayload::SelectionExpanded { reason_code, .. } if reason_code == "runner_version_unknown"
    )));
}

/// P06: shadow executes everything once and records failures outside the candidate.
#[tokio::test]
async fn p06_shadow_runs_everything_once_and_records_gap_candidates() {
    let mut harness = Harness::stable(&["apps/api/order.ts"]);
    harness.artifacts = FakeArtifacts::default()
        .report(
            "api-unit",
            "apps/api/user.test.ts|user|failed\napps/api/order.test.ts|order|passed\n",
        )
        .report("web-unit", WEB_PASSING);
    harness.runtime = FakeRuntime::default().exit("api-unit", ProcessExit::Exited { code: 1 });

    let report = harness
        .run(request(PlanPurpose::Verify, Mode::Shadow))
        .await
        .unwrap();
    assert_eq!(report.result.outcome, RunOutcome::Failed);
    assert_eq!(report.result.assurance, Assurance::FullSuite);
    // Every suite ran exactly once, in full.
    assert_eq!(
        harness.runtime.started_suites(),
        ["api-unit", "typecheck", "web-unit"]
    );
    assert_eq!(harness.runtime.tasks()[0].args, ["test"]);
    // user.test.ts failed, but the affected candidate only held order.test.ts.
    assert_eq!(
        report.result.shadow_gaps,
        Some(vec![ShadowGap {
            suite_id: suite_id("api-unit"),
            file: Some(path("apps/api/user.test.ts"))
        }])
    );
}

/// P08: exit code 0 without a report is an infrastructure error.
#[tokio::test]
async fn p08_exit_zero_without_report_is_an_infrastructure_error() {
    let mut harness = Harness::stable(&[]);
    harness.artifacts = FakeArtifacts::default().report("web-unit", WEB_PASSING);
    let report = harness.run(full()).await.unwrap();

    assert_eq!(
        suite(&report, "api-unit").result.outcome,
        TaskOutcome::InfrastructureError
    );
    assert_eq!(
        anomaly_codes(suite(&report, "api-unit")),
        [anomaly::REPORT_MISSING]
    );
    assert_eq!(report.result.outcome, RunOutcome::InfrastructureError);
    assert_eq!(report.result.exit_code, 3);
    // The other suites keep their own results.
    assert_eq!(
        suite(&report, "web-unit").result.outcome,
        TaskOutcome::Passed
    );
}

/// P09: a planned file that is missing from the report is never a pass.
#[tokio::test]
async fn p09_planned_file_missing_from_the_report() {
    let mut harness = Harness::stable(&["apps/api/order.ts", "apps/api/user.ts"]);
    harness.artifacts =
        FakeArtifacts::default().report("api-unit", "apps/api/user.test.ts|user|passed\n");
    let report = harness.run(affected()).await.unwrap();
    let api = suite(&report, "api-unit");
    assert_eq!(api.result.outcome, TaskOutcome::InfrastructureError);
    assert_eq!(anomaly_codes(api), [anomaly::EXPECTED_FILES_MISSING]);
    assert_eq!(report.result.exit_code, 3);

    // In a full run the registered test files are what must show up.
    let mut full_run = Harness::stable(&[]);
    full_run.artifacts = FakeArtifacts::default()
        .report("api-unit", "apps/api/user.test.ts|user|passed\n")
        .report("web-unit", WEB_PASSING);
    let report = full_run.run(full()).await.unwrap();
    assert_eq!(
        suite(&report, "api-unit").result.outcome,
        TaskOutcome::InfrastructureError
    );
}

/// P10: new skips and flaky tests violate the default policies; an exactly
/// listed skip does not.
#[tokio::test]
async fn p10_skips_and_flaky_tests_follow_the_policy() {
    let mut harness = Harness::stable(&[]);
    harness.artifacts = FakeArtifacts::default()
        .report(
            "api-unit",
            "apps/api/user.test.ts|user|passed\napps/api/order.test.ts|later|skipped\n",
        )
        .report("web-unit", "apps/web/App.test.tsx|sometimes|flaky\n");
    let report = harness.run(full()).await.unwrap();

    let api = suite(&report, "api-unit");
    assert_eq!(api.result.outcome, TaskOutcome::Failed);
    assert_eq!(api.findings[0].code, "unexpected_skips");
    assert_eq!(
        api.result.counts.unwrap().skipped,
        1,
        "the skip stays visible"
    );
    let web = suite(&report, "web-unit");
    assert_eq!(web.result.outcome, TaskOutcome::Failed);
    assert_eq!(web.findings[0].code, "flaky_tests");
    assert_eq!(
        web.result.counts.unwrap().flaky,
        1,
        "flaky is never counted as passed"
    );
    assert_eq!(report.result.outcome, RunOutcome::Failed);
    assert_eq!(report.result.exit_code, 1);

    let mut approved = suite_parts("api-unit", RunnerKind::Bun, &["apps/api/**/*.test.ts"]);
    approved.known_skips = vec![KnownSkip {
        project: None,
        file: path("apps/api/order.test.ts"),
        title_path: vec!["later".to_owned()],
        expected_status: SkipStatus::Skipped,
    }];
    let mut harness = Harness::stable(&[]).with_suites(vec![
        SuiteDefinition::try_new(approved).unwrap(),
        web_suite(),
    ]);
    harness.artifacts = FakeArtifacts::default()
        .report(
            "api-unit",
            "apps/api/user.test.ts|user|passed\napps/api/order.test.ts|later|skipped\n",
        )
        .report("web-unit", WEB_PASSING);
    let report = harness.run(full()).await.unwrap();
    assert_eq!(report.result.outcome, RunOutcome::Passed);
    assert!(suite(&report, "api-unit").findings.is_empty());
}

/// P11: the same test file in two runner projects keeps two results.
#[tokio::test]
async fn p11_same_file_in_two_projects_does_not_collide() {
    let mut unit = suite_parts("web-unit", RunnerKind::Vitest, &["apps/web/**/*.test.tsx"]);
    unit.project = Some(RunnerProject::try_new("unit").unwrap());
    let mut browser = suite_parts(
        "web-browser",
        RunnerKind::Vitest,
        &["apps/web/**/*.test.tsx"],
    );
    browser.project = Some(RunnerProject::try_new("browser").unwrap());
    let mut harness = Harness::stable(&[]).with_suites(vec![
        SuiteDefinition::try_new(unit).unwrap(),
        SuiteDefinition::try_new(browser).unwrap(),
    ]);
    harness.artifacts = FakeArtifacts::default()
        .report("web-unit", "apps/web/App.test.tsx|renders|passed\n")
        .report("web-browser", "apps/web/App.test.tsx|renders|failed\n");
    harness.runtime = FakeRuntime::default().exit("web-browser", ProcessExit::Exited { code: 1 });

    let report = harness.run(full()).await.unwrap();
    assert_eq!(
        suite(&report, "web-unit").result.outcome,
        TaskOutcome::Passed
    );
    assert_eq!(
        suite(&report, "web-browser").result.outcome,
        TaskOutcome::Failed
    );
    assert_ne!(
        suite(&report, "web-unit").report,
        suite(&report, "web-browser").report
    );
}

/// P12: a command gate reports success without inventing test counts.
#[tokio::test]
async fn p12_command_gate_has_unknown_counts() {
    let harness = Harness::stable(&[]);
    let report = harness.run(full()).await.unwrap();
    let gate = suite(&report, "typecheck");
    assert_eq!(gate.result.outcome, TaskOutcome::Passed);
    assert_eq!(gate.result.counts, None);
    assert_eq!(gate.report, None);
}

/// P13: inputs that changed after planning invalidate the plan before any
/// test runs.
#[tokio::test]
async fn p13_inputs_changed_before_execution_invalidate_the_run() {
    let harness = Harness::new(vec![
        snapshot_with(&["apps/api/order.ts"], "r1"),
        snapshot_with(&["apps/api/order.ts"], "r2"),
    ]);
    let report = harness.run(affected()).await.unwrap();
    assert_eq!(report.result.outcome, RunOutcome::Invalidated);
    assert_eq!(report.result.exit_code, 3);
    assert_eq!(report.result.input_validation, InputValidation::Changed);
    assert!(
        harness.runtime.tasks().is_empty(),
        "no test may run on a stale plan"
    );
    // The environment that was prepared is still released.
    assert_eq!(harness.runtime.released().len(), 1);
    assert!(harness.store.unfinished().is_empty());
}

/// P13: inputs that changed while the tests ran invalidate their result.
#[tokio::test]
async fn p13_inputs_changed_during_execution_invalidate_the_result() {
    let harness = Harness::new(vec![
        snapshot_with(&["apps/api/order.ts"], "r1"),
        snapshot_with(&["apps/api/order.ts"], "r1"),
        snapshot_with(&["apps/api/order.ts"], "r2"),
    ]);
    let report = harness.run(affected()).await.unwrap();
    assert_eq!(harness.runtime.started_suites(), ["api-unit", "typecheck"]);
    assert_eq!(
        suite(&report, "api-unit").result.outcome,
        TaskOutcome::Passed
    );
    assert_eq!(
        report.result.outcome,
        RunOutcome::Invalidated,
        "a pass on changed inputs is not a pass"
    );
    assert_eq!(report.result.exit_code, 3);
    assert_eq!(report.result.assurance, Assurance::Advisory);
}

/// A cancellation source a test can trigger.
#[derive(Default)]
struct Trigger {
    cancelled: AtomicBool,
}

impl Cancellation for Trigger {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    fn cancelled<'a>(&'a self) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            while !self.is_cancelled() {
                tokio::task::yield_now().await;
            }
        })
    }
}

/// R06: after a cancellation no new suite starts, cleanup still runs and the
/// run ends as cancelled (exit 130).
#[tokio::test]
async fn r06_cancellation_stops_new_suites_and_still_cleans_up() {
    let trigger = Arc::new(Trigger::default());
    let mut harness = Harness::stable(&[]);
    // One suite at a time, so the cancellation arrives while suites are waiting.
    let mut parts = settings_parts(vec![api_suite(), web_suite(), gate_suite()]);
    parts.budget = testry_domain::resources::ResourceBudget::try_new(1, 4096, 1).unwrap();
    harness.settings = Settings::try_new(parts).unwrap();
    harness.trust = Some(TrustApproval {
        approved_config_digest: *harness.settings.config_digest(),
        allow_initialize_command: false,
    });
    let firing = Arc::clone(&trigger);
    harness.runtime =
        FakeRuntime::default().on_task(move |_| firing.cancelled.store(true, Ordering::SeqCst));

    let ctx = OperationContext {
        cancellation: trigger,
        ..context()
    };
    let report = harness.run_in(full(), &ctx).await.unwrap();

    assert_eq!(
        harness.runtime.started_suites(),
        ["api-unit"],
        "no suite may start after the cancellation"
    );
    assert_eq!(report.result.outcome, RunOutcome::Cancelled);
    assert_eq!(report.result.exit_code, 130);
    assert_eq!(report.result.assurance, Assurance::Advisory);
    assert_eq!(harness.runtime.released(), [ReleaseDisposition::Cancelled]);
    let types = harness.event_types();
    assert!(
        types.ends_with(&["cleanup_started", "cleanup_finished", "run_finished"]),
        "{types:?}"
    );
    assert!(
        harness.store.unfinished().is_empty(),
        "a cancelled run still gets its terminal record"
    );
}

/// R08: a failed cleanup after passing tests is not a successful run.
#[tokio::test]
async fn r08_failed_cleanup_is_never_a_passed_run() {
    let mut harness = Harness::stable(&[]);
    harness.runtime.release_report = Ok(CleanupReport {
        residual_resources: vec!["process-group:4242".to_owned()],
        ..CleanupReport::default()
    });
    let report = harness.run(full()).await.unwrap();
    assert!(
        report
            .result
            .suites
            .iter()
            .all(|suite| suite.result.outcome == TaskOutcome::Passed)
    );
    assert_eq!(report.result.outcome, RunOutcome::InfrastructureError);
    assert_eq!(report.result.exit_code, 3);
    assert_eq!(
        report.result.cleanup.residual_resources,
        ["process-group:4242"]
    );
    assert_eq!(report.result.assurance, Assurance::Advisory);

    // A release that errors is recorded the same way.
    let mut erroring = Harness::stable(&[]);
    erroring.runtime.release_report = Err(PortError::CleanupFailed {
        message: "timeout".to_owned(),
        residual_resources: Vec::new(),
    });
    let report = erroring.run(full()).await.unwrap();
    assert_eq!(report.result.outcome, RunOutcome::InfrastructureError);
    assert!(!report.result.cleanup.is_complete());
}

/// R13: a failing environment preparation is an environment failure; no test starts.
#[tokio::test]
async fn r13_environment_preparation_failure_starts_no_test() {
    let mut harness = Harness::stable(&[]);
    harness.runtime.prepare_error = Some(PortError::Spawn {
        program: "bun".to_owned(),
        message: "readiness command failed".to_owned(),
    });
    let error = harness.run(full()).await.unwrap_err();
    assert_eq!(error.exit_condition().exit_code(), 3);
    assert!(harness.runtime.tasks().is_empty());
    assert_eq!(harness.runtime.probes(), 0);

    // The run is recorded, and not as passed.
    let stored = harness.store.results();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].outcome, RunOutcome::InfrastructureError);
    assert_eq!(stored[0].exit_code, 3);
    assert!(stored[0].suites.is_empty());
}

#[tokio::test]
async fn nothing_runs_without_trust_for_exactly_this_configuration() {
    let mut untrusted = Harness::stable(&[]);
    untrusted.trust = None;
    let error = untrusted.run(full()).await.unwrap_err();
    assert_eq!(error.code(), "trust_required");
    assert_eq!(error.exit_condition().exit_code(), 2);
    assert!(untrusted.runtime.prepared().is_empty());
    assert!(untrusted.runtime.tasks().is_empty());

    // Trust for another configuration digest does not carry over.
    let mut outdated = Harness::stable(&[]);
    outdated.trust = Some(TrustApproval {
        approved_config_digest: Fingerprint::of_bytes(b"an earlier configuration"),
        allow_initialize_command: false,
    });
    let error = outdated.run(full()).await.unwrap_err();
    assert_eq!(error.code(), "trust_required");
    assert!(outdated.runtime.prepared().is_empty());

    // A run without work executes nothing, so it needs no trust.
    let mut idle = Harness::stable(&[]).with_suites(vec![api_suite(), web_suite()]);
    idle.trust = None;
    assert_eq!(
        idle.run(affected()).await.unwrap().result.outcome,
        RunOutcome::NoWork
    );
}

#[tokio::test]
async fn a_suite_without_adapter_is_refused_before_anything_runs() {
    let playwright = SuiteDefinition::try_new(suite_parts(
        "e2e",
        RunnerKind::Playwright,
        &["apps/web/**/*.test.tsx"],
    ))
    .unwrap();
    let harness = Harness::stable(&[]).with_suites(vec![api_suite(), playwright]);
    let error = harness.run(full()).await.unwrap_err();
    assert_eq!(error.code(), "unsupported_capability");
    assert_eq!(error.exit_condition().exit_code(), 2);
    assert!(harness.runtime.prepared().is_empty());
    assert!(harness.store.results().is_empty(), "no run was started");
}

/// A suite command that its adapter refuses is a problem of the whole
/// request. No suite starts, also not the ones that are fine, and the
/// environment is still released.
#[tokio::test]
async fn a_command_its_adapter_refuses_stops_the_run_before_any_suite_starts() {
    let mut harness = Harness::stable(&[]);
    // Only the adapter of the web suite refuses its command; the other
    // suites are fine and must not start either.
    harness.vitest.rejected_argument = Some("test");
    let error = harness.run(full()).await.unwrap_err();
    assert_eq!(error.code(), "configuration");
    assert_eq!(error.exit_condition().exit_code(), 2);
    assert!(
        harness.runtime.started_suites().is_empty(),
        "no suite may start: {:?}",
        harness.runtime.started_suites()
    );
    assert_eq!(harness.runtime.released(), [ReleaseDisposition::Failed]);

    // The run is recorded, and it is not a pass.
    let results = harness.store.results();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].outcome, RunOutcome::InfrastructureError);
    assert_eq!(results[0].exit_code, 2);
    assert!(results[0].suites.is_empty());
    assert_eq!(results[0].assurance, Assurance::Advisory);
}

#[tokio::test]
async fn a_failing_suite_fails_the_run_and_keeps_the_other_results() {
    let mut harness = Harness::stable(&[]);
    harness.artifacts = FakeArtifacts::default()
        .report(
            "api-unit",
            "apps/api/user.test.ts|user|failed\napps/api/order.test.ts|order|passed\n",
        )
        .report("web-unit", WEB_PASSING);
    harness.runtime = FakeRuntime::default().exit("api-unit", ProcessExit::Exited { code: 1 });
    let report = harness.run(full()).await.unwrap();

    assert_eq!(report.result.outcome, RunOutcome::Failed);
    assert_eq!(report.result.exit_code, 1);
    // A failed run still verified the full scope.
    assert_eq!(report.result.assurance, Assurance::FullSuite);
    assert_eq!(
        suite(&report, "web-unit").result.outcome,
        TaskOutcome::Passed
    );
    assert_eq!(
        suite(&report, "typecheck").result.outcome,
        TaskOutcome::Passed
    );
    assert!(report.result.metrics.first_failure_ms.is_some());
    assert_eq!(harness.runtime.released(), [ReleaseDisposition::Completed]);
}

#[tokio::test]
async fn fail_fast_starts_no_further_suite_after_a_failure() {
    let mut parts = settings_parts(vec![api_suite(), web_suite(), gate_suite()]);
    parts.fail_fast = true;
    parts.budget = testry_domain::resources::ResourceBudget::try_new(1, 4096, 1).unwrap();
    let mut harness = Harness::stable(&[]);
    harness.settings = Settings::try_new(parts).unwrap();
    harness.trust = Some(TrustApproval {
        approved_config_digest: *harness.settings.config_digest(),
        allow_initialize_command: false,
    });
    harness.artifacts =
        FakeArtifacts::default().report("api-unit", "apps/api/user.test.ts|user|failed\n");
    harness.runtime = FakeRuntime::default().exit("api-unit", ProcessExit::Exited { code: 1 });

    let report = harness.run(full()).await.unwrap();
    assert_eq!(harness.runtime.started_suites(), ["api-unit"]);
    assert_eq!(report.result.outcome, RunOutcome::Failed);
    assert_eq!(report.result.suites.len(), 1);
    assert_eq!(
        report.result.assurance,
        Assurance::FullSuite,
        "the outcome is failed either way"
    );
}

#[tokio::test]
async fn timeouts_and_spawn_failures_are_infrastructure_errors() {
    let mut timed_out = Harness::stable(&[]);
    timed_out.runtime = FakeRuntime::default().exit("api-unit", ProcessExit::TimedOut);
    let report = timed_out.run(full()).await.unwrap();
    assert_eq!(
        suite(&report, "api-unit").result.outcome,
        TaskOutcome::TimedOut
    );
    assert_eq!(report.result.outcome, RunOutcome::InfrastructureError);
    assert_eq!(report.result.exit_code, 3);

    let mut missing = Harness::stable(&[]);
    missing.runtime = FakeRuntime::default().spawn_failure("api-unit");
    let report = missing.run(full()).await.unwrap();
    let api = suite(&report, "api-unit");
    assert_eq!(api.result.outcome, TaskOutcome::InfrastructureError);
    assert_eq!(anomaly_codes(api), [anomaly::SPAWN_FAILED]);
    assert_eq!(api.result.attempts, 0);
    assert_eq!(report.result.exit_code, 3);
}

/// P14: an unusable report is an infrastructure error, whatever the exit code.
#[tokio::test]
async fn p14_unusable_reports_fail_safely() {
    for content in [
        ReportContent::Bytes(b"not|a|valid|report|line".to_vec()),
        ReportContent::Bytes(Vec::new()),
        ReportContent::TooLarge { limit_bytes: 1024 },
        ReportContent::Unreadable {
            message: "permission denied".to_owned(),
        },
    ] {
        let mut harness = Harness::stable(&[]);
        harness.artifacts = FakeArtifacts::default()
            .report_content("api-unit", content.clone())
            .report("web-unit", WEB_PASSING);
        let report = harness.run(full()).await.unwrap();
        assert_eq!(
            suite(&report, "api-unit").result.outcome,
            TaskOutcome::InfrastructureError,
            "{content:?}"
        );
        assert_eq!(report.result.exit_code, 3);
    }
}

/// R02 / R03: the scheduler keeps suites with a shared exclusive key apart
/// and stays within the budget while the others run in parallel.
#[tokio::test]
async fn r02_r03_suites_respect_exclusive_keys_and_the_budget() {
    let exclusive = |id: &str, runner, include: &[&str]| {
        let mut parts = suite_parts(id, runner, include);
        parts.resources =
            ResourceRequest::try_new(1, 512, [ExclusiveKey::try_new("test-database").unwrap()])
                .unwrap();
        SuiteDefinition::try_new(parts).unwrap()
    };
    let mut harness = Harness::stable(&[]).with_suites(vec![
        exclusive("api-unit", RunnerKind::Bun, &["apps/api/**/*.test.ts"]),
        exclusive("web-unit", RunnerKind::Vitest, &["apps/web/**/*.test.tsx"]),
    ]);
    harness.runtime.task_duration = Duration::from_millis(20);
    let report = harness.run(full()).await.unwrap();
    assert_eq!(report.result.outcome, RunOutcome::Passed);
    assert_eq!(
        harness.runtime.max_parallel_tasks(),
        1,
        "suites sharing a key must not overlap"
    );

    // Without the shared key the same suites run together (budget: 2 at a time).
    let mut parallel = Harness::stable(&[]).with_suites(vec![api_suite(), web_suite()]);
    parallel.runtime.task_duration = Duration::from_millis(20);
    parallel.run(full()).await.unwrap();
    assert_eq!(parallel.runtime.max_parallel_tasks(), 2);
}

#[tokio::test]
async fn a_dev_container_lease_runs_its_suites_one_at_a_time() {
    let mut harness = Harness::stable(&[]).with_suites(vec![api_suite(), web_suite()]);
    harness.runtime.provider = ProviderKind::DevcontainerCli;
    harness.runtime.task_duration = Duration::from_millis(10);
    harness.run(full()).await.unwrap();
    assert_eq!(harness.runtime.max_parallel_tasks(), 1);
}

#[tokio::test]
async fn request_rules_are_checked_before_anything_is_recorded() {
    let harness = Harness::stable(&[]);
    let ci_affected = PlanRequest {
        ci: true,
        ..affected()
    };
    let error = harness.run(ci_affected).await.unwrap_err();
    assert_eq!(error.exit_condition().exit_code(), 2);
    assert!(harness.events.events().is_empty());
    assert!(harness.store.results().is_empty());
    assert!(harness.source.requests().is_empty());
}
