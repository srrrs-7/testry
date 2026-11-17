//! Acceptance tests of the planning use case with fake ports
//! (spec section 18: P02-P07, P13 and the mode rules of section 8).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use testry_application::commands::create_plan::{
    PlanningError, apply_runner_capabilities, ensure_plan_is_current,
};
use testry_application::commands::plan_workspace::{
    PlanPurpose, PlanRequest, PlanWorkspace, PlanningOutput,
};
use testry_application::ports::planning::{ResolverProfile, SnapshotSide};
use testry_application::ports::{PortError, SourceUnavailableReason};
use testry_application::queries::explain::explain;
use testry_application::settings::Settings;
use testry_application::settings::test_support::{settings, settings_parts, suite, suite_parts};
use testry_application::testing::{
    FakeDependencyAnalyzer, FakeSourceRepository, StaticCatalog, cancelled_context, context,
};
use testry_domain::fingerprint::Fingerprint;
use testry_domain::graph::DependencyGraph;
use testry_domain::ids::{BaselineId, SuiteId, WorkspaceId};
use testry_domain::paths::RepoPath;
use testry_domain::plan::{CapabilityVerification, FallbackReason, Mode, SelectionReason};
use testry_domain::results::Assurance;
use testry_domain::selection::{OmitReason, Selection};
use testry_domain::snapshot::{
    ChangeSource, CurrentFile, FileKind, SourceSnapshot, SourceSnapshotParts,
};
use testry_domain::suite::{RunnerKind, SuiteDefinition};

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
    "packages/shared/date.ts",
    "tests/e2e/login.spec.ts",
    "testry.toml",
];

