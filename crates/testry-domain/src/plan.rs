//! Execution plans (spec sections 8 and 16).
//!
//! A plan is a canonical, explainable document: which suites run, with which
//! selection, and why. [`ExecutionPlan::try_new`] sorts and deduplicates every
//! collection before the plan ID is derived, so the same inputs enumerated in
//! a different order produce the same plan. Run IDs and timestamps are never
//! part of a plan.

use serde::{Deserialize, Serialize};

use crate::fingerprint::{Fingerprint, FingerprintBuilder};
use crate::ids::{BaselineId, PlanId, RunnerProject, SourceSnapshotId, SuiteId, WorkspaceId};
use crate::paths::RepoPath;
use crate::resources::{ResourceBudget, ResourceRequest};
use crate::results::Assurance;
use crate::selection::Selection;
use crate::suite::RunnerKind;

/// The only plan schema version this build reads or writes.
pub const PLAN_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Narrow through the graph; widen on any uncertainty. Advisory only.
    Affected,
    /// Run every suite in scope.
    Full,
    /// Compute the affected plan as a candidate, execute everything once.
    Shadow,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Affected => "affected",
            Self::Full => "full",
            Self::Shadow => "shadow",
        }
    }

    /// `affected` and `shadow` are defined relative to a baseline commit.
    pub fn needs_baseline(self) -> bool {
        !matches!(self, Self::Full)
    }

    /// `full` and `shadow` execute every suite in scope completely.
    pub fn executes_everything(self) -> bool {
        matches!(self, Self::Full | Self::Shadow)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackKind {
    GlobalInputChanged,
    UnknownDependency,
    NoRelatedTests,
}

/// Why every suite in scope was widened to its full extent.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum FallbackReason {
    /// A changed path matches `analysis.global_inputs`.
    GlobalInputChanged { path: RepoPath, pattern: String },
    /// The dependency graph is incomplete. Details are in the plan diagnostics.
    UnknownDependency { diagnostics: u64 },
    /// A changed path that no test of any suite depends on. It is not assumed
    /// to be irrelevant.
    NoRelatedTests { path: RepoPath },
}

impl FallbackReason {
    pub fn kind(&self) -> FallbackKind {
        match self {
            Self::GlobalInputChanged { .. } => FallbackKind::GlobalInputChanged,
            Self::UnknownDependency { .. } => FallbackKind::UnknownDependency,
            Self::NoRelatedTests { .. } => FallbackKind::NoRelatedTests,
        }
    }
}

/// Why a suite has the selection it has.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum SelectionReason {
    /// `full` or `shadow`: everything in scope is executed.
    FullVerification,
    /// The suite has `always_run = true`.
    AlwaysRun,
    /// The suite has `selection = "whole_suite"` and a relevant change exists.
    WholeSuitePolicy,
    /// The test file itself was added or modified.
    DirectChange {
        path: RepoPath,
    },
    /// The test file depends on a changed file. `dependency_path` leads from
    /// the test file to the changed file.
    TransitiveDependency {
        dependency_path: Vec<RepoPath>,
    },
    /// A changed file matches the suite's declared `inputs`, and the import
    /// graph cannot show which tests use it.
    DeclaredInputChanged {
        path: RepoPath,
        pattern: String,
    },
    /// A plan-wide fallback applied; see `fallbacks` of the plan.
    ExpandedByFallback {
        fallback: FallbackKind,
    },
    /// The runner cannot honour the narrower selection.
    UnsupportedCapability {
        capability: String,
    },
    NoChanges,
    OnlyNonTestInputsChanged,
    NoAffectedTests,
}

/// Whether the selection still depends on a runner capability that only the
/// execution preflight can confirm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityVerification {
    NotRequired,
    Pending,
}

