//! The planning use case: snapshot -> graphs -> inventory -> plan.
//!
//! Only Git and file reads happen here. No runner configuration is executed,
//! nothing is installed and no environment is started.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, Instant};

use testry_domain::graph::DependencyGraph;
use testry_domain::ids::SuiteId;
use testry_domain::inventory::TestInventory;
use testry_domain::plan::{ExecutionPlan, Mode};
use testry_domain::snapshot::SourceSnapshot;

use super::create_plan::{PlanningError, SelectionPolicy, create_plan, validate_execution_plan};
use crate::ports::planning::{
    AnalysisLimits, AnalysisProfile, DependencyAnalyzer, SnapshotRequest, SnapshotSide,
    SourceRepository, TestCatalog,
};
use crate::ports::{OperationContext, PortError};
use crate::settings::Settings;

/// Which command asks for the plan. `verify` is the quality gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanPurpose {
    /// `plan`: inspect only.
    Inspect,
    /// `test`: the local feedback loop.
    Test,
    /// `verify`: the quality gate.
    Verify,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanRequest {
    pub purpose: PlanPurpose,
    pub mode: Mode,
    /// `None` means `HEAD`: the uncommitted changes.
    pub base_ref: Option<String>,
    pub scope: Option<BTreeSet<SuiteId>>,
    pub ci: bool,
}

impl PlanRequest {
    /// Rules that can be decided from the request alone. They are checked
    /// before any IO so that a rejected request has no side effects.
    pub fn validate(&self, settings: &Settings) -> Result<(), PlanningError> {
        if self.purpose == PlanPurpose::Verify && self.mode == Mode::Affected {
            return Err(PlanningError::VerifyRejectsAffected);
        }
        if self.ci {
            if self.mode == Mode::Affected {
                return Err(PlanningError::CiRequiresFullVerification);
            }
            if self.scope.is_some() {
                return Err(PlanningError::CiRejectsScopeLimit);
            }
            if self.mode == Mode::Shadow && self.base_ref.is_none() {
                return Err(PlanningError::CiShadowNeedsExplicitBase);
            }
        }
        for id in self.scope.iter().flatten() {
            if !settings.suites().iter().any(|suite| suite.id() == id) {
                return Err(PlanningError::UnknownSuite(id.clone()));
            }
        }
        Ok(())
    }
}

/// Wall-clock breakdown of planning, measured with a monotonic clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlanningTimings {
    pub snapshot: Duration,
    pub inventory: Duration,
    pub analysis: Duration,
    pub selection: Duration,
    pub total: Duration,
}

#[derive(Debug, Clone)]
pub struct PlanningOutput {
    pub plan: ExecutionPlan,
    pub snapshot: SourceSnapshot,
    pub inventory: TestInventory,
    pub timings: PlanningTimings,
}

pub struct PlanWorkspace<'a> {
    pub settings: &'a Settings,
    pub source: &'a dyn SourceRepository,
    pub analyzer: &'a dyn DependencyAnalyzer,
    pub catalog: &'a dyn TestCatalog,
}

impl PlanWorkspace<'_> {
    /// Captures the inputs of `request`. The execution use case calls this
    /// again before and after running tests to detect changed inputs.
    pub async fn snapshot(
        &self,
        workspace_root: &Path,
        request: &PlanRequest,
        ctx: &OperationContext,
    ) -> Result<SourceSnapshot, PortError> {
        let snapshot_request = SnapshotRequest {
            workspace_root: workspace_root.to_path_buf(),
            mode: request.mode,
            base_ref: request.base_ref.clone(),
            include_dirty: true,
            scope: request.scope.clone(),
            exclude: self.settings.exclude().clone(),
            declared_inputs: self.settings.declared_inputs().clone(),
        };
        self.source.snapshot(&snapshot_request, ctx).await
    }

    pub async fn execute(
        &self,
        workspace_root: &Path,
        request: &PlanRequest,
        ctx: &OperationContext,
    ) -> Result<PlanningOutput, PortError> {
        let started = Instant::now();
        request.validate(self.settings)?;
        ctx.ensure_active("planning")?;

        let snapshot = self.snapshot(workspace_root, request, ctx).await?;
        let snapshot_done = Instant::now();

        let inventory = self
            .catalog
            .collect(&snapshot, self.settings.suites(), ctx)
            .await?;
        let inventory_done = Instant::now();

        // `full` executes everything in scope, so no diff and no graph is
        // needed. For `affected` and `shadow` a failing analysis is an error;
        // it is never downgraded to "run nothing" or silently to `full`.
        let (baseline_graph, current_graph) = if request.mode.needs_baseline() {
            let baseline = self.analyze(&snapshot, SnapshotSide::Baseline, ctx).await?;
            let current = self.analyze(&snapshot, SnapshotSide::Current, ctx).await?;
            (Some(baseline), Some(current))
        } else {
            (None, None)
        };
        let analysis_done = Instant::now();

        let policy = SelectionPolicy {
            mode: request.mode,
            scope: request.scope.clone(),
            ci: request.ci,
            global_inputs: self.settings.global_inputs().clone(),
            non_test_inputs: self.settings.non_test_inputs().clone(),
            budget: *self.settings.budget(),
            config_digest: *self.settings.config_digest(),
        };
        let plan = create_plan(
            &snapshot,
            baseline_graph.as_ref(),
            current_graph.as_ref(),
            &inventory,
            self.settings.suites(),
            &policy,
        )?;
        validate_execution_plan(&plan, &inventory, self.settings.budget())?;
        let finished = Instant::now();

        Ok(PlanningOutput {
            plan,
            snapshot,
            inventory,
            timings: PlanningTimings {
                snapshot: snapshot_done - started,
                inventory: inventory_done - snapshot_done,
                analysis: analysis_done - inventory_done,
                selection: finished - analysis_done,
                total: finished - started,
            },
        })
    }

    /// One graph per resolver profile, combined as a union. A union
    /// over-approximates every profile, so it can only widen selections.
    async fn analyze(
        &self,
        snapshot: &SourceSnapshot,
        side: SnapshotSide,
        ctx: &OperationContext,
    ) -> Result<DependencyGraph, PortError> {
        let mut combined: Option<DependencyGraph> = None;
        for resolver in self.settings.resolver_profiles() {
            ctx.ensure_active("dependency analysis")?;
            let profile = AnalysisProfile {
                side,
                resolver,
                limits: AnalysisLimits::default(),
            };
            let graph = self.analyzer.analyze(snapshot, &profile, ctx).await?;
            combined = Some(match combined {
                Some(previous) => previous.union(&graph),
                None => graph,
            });
        }
        Ok(combined.unwrap_or_else(|| DependencyGraph::builder().build()))
    }
}