fn snapshot_with(baseline: bool, changed: &[&str], files: &[&str]) -> SourceSnapshot {
    let current: BTreeMap<RepoPath, CurrentFile> = files
        .iter()
        .map(|name| {
            (
                path(name),
                CurrentFile {
                    kind: FileKind::File,
                    digest: Fingerprint::of_bytes(name.as_bytes()),
                },
            )
        })
        .collect();
    SourceSnapshot::new(
        WorkspaceId::try_new("ws-fixture").unwrap(),
        SourceSnapshotParts {
            baseline: baseline.then(|| BaselineId::try_new("c".repeat(40)).unwrap()),
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
    snapshot_with(true, changed, FILES)
}

fn graph() -> DependencyGraph {
    let mut builder = DependencyGraph::builder();
    for (consumer, dependency) in [
        ("apps/api/user.test.ts", "apps/api/user.ts"),
        ("apps/api/order.test.ts", "apps/api/order.ts"),
        ("apps/api/user.ts", "packages/shared/date.ts"),
        ("apps/web/App.test.tsx", "apps/web/App.tsx"),
        ("apps/web/App.tsx", "packages/shared/date.ts"),
    ] {
        builder.edge(path(consumer), path(dependency));
    }
    builder.build()
}

fn suites() -> Vec<SuiteDefinition> {
    let mut typecheck = suite_parts("typecheck", RunnerKind::Command, &[]);
    typecheck.always_run = true;
    vec![
        suite("api-unit", RunnerKind::Bun, &["apps/api/**/*.test.ts"]),
        suite("web-unit", RunnerKind::Vitest, &["apps/web/**/*.test.tsx"]),
        suite("e2e", RunnerKind::Playwright, &["tests/e2e/**/*.spec.ts"]),
        SuiteDefinition::try_new(typecheck).unwrap(),
    ]
}

struct Harness {
    settings: Settings,
    source: FakeSourceRepository,
    analyzer: FakeDependencyAnalyzer,
}

impl Harness {
    fn new(snapshot: SourceSnapshot) -> Self {
        Self {
            settings: settings(suites()),
            source: FakeSourceRepository::returning(snapshot),
            analyzer: FakeDependencyAnalyzer::returning(graph(), graph()),
        }
    }

    async fn plan(&self, request: &PlanRequest) -> Result<PlanningOutput, PortError> {
        let use_case = PlanWorkspace {
            settings: &self.settings,
            source: &self.source,
            analyzer: &self.analyzer,
            catalog: &StaticCatalog,
        };
        use_case
            .execute(Path::new("/workspace"), request, &context())
            .await
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

fn selection_of(output: &PlanningOutput, suite: &str) -> Selection {
    output
        .plan
        .suite(&suite_id(suite))
        .unwrap()
        .effective_selection
        .clone()
}

fn files(names: &[&str]) -> Selection {
    Selection::files(names.iter().map(|name| path(name))).unwrap()
}

#[tokio::test]
async fn affected_narrows_file_graph_suites_and_keeps_whole_suites_whole() {
    let harness = Harness::new(snapshot(&["apps/api/order.ts"]));
    let output = harness
        .plan(&request(PlanPurpose::Test, Mode::Affected))
        .await
        .unwrap();

    assert_eq!(
        selection_of(&output, "api-unit"),
        files(&["apps/api/order.test.ts"])
    );
    assert_eq!(
        selection_of(&output, "web-unit"),
        Selection::Omit {
            reason: OmitReason::NoAffectedTests
        }
    );
    assert_eq!(selection_of(&output, "e2e"), Selection::All);
    assert_eq!(selection_of(&output, "typecheck"), Selection::All);

    let api = output.plan.suite(&suite_id("api-unit")).unwrap();
    assert_eq!(api.capability_verification, CapabilityVerification::Pending);
    assert!(
        api.reasons
            .contains(&SelectionReason::TransitiveDependency {
                dependency_path: vec![path("apps/api/order.test.ts"), path("apps/api/order.ts")],
            })
    );
    assert_eq!(output.plan.assurance(), Assurance::Advisory);
    assert_eq!(
        output.plan.body().changed_paths,
        Some(vec![path("apps/api/order.ts")])
    );
}

#[tokio::test]
async fn full_mode_needs_neither_a_baseline_nor_an_analysis() {
    let harness = Harness::new(snapshot_with(false, &[], FILES));
    let output = harness
        .plan(&request(PlanPurpose::Verify, Mode::Full))
        .await
        .unwrap();

    assert!(
        harness.analyzer.calls().is_empty(),
        "full mode must not analyze dependencies"
    );
    assert!(
        output
            .plan
            .suites()
            .iter()
            .all(|suite| suite.effective_selection == Selection::All)
    );
    assert_eq!(output.plan.assurance(), Assurance::FullSuite);
    assert_eq!(output.plan.body().baseline_commit, None);
    assert_eq!(output.plan.body().changed_paths, None);
    assert_eq!(harness.source.requests()[0].mode, Mode::Full);
}

#[tokio::test]
async fn an_unavailable_baseline_is_an_error_and_never_a_silent_full_or_empty_diff() {
    let mut harness = Harness::new(snapshot(&[]));
    harness.source = FakeSourceRepository::failing(PortError::SourceUnavailable {
        reason: SourceUnavailableReason::BaseNotFound,
        message: "origin/main is not available in this clone".to_owned(),
    });
    for mode in [Mode::Affected, Mode::Shadow] {
        let error = harness
            .plan(&request(PlanPurpose::Test, mode))
            .await
            .unwrap_err();
        assert_eq!(error.code(), "source_unavailable");
        assert_eq!(error.exit_condition().exit_code(), 2);
    }
}

#[tokio::test]
async fn a_snapshot_without_baseline_cannot_produce_an_affected_plan() {
    let harness = Harness::new(snapshot_with(false, &[], FILES));
    let error = harness
        .plan(&request(PlanPurpose::Test, Mode::Affected))
        .await
        .unwrap_err();
    assert_eq!(
        error,
        PortError::Planning(PlanningError::MissingBaseline { mode: "affected" })
    );
}

#[tokio::test]
async fn a_failing_analysis_is_an_error_and_never_an_empty_graph() {
    let mut harness = Harness::new(snapshot(&["apps/api/order.ts"]));
    harness.analyzer = FakeDependencyAnalyzer::failing(PortError::SourceUnavailable {
        reason: SourceUnavailableReason::ReadFailed,
        message: "cannot read".to_owned(),
    });
    let error = harness
        .plan(&request(PlanPurpose::Test, Mode::Affected))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "source_unavailable");
    assert_eq!(error.exit_condition().exit_code(), 3);
}

/// P02: a code change no test depends on widens every suite.
#[tokio::test]
async fn p02_change_without_related_tests_widens_every_suite() {
    let harness = Harness::new(snapshot(&["apps/api/server.ts"]));
    let output = harness
        .plan(&request(PlanPurpose::Test, Mode::Affected))
        .await
        .unwrap();
    assert!(
        output
            .plan
            .suites()
            .iter()
            .all(|suite| suite.effective_selection == Selection::All)
    );
    assert_eq!(
        output.plan.body().fallbacks,
        vec![FallbackReason::NoRelatedTests {
            path: path("apps/api/server.ts")
        }]
    );
}

/// P03: without changes only `always_run` suites have work.
#[tokio::test]
async fn p03_no_changes_is_no_work_except_for_always_run() {
    let harness = Harness::new(snapshot(&[]));
    let output = harness
        .plan(&request(PlanPurpose::Test, Mode::Affected))
        .await
        .unwrap();
    assert_eq!(
        selection_of(&output, "api-unit"),
        Selection::Omit {
            reason: OmitReason::NoChanges
        }
    );
    assert_eq!(
        selection_of(&output, "e2e"),
        Selection::Omit {
            reason: OmitReason::NoChanges
        }
    );
    assert_eq!(selection_of(&output, "typecheck"), Selection::All);
    assert!(output.plan.has_work());

    let without_gate = Harness {
        settings: settings(
            suites()
                .into_iter()
                .filter(|suite| !suite.always_run())
                .collect(),
        ),
        ..Harness::new(snapshot(&[]))
    };
    let output = without_gate
        .plan(&request(PlanPurpose::Test, Mode::Affected))
        .await
        .unwrap();
    assert!(
        !output.plan.has_work(),
        "no change and no always_run suite means no work"
    );
}

/// P04 / G12: the configuration file is always a global input.
#[tokio::test]
async fn p04_g12_global_inputs_widen_every_suite() {
    for changed in ["testry.toml", "apps/api/package.json", "bun.lock"] {
        let harness = Harness::new(snapshot(&[changed]));
        let output = harness
            .plan(&request(PlanPurpose::Test, Mode::Affected))
            .await
            .unwrap();
        assert!(
            output
                .plan
                .suites()
                .iter()
                .all(|suite| suite.effective_selection == Selection::All),
            "{changed}"
        );
        assert!(matches!(
            output.plan.body().fallbacks.as_slice(),
            [FallbackReason::GlobalInputChanged { .. }]
        ));
    }
}

/// P05: a runner without an exact file filter runs the whole suite.
#[tokio::test]
async fn p05_unsupported_runner_filter_widens_to_the_whole_suite() {
    let harness = Harness::new(snapshot(&["apps/api/order.ts"]));
    let output = harness
        .plan(&request(PlanPurpose::Test, Mode::Affected))
        .await
        .unwrap();
    let mut api = output.plan.suite(&suite_id("api-unit")).unwrap().clone();
    assert!(matches!(api.effective_selection, Selection::Files { .. }));

    let mut supported = api.clone();
    apply_runner_capabilities(&mut supported, true);
    assert!(matches!(
        supported.effective_selection,
        Selection::Files { .. }
    ));
    assert_eq!(
        supported.capability_verification,
        CapabilityVerification::NotRequired
    );

    apply_runner_capabilities(&mut api, false);
    assert_eq!(api.effective_selection, Selection::All);
    assert!(api.effective_selection.covers(&api.requested_selection));
    assert!(
        api.reasons
            .contains(&SelectionReason::UnsupportedCapability {
                capability: "file_selection".to_owned()
            })
    );

    let mut omitted = output.plan.suite(&suite_id("web-unit")).unwrap().clone();
    apply_runner_capabilities(&mut omitted, false);
    assert!(
        !omitted.effective_selection.runs_anything(),
        "an omitted suite stays omitted"
    );
}

/// P06: shadow executes everything once and records the affected candidate.
#[tokio::test]
async fn p06_shadow_runs_everything_and_records_the_candidate() {
    let harness = Harness::new(snapshot(&["apps/api/order.ts"]));
    let output = harness
        .plan(&request(PlanPurpose::Verify, Mode::Shadow))
        .await
        .unwrap();

    assert!(
        output
            .plan
            .suites()
            .iter()
            .all(|suite| suite.effective_selection == Selection::All)
    );
    assert_eq!(output.plan.assurance(), Assurance::FullSuite);
    let api = output.plan.suite(&suite_id("api-unit")).unwrap();
    assert_eq!(
        api.candidate.as_ref().unwrap().selection,
        files(&["apps/api/order.test.ts"])
    );
    let web = output.plan.suite(&suite_id("web-unit")).unwrap();
    assert_eq!(
        web.candidate.as_ref().unwrap().selection,
        Selection::Omit {
            reason: OmitReason::NoAffectedTests
        }
    );
    assert!(output.plan.body().baseline_commit.is_some());
}

/// P07: affected alone or a limited scope is rejected as a CI quality gate.
#[tokio::test]
async fn p07_ci_rejects_affected_and_scope_limits_before_any_io() {
    let harness = Harness::new(snapshot(&["apps/api/order.ts"]));
    let scope = Some(BTreeSet::from([suite_id("api-unit")]));
    let base = Some("origin/main".to_owned());
    let cases = [
        (
            PlanRequest {
                ci: true,
                ..request(PlanPurpose::Test, Mode::Affected)
            },
            PlanningError::CiRequiresFullVerification,
        ),
        (
            PlanRequest {
                ci: true,
                scope: scope.clone(),
                ..request(PlanPurpose::Verify, Mode::Full)
            },
            PlanningError::CiRejectsScopeLimit,
        ),
        (
            PlanRequest {
                ci: true,
                scope,
                base_ref: base.clone(),
                ..request(PlanPurpose::Verify, Mode::Shadow)
            },
            PlanningError::CiRejectsScopeLimit,
        ),
        (
            PlanRequest {
                ci: true,
                ..request(PlanPurpose::Verify, Mode::Shadow)
            },
            PlanningError::CiShadowNeedsExplicitBase,
        ),
        (
            request(PlanPurpose::Verify, Mode::Affected),
            PlanningError::VerifyRejectsAffected,
        ),
    ];
    for (request, expected) in cases {
        let error = harness.plan(&request).await.unwrap_err();
        assert_eq!(error, PortError::Planning(expected));
        assert_eq!(error.exit_condition().exit_code(), 2);
    }
    assert!(
        harness.source.requests().is_empty(),
        "rejected requests must not touch the sources"
    );

    let accepted = PlanRequest {
        ci: true,
        base_ref: base,
        ..request(PlanPurpose::Verify, Mode::Shadow)
    };
    assert!(harness.plan(&accepted).await.is_ok());
    let full = PlanRequest {
        ci: true,
        ..request(PlanPurpose::Verify, Mode::Full)
    };
    assert_eq!(
        harness.plan(&full).await.unwrap().plan.assurance(),
        Assurance::FullSuite
    );
}

#[tokio::test]
async fn a_local_suite_scope_is_allowed_but_only_advisory() {
    let harness = Harness::new(snapshot_with(false, &[], FILES));
    let scoped = PlanRequest {
        scope: Some(BTreeSet::from([suite_id("api-unit")])),
        ..request(PlanPurpose::Verify, Mode::Full)
    };
    let output = harness.plan(&scoped).await.unwrap();
    assert_eq!(output.plan.suites().len(), 1);
    assert_eq!(output.plan.assurance(), Assurance::Advisory);
    assert!(output.plan.body().scope_limited);
    assert_eq!(output.plan.body().scope, vec![suite_id("api-unit")]);
}

#[tokio::test]
async fn an_unknown_suite_in_the_scope_is_rejected() {
    let harness = Harness::new(snapshot(&[]));
    let scoped = PlanRequest {
        scope: Some(BTreeSet::from([suite_id("nope")])),
        ..request(PlanPurpose::Test, Mode::Affected)
    };
    let error = harness.plan(&scoped).await.unwrap_err();
    assert_eq!(
        error,
        PortError::Planning(PlanningError::UnknownSuite(suite_id("nope")))
    );
}

#[tokio::test]
async fn a_change_reached_only_by_an_out_of_scope_suite_is_covered() {
    let harness = Harness::new(snapshot(&["apps/web/App.tsx"]));
    let scoped = PlanRequest {
        scope: Some(BTreeSet::from([suite_id("api-unit")])),
        ..request(PlanPurpose::Test, Mode::Affected)
    };
    let output = harness.plan(&scoped).await.unwrap();
    assert!(output.plan.body().fallbacks.is_empty());
    assert_eq!(
        selection_of(&output, "api-unit"),
        Selection::Omit {
            reason: OmitReason::NoAffectedTests
        }
    );
    assert!(!output.plan.has_work());
}

#[tokio::test]
async fn a_test_suite_without_test_files_is_a_configuration_error() {
    let files: Vec<&str> = FILES
        .iter()
        .copied()
        .filter(|name| !name.contains("e2e"))
        .collect();
    let harness = Harness::new(snapshot_with(false, &[], &files));
    let error = harness
        .plan(&request(PlanPurpose::Verify, Mode::Full))
        .await
        .unwrap_err();
    assert_eq!(
        error,
        PortError::Planning(PlanningError::EmptySuite {
            suite: suite_id("e2e")
        })
    );
    assert_eq!(error.exit_condition().exit_code(), 2);
}

#[tokio::test]
async fn every_resolver_profile_is_analyzed_for_both_sides() {
    let harness = Harness::new(snapshot(&["apps/api/order.ts"]));
    harness
        .plan(&request(PlanPurpose::Test, Mode::Affected))
        .await
        .unwrap();
    let calls: BTreeSet<(SnapshotSide, ResolverProfile)> = harness
        .analyzer
        .calls()
        .iter()
        .map(|profile| (profile.side, profile.resolver))
        .collect();
    let expected: BTreeSet<(SnapshotSide, ResolverProfile)> =
        [SnapshotSide::Baseline, SnapshotSide::Current]
            .into_iter()
            .flat_map(|side| {
                [
                    ResolverProfile::Bun,
                    ResolverProfile::Vite,
                    ResolverProfile::Node,
                ]
                .into_iter()
                .map(move |profile| (side, profile))
            })
            .collect();
    assert_eq!(calls, expected);
}

#[tokio::test]
async fn a_cancelled_operation_stops_before_touching_the_sources() {
    let harness = Harness::new(snapshot(&[]));
    let use_case = PlanWorkspace {
        settings: &harness.settings,
        source: &harness.source,
        analyzer: &harness.analyzer,
        catalog: &StaticCatalog,
    };
    let error = use_case
        .execute(
            Path::new("/workspace"),
            &request(PlanPurpose::Test, Mode::Affected),
            &cancelled_context(),
        )
        .await
        .unwrap_err();
    assert_eq!(error, PortError::Cancelled);
    assert_eq!(error.exit_condition().exit_code(), 130);
    assert!(harness.source.requests().is_empty());
}

/// P13: a plan is not usable once sources or configuration changed.
#[tokio::test]
async fn p13_a_plan_is_invalid_for_changed_inputs() {
    let harness = Harness::new(snapshot(&["apps/api/order.ts"]));
    let output = harness
        .plan(&request(PlanPurpose::Test, Mode::Affected))
        .await
        .unwrap();
    let digest = *harness.settings.config_digest();

    assert!(ensure_plan_is_current(&output.plan, &output.snapshot, &digest).is_ok());

    let moved_on = snapshot(&["apps/api/order.ts", "apps/api/user.ts"]);
    let stale = ensure_plan_is_current(&output.plan, &moved_on, &digest).unwrap_err();
    assert_eq!(stale.code(), "stale_inputs");
    assert_eq!(stale.exit_condition().exit_code(), 3);

    let other_config = Fingerprint::of_bytes(b"edited configuration");
    let stale = ensure_plan_is_current(&output.plan, &output.snapshot, &other_config).unwrap_err();
    assert_eq!(stale.code(), "stale_inputs");
}

/// G18: the plan does not depend on the order in which inputs are listed.
#[tokio::test]
async fn g18_plan_is_identical_for_reordered_inputs() {
    let forward = Harness::new(snapshot(&["apps/api/order.ts", "packages/shared/date.ts"]));
    let mut reversed_files: Vec<&str> = FILES.to_vec();
    reversed_files.reverse();
    let mut backward = Harness::new(snapshot_with(
        true,
        &["packages/shared/date.ts", "apps/api/order.ts"],
        &reversed_files,
    ));
    let mut reversed_suites = suites();
    reversed_suites.reverse();
    backward.settings = settings(reversed_suites);

    let request = request(PlanPurpose::Test, Mode::Affected);
    let a = forward.plan(&request).await.unwrap().plan;
    let b = backward.plan(&request).await.unwrap().plan;
    assert_eq!(a.id(), b.id());
    assert_eq!(a.to_json().unwrap(), b.to_json().unwrap());
}

#[tokio::test]
async fn explain_reports_reasons_and_only_relevant_fallbacks() {
    let harness = Harness::new(snapshot(&["apps/api/order.ts"]));
    let narrowed = harness
        .plan(&request(PlanPurpose::Test, Mode::Affected))
        .await
        .unwrap()
        .plan;
    let explanation = explain(&narrowed, &suite_id("api-unit")).unwrap();
    assert!(explanation.fallbacks.is_empty());
    let json = serde_json::to_value(&explanation).unwrap();
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["suite"]["suite_id"], "api-unit");
    assert_eq!(json["suite"]["reasons"][0]["code"], "transitive_dependency");
    assert_eq!(
        json["suite"]["reasons"][0]["dependency_path"],
        serde_json::json!(["apps/api/order.test.ts", "apps/api/order.ts"])
    );

    let widened = Harness::new(snapshot(&["bun.lock"]));
    let widened = widened
        .plan(&request(PlanPurpose::Test, Mode::Affected))
        .await
        .unwrap()
        .plan;
    let explanation = explain(&widened, &suite_id("api-unit")).unwrap();
    assert_eq!(explanation.fallbacks.len(), 1);

    assert_eq!(
        explain(&narrowed, &suite_id("missing")).unwrap_err(),
        PlanningError::UnknownSuite(suite_id("missing"))
    );
}

#[test]
fn settings_builder_rejects_what_the_use_case_relies_on() {
    let mut parts = settings_parts(suites());
    parts.ci_default_mode = Mode::Affected;
    assert!(Settings::try_new(parts).is_err());
}
