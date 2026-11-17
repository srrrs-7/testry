//! Explains why a suite of a stored plan has its selection.

use serde::Serialize;

use testry_domain::ids::{BaselineId, PlanId, SuiteId};
use testry_domain::plan::{
    ExecutionPlan, FallbackReason, Mode, PLAN_SCHEMA_VERSION, PlanDiagnostic, SelectionReason,
    SuitePlan,
};
use testry_domain::results::Assurance;

use crate::commands::create_plan::PlanningError;

/// Everything needed to understand one suite of a plan. Serializes as one
/// JSON object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Explanation<'a> {
    pub schema_version: u32,
    pub plan_id: &'a PlanId,
    pub mode: Mode,
    pub assurance: Assurance,
    pub baseline_commit: Option<&'a BaselineId>,
    pub suite: &'a SuitePlan,
    /// Plan-wide fallbacks, present when they widened this suite.
    pub fallbacks: &'a [FallbackReason],
    /// Graph and snapshot diagnostics, present when they widened this suite.
    pub diagnostics: &'a [PlanDiagnostic],
}

pub fn explain<'a>(
    plan: &'a ExecutionPlan,
    suite: &SuiteId,
) -> Result<Explanation<'a>, PlanningError> {
    let suite_plan = plan
        .suite(suite)
        .ok_or_else(|| PlanningError::UnknownSuite(suite.clone()))?;
    let reasons = suite_plan.reasons.iter().chain(
        suite_plan
            .candidate
            .iter()
            .flat_map(|candidate| candidate.reasons.iter()),
    );
    let widened_by_fallback = reasons
        .into_iter()
        .any(|reason| matches!(reason, SelectionReason::ExpandedByFallback { .. }));
    let body = plan.body();
    Ok(Explanation {
        schema_version: PLAN_SCHEMA_VERSION,
        plan_id: plan.id(),
        mode: plan.mode(),
        assurance: plan.assurance(),
        baseline_commit: body.baseline_commit.as_ref(),
        suite: suite_plan,
        fallbacks: if widened_by_fallback {
            &body.fallbacks
        } else {
            &[]
        },
        diagnostics: if widened_by_fallback {
            &body.diagnostics
        } else {
            &[]
        },
    })
}
