//! Plan creation and validation (spec sections 5 and 8). Pure functions.

use std::collections::BTreeSet;

use testry_domain::fingerprint::Fingerprint;
use testry_domain::graph::DependencyGraph;
use testry_domain::ids::SuiteId;
use testry_domain::inventory::TestInventory;
use testry_domain::paths::RepoPath;
use testry_domain::pattern::PatternSet;
use testry_domain::plan::{
    CandidatePlan, CapabilityVerification, DiagnosticSource, ExecutionPlan, Mode, PlanDiagnostic,
    PlanDraft, PlanError, SelectionReason, SuitePlan,
};
use testry_domain::planning::{AffectedSelection, SelectionInput, SuiteSelection, select_affected};
use testry_domain::resources::{ResourceBudget, ResourceError};
use testry_domain::selection::Selection;
use testry_domain::snapshot::SourceSnapshot;
use testry_domain::suite::SuiteDefinition;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanningError {
    #[error("unknown suite `{0}`; it is not defined in the configuration")]
    UnknownSuite(SuiteId),
    #[error("a CI run must use mode full or shadow; affected alone is not a quality gate")]
    CiRequiresFullVerification,
    #[error("a CI run must cover every configured suite; remove --suite")]
    CiRejectsScopeLimit,
    #[error("shadow mode in CI needs an explicit --base")]
    CiShadowNeedsExplicitBase,
    #[error(
        "`verify` is the quality gate and only accepts mode full or shadow; use `test` for affected runs"
    )]
    VerifyRejectsAffected,
    #[error("mode {mode} needs a resolved baseline commit; it was not treated as an empty diff")]
    MissingBaseline { mode: &'static str },
    #[error("mode {mode} needs the baseline and the current dependency graph")]
    MissingGraph { mode: &'static str },
    #[error("suite {suite}: no file matches its include patterns, so it would verify nothing")]
    EmptySuite { suite: SuiteId },
    #[error("suite {suite}: selected file {path} is not a test file of this suite")]
    SelectionOutsideInventory { suite: SuiteId, path: RepoPath },
    #[error("suite {suite}: {source}")]
    Resource {
        suite: SuiteId,
        source: ResourceError,
    },
    #[error("the selection policy produced no entry for suite {0}")]
    MissingSelection(SuiteId),
    #[error("scheduling failed: {0}")]
    Scheduling(ResourceError),
    #[error(transparent)]
    Plan(#[from] PlanError),
}

/// Everything besides the sources that decides a plan.
#[derive(Debug, Clone)]
pub struct SelectionPolicy {
    pub mode: Mode,
    /// The explicit `--suite` scope. `None` means every configured suite.
    pub scope: Option<BTreeSet<SuiteId>>,
    /// Whether the rules for CI quality gates apply.
    pub ci: bool,
    pub global_inputs: PatternSet,
    pub non_test_inputs: PatternSet,
    pub budget: ResourceBudget,
    pub config_digest: Fingerprint,
}

/// Builds the plan for one snapshot.
///
/// `full` needs neither graph. `affected` and `shadow` need a resolved
/// baseline and both graphs; a missing one is an error and is never treated
/// like `full` or like an empty diff.
pub fn create_plan(
    snapshot: &SourceSnapshot,
    baseline_graph: Option<&DependencyGraph>,
    current_graph: Option<&DependencyGraph>,
    inventory: &TestInventory,
    suites: &[SuiteDefinition],
    policy: &SelectionPolicy,
) -> Result<ExecutionPlan, PlanningError> {
    if policy.ci {
        if policy.mode == Mode::Affected {
            return Err(PlanningError::CiRequiresFullVerification);
        }
        if policy.scope.is_some() {
            return Err(PlanningError::CiRejectsScopeLimit);
        }
    }

    let in_scope = suites_in_scope(suites, policy.scope.as_ref())?;
    let scope_limited = in_scope.len() < suites.len();

    let affected = if policy.mode.needs_baseline() {
        let mode = policy.mode.as_str();
        if snapshot.baseline().is_none() {
            return Err(PlanningError::MissingBaseline { mode });
        }
        let (Some(baseline_graph), Some(current_graph)) = (baseline_graph, current_graph) else {
            return Err(PlanningError::MissingGraph { mode });
        };
        let changed: BTreeSet<RepoPath> = snapshot.changed_paths().cloned().collect();
        Some(select_affected(&SelectionInput {
            changed: &changed,
            graphs: &[baseline_graph, current_graph],
            inventory,
            suites: &in_scope,
            global_inputs: &policy.global_inputs,
            non_test_inputs: &policy.non_test_inputs,
        }))
    } else {
        None
    };

    let mut suite_plans = Vec::with_capacity(in_scope.len());
    for suite in &in_scope {
        suite_plans.push(suite_plan(suite, policy.mode, affected.as_ref())?);
    }

    let mut diagnostics: Vec<PlanDiagnostic> = snapshot
        .diagnostics()
        .iter()
        .map(|diagnostic| PlanDiagnostic {
            source: DiagnosticSource::Snapshot,
            code: diagnostic.kind.code().to_owned(),
            path: diagnostic.path.clone(),
            detail: String::new(),
        })
        .collect();
    if policy.mode.needs_baseline() {
        for graph in [baseline_graph, current_graph].into_iter().flatten() {
            diagnostics.extend(graph.diagnostics().iter().map(|diagnostic| PlanDiagnostic {
                source: DiagnosticSource::Graph,
                code: diagnostic.kind.code().to_owned(),
                path: diagnostic.file.clone(),
                detail: diagnostic.detail.clone(),
            }));
        }
    }

    let draft = PlanDraft {
        workspace_id: snapshot.workspace_id().clone(),
        snapshot_id: *snapshot.id(),
        baseline_commit: snapshot.baseline().cloned(),
        config_digest: policy.config_digest,
        mode: policy.mode,
        scope_limited,
        changed_paths: policy
            .mode
            .needs_baseline()
            .then(|| snapshot.changed_paths().cloned().collect()),
        suites: suite_plans,
        fallbacks: affected
            .map(|selection| selection.fallbacks.into_iter().collect())
            .unwrap_or_default(),
        resources: policy.budget,
        diagnostics,
    };
    Ok(ExecutionPlan::try_new(draft)?)
}

fn suites_in_scope<'a>(
    suites: &'a [SuiteDefinition],
    scope: Option<&BTreeSet<SuiteId>>,
) -> Result<Vec<&'a SuiteDefinition>, PlanningError> {
    let Some(scope) = scope else {
        return Ok(suites.iter().collect());
    };
    for id in scope {
        if !suites.iter().any(|suite| suite.id() == id) {
            return Err(PlanningError::UnknownSuite(id.clone()));
        }
    }
    Ok(suites
        .iter()
        .filter(|suite| scope.contains(suite.id()))
        .collect())
}