/// The affected selection recorded next to a `shadow` run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidatePlan {
    pub selection: Selection,
    pub reasons: Vec<SelectionReason>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuitePlan {
    pub suite_id: SuiteId,
    pub runner: RunnerKind,
    pub project: Option<RunnerProject>,
    /// What the selection policy asks for.
    pub requested_selection: Selection,
    /// What will be executed. Always covers `requested_selection`.
    pub effective_selection: Selection,
    pub reasons: Vec<SelectionReason>,
    /// Present in `shadow` mode only.
    pub candidate: Option<CandidatePlan>,
    pub capability_verification: CapabilityVerification,
    pub resources: ResourceRequest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSource {
    Snapshot,
    Graph,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PlanDiagnostic {
    pub source: DiagnosticSource,
    pub code: String,
    pub path: RepoPath,
    pub detail: String,
}

/// Everything a plan is made of, before canonicalization.
#[derive(Debug, Clone)]
pub struct PlanDraft {
    pub workspace_id: WorkspaceId,
    pub snapshot_id: SourceSnapshotId,
    pub baseline_commit: Option<BaselineId>,
    pub config_digest: Fingerprint,
    pub mode: Mode,
    /// Whether `--suite` narrowed the scope below all configured suites.
    pub scope_limited: bool,
    /// `None` when no diff was computed (full mode).
    pub changed_paths: Option<Vec<RepoPath>>,
    pub suites: Vec<SuitePlan>,
    pub fallbacks: Vec<FallbackReason>,
    pub resources: ResourceBudget,
    pub diagnostics: Vec<PlanDiagnostic>,
}

/// The canonical content of a plan. The plan ID is the fingerprint of this
/// structure's JSON form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanBody {
    pub workspace_id: WorkspaceId,
    pub snapshot_id: SourceSnapshotId,
    pub baseline_commit: Option<BaselineId>,
    pub config_digest: Fingerprint,
    pub mode: Mode,
    pub scope: Vec<SuiteId>,
    pub scope_limited: bool,
    pub assurance: Assurance,
    pub changed_paths: Option<Vec<RepoPath>>,
    pub suites: Vec<SuitePlan>,
    pub fallbacks: Vec<FallbackReason>,
    pub resources: ResourceBudget,
    pub diagnostics: Vec<PlanDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    #[error("a plan needs at least one suite in scope")]
    NoSuites,
    #[error("suite {0} appears more than once in the plan")]
    DuplicateSuite(SuiteId),
    #[error("mode {mode} needs a resolved baseline commit and a computed diff")]
    MissingBaseline { mode: &'static str },
    #[error("suite {0}: the effective selection is narrower than the requested selection")]
    EffectiveSelectionNarrower(SuiteId),
    #[error("suite {suite}: mode {mode} must execute the whole suite")]
    NotExhaustive { suite: SuiteId, mode: &'static str },
    #[error("suite {0}: shadow mode needs a candidate selection")]
    MissingCandidate(SuiteId),
    #[error("suite {0}: a candidate selection is only allowed in shadow mode")]
    UnexpectedCandidate(SuiteId),
    #[error("suite {0}: a file selection must be marked as pending capability verification")]
    UnverifiedFileSelection(SuiteId),
    #[error("plan could not be serialized: {0}")]
    Serialization(String),
}

/// Why a stored plan document was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanDocumentError {
    #[error("plan is not valid JSON or does not match the plan schema: {0}")]
    Malformed(String),
    #[error("plan has no numeric `schema_version`")]
    MissingSchemaVersion,
    #[error("plan schema version {found} is not supported (this build reads version {supported})")]
    UnsupportedSchemaVersion { found: u64, supported: u32 },
    #[error("plan content does not match its plan_id; the document was modified or is corrupted")]
    IdMismatch,
    #[error("plan violates a plan invariant: {0}")]
    Invalid(#[from] PlanError),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExecutionPlan {
    schema_version: u32,
    plan_id: PlanId,
    #[serde(flatten)]
    body: PlanBody,
}

#[derive(Deserialize)]
struct PlanDocument {
    plan_id: PlanId,
    #[serde(flatten)]
    body: PlanBody,
}

impl ExecutionPlan {
    /// Canonicalizes and validates a draft, then derives the plan ID.
    pub fn try_new(draft: PlanDraft) -> Result<Self, PlanError> {
        let body = canonicalize(draft)?;
        let plan_id = compute_plan_id(&body)?;
        Ok(Self {
            schema_version: PLAN_SCHEMA_VERSION,
            plan_id,
            body,
        })
    }

    /// Reads a stored plan. Other schema versions are rejected instead of
    /// being interpreted as version 1, and the content must match the ID.
    pub fn from_json(bytes: &[u8]) -> Result<Self, PlanDocumentError> {
        let value: serde_json::Value = serde_json::from_slice(bytes)
            .map_err(|error| PlanDocumentError::Malformed(error.to_string()))?;
        let found = value
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            .ok_or(PlanDocumentError::MissingSchemaVersion)?;
        if found != u64::from(PLAN_SCHEMA_VERSION) {
            return Err(PlanDocumentError::UnsupportedSchemaVersion {
                found,
                supported: PLAN_SCHEMA_VERSION,
            });
        }
        let document: PlanDocument = serde_json::from_value(value)
            .map_err(|error| PlanDocumentError::Malformed(error.to_string()))?;
        validate(&document.body)?;
        if !is_canonical(&document.body) || compute_plan_id(&document.body)? != document.plan_id {
            return Err(PlanDocumentError::IdMismatch);
        }
        Ok(Self {
            schema_version: PLAN_SCHEMA_VERSION,
            plan_id: document.plan_id,
            body: document.body,
        })
    }

    pub fn to_json(&self) -> Result<String, PlanError> {
        serde_json::to_string_pretty(self)
            .map_err(|error| PlanError::Serialization(error.to_string()))
    }

    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub fn id(&self) -> &PlanId {
        &self.plan_id
    }

    pub fn body(&self) -> &PlanBody {
        &self.body
    }

    pub fn mode(&self) -> Mode {
        self.body.mode
    }

    pub fn assurance(&self) -> Assurance {
        self.body.assurance
    }

    pub fn suites(&self) -> &[SuitePlan] {
        &self.body.suites
    }

    pub fn suite(&self, id: &SuiteId) -> Option<&SuitePlan> {
        self.body.suites.iter().find(|suite| &suite.suite_id == id)
    }

    /// Whether at least one suite executes something. A plan without work is
    /// reported as `no_work`, never as a successful verification.
    pub fn has_work(&self) -> bool {
        self.body
            .suites
            .iter()
            .any(|suite| suite.effective_selection.runs_anything())
    }
}

fn canonicalize(draft: PlanDraft) -> Result<PlanBody, PlanError> {
    let mut suites = draft.suites;
    suites.sort_by(|left, right| left.suite_id.cmp(&right.suite_id));
    for suite in &mut suites {
        sort_dedup(&mut suite.reasons);
        if let Some(candidate) = &mut suite.candidate {
            sort_dedup(&mut candidate.reasons);
        }
    }
    let scope: Vec<SuiteId> = suites.iter().map(|suite| suite.suite_id.clone()).collect();

    let mut changed_paths = draft.changed_paths;
    if let Some(paths) = &mut changed_paths {
        sort_dedup(paths);
    }
    let mut fallbacks = draft.fallbacks;
    sort_dedup(&mut fallbacks);
    let mut diagnostics = draft.diagnostics;
    sort_dedup(&mut diagnostics);

    let assurance = derive_assurance(draft.mode, draft.scope_limited);
    let body = PlanBody {
        workspace_id: draft.workspace_id,
        snapshot_id: draft.snapshot_id,
        baseline_commit: draft.baseline_commit,
        config_digest: draft.config_digest,
        mode: draft.mode,
        scope,
        scope_limited: draft.scope_limited,
        assurance,
        changed_paths,
        suites,
        fallbacks,
        resources: draft.resources,
        diagnostics,
    };
    validate(&body)?;
    Ok(body)
}

/// `full_suite` is only attainable when every configured suite runs in full.
fn derive_assurance(mode: Mode, scope_limited: bool) -> Assurance {
    if mode.executes_everything() && !scope_limited {
        Assurance::FullSuite
    } else {
        Assurance::Advisory
    }
}

fn sort_dedup<T: Ord>(items: &mut Vec<T>) {
    items.sort();
    items.dedup();
}

fn is_sorted_unique<T: Ord>(items: &[T]) -> bool {
    items
        .windows(2)
        .all(|pair| matches!(pair, [left, right] if left < right))
}

fn is_canonical(body: &PlanBody) -> bool {
    let suite_ids: Vec<&SuiteId> = body.suites.iter().map(|suite| &suite.suite_id).collect();
    is_sorted_unique(&suite_ids)
        && body.scope.iter().eq(suite_ids.iter().copied())
        && body.changed_paths.as_deref().is_none_or(is_sorted_unique)
        && is_sorted_unique(&body.fallbacks)
        && is_sorted_unique(&body.diagnostics)
        && body.assurance == derive_assurance(body.mode, body.scope_limited)
        && body.suites.iter().all(|suite| {
            is_sorted_unique(&suite.reasons)
                && suite
                    .candidate
                    .as_ref()
                    .is_none_or(|c| is_sorted_unique(&c.reasons))
        })
}

fn validate(body: &PlanBody) -> Result<(), PlanError> {
    if body.suites.is_empty() {
        return Err(PlanError::NoSuites);
    }
    for pair in body.suites.windows(2) {
        if let [left, right] = pair
            && left.suite_id == right.suite_id
        {
            return Err(PlanError::DuplicateSuite(left.suite_id.clone()));
        }
    }
    if body.mode.needs_baseline()
        && (body.baseline_commit.is_none() || body.changed_paths.is_none())
    {
        return Err(PlanError::MissingBaseline {
            mode: body.mode.as_str(),
        });
    }
    for suite in &body.suites {
        let id = || suite.suite_id.clone();
        if !suite.effective_selection.covers(&suite.requested_selection) {
            return Err(PlanError::EffectiveSelectionNarrower(id()));
        }
        if body.mode.executes_everything() && suite.effective_selection != Selection::All {
            return Err(PlanError::NotExhaustive {
                suite: id(),
                mode: body.mode.as_str(),
            });
        }
        match (body.mode, &suite.candidate) {
            (Mode::Shadow, None) => return Err(PlanError::MissingCandidate(id())),
            (Mode::Affected | Mode::Full, Some(_)) => {
                return Err(PlanError::UnexpectedCandidate(id()));
            }
            _ => {}
        }
        let selects_files = matches!(suite.effective_selection, Selection::Files { .. });
        if selects_files && suite.capability_verification != CapabilityVerification::Pending {
            return Err(PlanError::UnverifiedFileSelection(id()));
        }
    }
    Ok(())
}

fn compute_plan_id(body: &PlanBody) -> Result<PlanId, PlanError> {
    let canonical =
        serde_json::to_vec(body).map_err(|error| PlanError::Serialization(error.to_string()))?;
    let mut fingerprint = FingerprintBuilder::new("testry.plan.v1");
    fingerprint.bytes("body", &canonical);
    Ok(PlanId::new(fingerprint.finish()))
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub(crate) fn path(raw: &str) -> RepoPath {
        RepoPath::try_new(raw).unwrap()
    }

    pub(crate) fn suite_plan(id: &str, selection: Selection) -> SuitePlan {
        let capability_verification = if matches!(selection, Selection::Files { .. }) {
            CapabilityVerification::Pending
        } else {
            CapabilityVerification::NotRequired
        };
        SuitePlan {
            suite_id: SuiteId::try_new(id).unwrap(),
            runner: RunnerKind::Bun,
            project: None,
            requested_selection: selection.clone(),
            effective_selection: selection,
            reasons: Vec::new(),
            candidate: None,
            capability_verification,
            resources: ResourceRequest::try_new(1, 512, []).unwrap(),
        }
    }

    pub(crate) fn draft(mode: Mode, suites: Vec<SuitePlan>) -> PlanDraft {
        PlanDraft {
            workspace_id: WorkspaceId::try_new("ws-test").unwrap(),
            snapshot_id: SourceSnapshotId::new(Fingerprint::of_bytes(b"snapshot")),
            baseline_commit: mode
                .needs_baseline()
                .then(|| BaselineId::try_new("a".repeat(40)).unwrap()),
            config_digest: Fingerprint::of_bytes(b"config"),
            mode,
            scope_limited: false,
            changed_paths: mode.needs_baseline().then(Vec::new),
            suites,
            fallbacks: Vec::new(),
            resources: ResourceBudget::try_new(4, 4096, 2).unwrap(),
            diagnostics: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{draft, path, suite_plan};
    use super::*;
    use crate::selection::OmitReason;

    fn files(names: &[&str]) -> Selection {
        Selection::files(names.iter().map(|name| path(name))).unwrap()
    }

    /// G18: the same inputs enumerated in a different order give the same plan.
    #[test]
    fn g18_canonical_plan_is_independent_of_enumeration_order() {
        let mut first = draft(
            Mode::Affected,
            vec![
                suite_plan("api", files(&["a.test.ts", "b.test.ts"])),
                suite_plan("web", Selection::All),
            ],
        );
        first.changed_paths = Some(vec![path("src/b.ts"), path("src/a.ts")]);
        first.suites[0].reasons = vec![
            SelectionReason::DirectChange {
                path: path("b.test.ts"),
            },
            SelectionReason::DirectChange {
                path: path("a.test.ts"),
            },
        ];
        first.fallbacks = vec![
            FallbackReason::NoRelatedTests { path: path("z") },
            FallbackReason::NoRelatedTests { path: path("y") },
        ];

        let mut second = draft(
            Mode::Affected,
            vec![
                suite_plan("web", Selection::All),
                suite_plan("api", files(&["b.test.ts", "a.test.ts"])),
            ],
        );
        second.changed_paths = Some(vec![path("src/a.ts"), path("src/b.ts"), path("src/a.ts")]);
        second.suites[1].reasons = vec![
            SelectionReason::DirectChange {
                path: path("a.test.ts"),
            },
            SelectionReason::DirectChange {
                path: path("b.test.ts"),
            },
            SelectionReason::DirectChange {
                path: path("a.test.ts"),
            },
        ];
        second.fallbacks = vec![
            FallbackReason::NoRelatedTests { path: path("y") },
            FallbackReason::NoRelatedTests { path: path("z") },
        ];

        let first = ExecutionPlan::try_new(first).unwrap();
        let second = ExecutionPlan::try_new(second).unwrap();
        assert_eq!(first.id(), second.id());
        assert_eq!(first.to_json().unwrap(), second.to_json().unwrap());
    }

    #[test]
    fn plan_id_changes_with_content() {
        let all =
            ExecutionPlan::try_new(draft(Mode::Full, vec![suite_plan("api", Selection::All)]))
                .unwrap();
        let mut other = draft(Mode::Full, vec![suite_plan("api", Selection::All)]);
        other.config_digest = Fingerprint::of_bytes(b"other config");
        assert_ne!(all.id(), ExecutionPlan::try_new(other).unwrap().id());
    }

    #[test]
    fn assurance_is_full_suite_only_for_unscoped_full_or_shadow() {
        let full =
            ExecutionPlan::try_new(draft(Mode::Full, vec![suite_plan("api", Selection::All)]))
                .unwrap();
        assert_eq!(full.assurance(), Assurance::FullSuite);

        let mut scoped = draft(Mode::Full, vec![suite_plan("api", Selection::All)]);
        scoped.scope_limited = true;
        assert_eq!(
            ExecutionPlan::try_new(scoped).unwrap().assurance(),
            Assurance::Advisory
        );

        let affected = draft(Mode::Affected, vec![suite_plan("api", Selection::All)]);
        assert_eq!(
            ExecutionPlan::try_new(affected).unwrap().assurance(),
            Assurance::Advisory
        );
    }

    #[test]
    fn affected_and_shadow_need_a_baseline() {
        for mode in [Mode::Affected, Mode::Shadow] {
            let mut missing = draft(mode, vec![suite_plan("api", Selection::All)]);
            missing.baseline_commit = None;
            assert!(matches!(
                ExecutionPlan::try_new(missing),
                Err(PlanError::MissingBaseline { .. })
            ));
        }
        let full = draft(Mode::Full, vec![suite_plan("api", Selection::All)]);
        assert!(full.baseline_commit.is_none());
        assert!(ExecutionPlan::try_new(full).is_ok());
    }

    #[test]
    fn full_and_shadow_must_execute_everything() {
        let narrowed = draft(Mode::Full, vec![suite_plan("api", files(&["a.test.ts"]))]);
        assert!(matches!(
            ExecutionPlan::try_new(narrowed),
            Err(PlanError::NotExhaustive { .. })
        ));

        let mut shadow = suite_plan("api", Selection::All);
        shadow.candidate = Some(CandidatePlan {
            selection: files(&["a.test.ts"]),
            reasons: Vec::new(),
        });
        assert!(ExecutionPlan::try_new(draft(Mode::Shadow, vec![shadow])).is_ok());

        let without_candidate = draft(Mode::Shadow, vec![suite_plan("api", Selection::All)]);
        assert!(matches!(
            ExecutionPlan::try_new(without_candidate),
            Err(PlanError::MissingCandidate(_))
        ));

        let mut stray = suite_plan("api", Selection::All);
        stray.candidate = Some(CandidatePlan {
            selection: Selection::All,
            reasons: Vec::new(),
        });
        assert!(matches!(
            ExecutionPlan::try_new(draft(Mode::Affected, vec![stray])),
            Err(PlanError::UnexpectedCandidate(_))
        ));
    }

    #[test]
    fn effective_selection_can_only_widen() {
        let mut narrowed = suite_plan("api", Selection::All);
        narrowed.effective_selection = files(&["a.test.ts"]);
        narrowed.capability_verification = CapabilityVerification::Pending;
        assert!(matches!(
            ExecutionPlan::try_new(draft(Mode::Affected, vec![narrowed])),
            Err(PlanError::EffectiveSelectionNarrower(_))
        ));

        let mut widened = suite_plan("api", files(&["a.test.ts"]));
        widened.effective_selection = Selection::All;
        widened.capability_verification = CapabilityVerification::NotRequired;
        assert!(ExecutionPlan::try_new(draft(Mode::Affected, vec![widened])).is_ok());
    }

    #[test]
    fn rejects_empty_and_duplicate_suites() {
        assert_eq!(
            ExecutionPlan::try_new(draft(Mode::Full, Vec::new())),
            Err(PlanError::NoSuites)
        );
        let duplicated = draft(
            Mode::Full,
            vec![
                suite_plan("api", Selection::All),
                suite_plan("api", Selection::All),
            ],
        );
        assert!(matches!(
            ExecutionPlan::try_new(duplicated),
            Err(PlanError::DuplicateSuite(_))
        ));
    }

    #[test]
    fn a_plan_without_work_is_detectable() {
        let omit = Selection::Omit {
            reason: OmitReason::NoChanges,
        };
        let plan =
            ExecutionPlan::try_new(draft(Mode::Affected, vec![suite_plan("api", omit)])).unwrap();
        assert!(!plan.has_work());
    }

    #[test]
    fn json_round_trip_keeps_the_plan() {
        let mut suite = suite_plan("api", files(&["a.test.ts"]));
        suite.reasons = vec![SelectionReason::TransitiveDependency {
            dependency_path: vec![path("a.test.ts"), path("a.ts")],
        }];
        let mut draft = draft(Mode::Affected, vec![suite]);
        draft.changed_paths = Some(vec![path("a.ts")]);
        let plan = ExecutionPlan::try_new(draft).unwrap();
        let json = plan.to_json().unwrap();
        let read = ExecutionPlan::from_json(json.as_bytes()).unwrap();
        assert_eq!(read, plan);

        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["mode"], "affected");
        assert_eq!(value["assurance"], "advisory");
        assert_eq!(value["suites"][0]["requested_selection"]["kind"], "files");
        assert_eq!(
            value["suites"][0]["reasons"][0]["code"],
            "transitive_dependency"
        );
        assert_eq!(value["suites"][0]["capability_verification"], "pending");
        assert!(value["plan_id"].as_str().unwrap().starts_with("blake3:"));
    }

    #[test]
    fn other_schema_versions_are_rejected() {
        let plan =
            ExecutionPlan::try_new(draft(Mode::Full, vec![suite_plan("api", Selection::All)]))
                .unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&plan.to_json().unwrap()).unwrap();
        value["schema_version"] = serde_json::json!(2);
        assert_eq!(
            ExecutionPlan::from_json(value.to_string().as_bytes()),
            Err(PlanDocumentError::UnsupportedSchemaVersion {
                found: 2,
                supported: 1
            })
        );
        value.as_object_mut().unwrap().remove("schema_version");
        assert_eq!(
            ExecutionPlan::from_json(value.to_string().as_bytes()),
            Err(PlanDocumentError::MissingSchemaVersion)
        );
        assert!(matches!(
            ExecutionPlan::from_json(b"not json"),
            Err(PlanDocumentError::Malformed(_))
        ));
    }

    #[test]
    fn tampered_plans_are_rejected() {
        let plan = ExecutionPlan::try_new(draft(
            Mode::Affected,
            vec![suite_plan("api", files(&["a.test.ts", "b.test.ts"]))],
        ))
        .unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&plan.to_json().unwrap()).unwrap();
        value["suites"][0]["requested_selection"]["files"] = serde_json::json!(["a.test.ts"]);
        value["suites"][0]["effective_selection"]["files"] = serde_json::json!(["a.test.ts"]);
        assert_eq!(
            ExecutionPlan::from_json(value.to_string().as_bytes()),
            Err(PlanDocumentError::IdMismatch)
        );

        let mut empty: serde_json::Value = serde_json::from_str(&plan.to_json().unwrap()).unwrap();
        empty["suites"][0]["effective_selection"]["files"] = serde_json::json!([]);
        assert!(matches!(
            ExecutionPlan::from_json(empty.to_string().as_bytes()),
            Err(PlanDocumentError::Malformed(_))
        ));
    }
}