fn suite_plan(
    suite: &SuiteDefinition,
    mode: Mode,
    affected: Option<&AffectedSelection>,
) -> Result<SuitePlan, PlanningError> {
    let candidate: Option<&SuiteSelection> = affected.and_then(|selection| {
        selection
            .suites
            .iter()
            .find(|entry| &entry.suite_id == suite.id())
    });
    let missing = || PlanningError::MissingSelection(suite.id().clone());

    let (selection, reasons, candidate) = match mode {
        Mode::Full => (
            Selection::All,
            vec![SelectionReason::FullVerification],
            None,
        ),
        Mode::Shadow => {
            let candidate = candidate.ok_or_else(missing)?;
            (
                Selection::All,
                vec![SelectionReason::FullVerification],
                Some(CandidatePlan {
                    selection: candidate.selection.clone(),
                    reasons: candidate.reasons.iter().cloned().collect(),
                }),
            )
        }
        Mode::Affected => {
            let candidate = candidate.ok_or_else(missing)?;
            (
                candidate.selection.clone(),
                candidate.reasons.iter().cloned().collect(),
                None,
            )
        }
    };

    // A file selection relies on the runner's file filter, which only the
    // execution preflight can confirm for the installed version.
    let capability_verification = if matches!(selection, Selection::Files { .. }) {
        CapabilityVerification::Pending
    } else {
        CapabilityVerification::NotRequired
    };

    Ok(SuitePlan {
        suite_id: suite.id().clone(),
        runner: suite.runner(),
        project: suite.project().cloned(),
        requested_selection: selection.clone(),
        effective_selection: selection,
        reasons,
        candidate,
        capability_verification,
        resources: suite.resources().clone(),
    })
}

/// Applies what the execution preflight learned about a runner. A selection
/// the runner cannot honour exactly is widened to the whole suite; it is never
/// narrowed and never dropped.
pub fn apply_runner_capabilities(suite: &mut SuitePlan, file_selection_supported: bool) {
    if matches!(suite.effective_selection, Selection::Files { .. }) && !file_selection_supported {
        suite.effective_selection = Selection::All;
        suite.reasons.push(SelectionReason::UnsupportedCapability {
            capability: "file_selection".to_owned(),
        });
        suite.reasons.sort();
        suite.reasons.dedup();
    }
    suite.capability_verification = CapabilityVerification::NotRequired;
}

/// Checks a plan against the inventory and the resource budget before it is
/// stored or executed.
pub fn validate_execution_plan(
    plan: &ExecutionPlan,
    inventory: &TestInventory,
    budgets: &ResourceBudget,
) -> Result<(), PlanningError> {
    for suite in plan.suites() {
        let id = &suite.suite_id;
        budgets
            .admit(&suite.resources)
            .map_err(|source| PlanningError::Resource {
                suite: id.clone(),
                source,
            })?;

        let known_files = inventory.suite(id).map(|inventory| &inventory.files);
        if suite.runner.is_test_runner() && known_files.is_none_or(BTreeSet::is_empty) {
            return Err(PlanningError::EmptySuite { suite: id.clone() });
        }

        let candidate = suite
            .candidate
            .as_ref()
            .map(|candidate| &candidate.selection);
        let selections = [
            Some(&suite.requested_selection),
            Some(&suite.effective_selection),
            candidate,
        ];
        for selection in selections.into_iter().flatten() {
            let Selection::Files { files } = selection else {
                continue;
            };
            for file in files.iter() {
                if !known_files.is_some_and(|known| known.contains(file)) {
                    return Err(PlanningError::SelectionOutsideInventory {
                        suite: id.clone(),
                        path: file.clone(),
                    });
                }
            }
        }
    }
    Ok(())
}

/// Refuses to use a plan for inputs other than the ones it was created from.
/// The result of such a run would not describe the current code.
pub fn ensure_plan_is_current(
    plan: &ExecutionPlan,
    snapshot: &SourceSnapshot,
    config_digest: &Fingerprint,
) -> Result<(), crate::ports::PortError> {
    let body = plan.body();
    if &body.config_digest != config_digest {
        return Err(crate::ports::PortError::StaleInputs {
            message: format!(
                "the configuration changed after plan {} was created",
                plan.id()
            ),
        });
    }
    if &body.snapshot_id != snapshot.id() {
        return Err(crate::ports::PortError::StaleInputs {
            message: format!(
                "sources changed after plan {} was created (planned {}, now {})",
                plan.id(),
                body.snapshot_id,
                snapshot.id()
            ),
        });
    }
    Ok(())
}
