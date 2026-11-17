//! Ports used to execute a plan (spec section 5): runners, execution
//! environments, events and run storage.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use testry_domain::environment::{LeaseState, ProviderKind};
use testry_domain::fingerprint::Fingerprint;
use testry_domain::ids::{EnvironmentLeaseId, PlanId, RunId, SuiteId, TaskId, WorkspaceId};
use testry_domain::paths::{RepoDir, RepoPath};
use testry_domain::plan::Mode;
use testry_domain::results::{
    Assurance, CaseResult, ProcessExit, RunOutcome, TaskOutcome, TestCounts,
};
use testry_domain::selection::Selection;
use testry_domain::suite::{RunnerKind, SuiteDefinition};

use super::{OperationContext, PortError, PortFuture};

// ---------------------------------------------------------------- runners --

/// The version of a tool as observed in the execution environment.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ToolVersion {
    pub tool: String,
    pub version: String,
}

/// What a runner adapter can do for one observed tool version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunnerCapabilities {
    pub full_run: bool,
    /// The command line can select test files such that every requested file
    /// runs. The runner may run more files, never fewer.
    pub file_selection: bool,
    /// The structured report attributes every case to its file.
    pub file_attribution: bool,
    pub structured_report: bool,
    pub worker_control: bool,
    /// Whether this exact version is listed in `compatibility.toml`.
    pub version_verified: bool,
}

/// How workspace paths translate between testry and the environment in which
/// commands run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceMapping {
    pub local_root: PathBuf,
    pub execution_root: PathBuf,
}

/// The environment a command actually ran in. Recorded with every result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionDescriptor {
    pub provider: ProviderKind,
    pub os: String,
    pub arch: String,
    pub tool_versions: BTreeMap<String, String>,
    pub workspace: WorkspaceMapping,
    /// Image ID or digest that was really used, not just its name.
    pub image: Option<String>,
    pub capabilities: BTreeSet<String>,
}

/// Where a task writes its artifacts, as seen from the execution environment.
/// The paths are unique per run, so a report left behind by an earlier run can
/// never be read as the new result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactLocations {
    pub task_directory: PathBuf,
    pub report_path: PathBuf,
}

/// Whether a report may contain files beyond the expected ones. Running more
/// than requested is always allowed; this records whether it was expected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdditionalFiles {
    /// The runner's file filter may match more files, or the whole suite runs.
    Expected,
    /// The file filter is exact; extra files are reported as an anomaly.
    Unexpected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportFormat {
    BunJunitXml,
    VitestJson,
    PlaywrightJson,
    /// The adapter has no structured report (`command`).
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedInvocation {
    pub suite_id: SuiteId,
    pub task_id: TaskId,
    pub requested_selection: Selection,
    /// Never narrower than `requested_selection`.
    pub effective_selection: Selection,
    /// Why the adapter widened the selection, if it did.
    pub widened_because: Option<String>,
    /// Fixed argv. Never a shell string.
    pub argv: Vec<String>,
    /// Environment variables the adapter owns (for example to forbid focused
    /// tests). They are added to the allowlisted environment.
    pub env: BTreeMap<String, String>,
    /// Working directory relative to the workspace.
    pub cwd: RepoDir,
    /// The workspace root as the runner sees it. Reports with absolute paths
    /// are mapped back to repository paths through it.
    pub workspace_root: PathBuf,
    /// Files that must appear in the report. `None` when the adapter cannot
    /// name them (whole suite) or the runner cannot attribute cases to files.
    pub expected_files: Option<BTreeSet<RepoPath>>,
    pub report_format: ReportFormat,
    pub report_path: Option<PathBuf>,
    pub additional_files: AdditionalFiles,
}

/// A file below the artifact root of a run.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ArtifactRef {
    pub relative_path: PathBuf,
}

/// Facts about how a process ended that are not part of its exit status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProcessNotes {
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    /// The command exited but left processes behind; they were stopped.
    pub stopped_survivors: bool,
    /// Processes of the command could not be confirmed gone.
    pub residual_processes: bool,
    /// Output may be missing because a pipe stayed open.
    pub output_incomplete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessResult {
    pub exit: ProcessExit,
    /// Wall-clock times for display (UTC).
    pub started_at: SystemTime,
    pub finished_at: SystemTime,
    /// Measured with a monotonic clock.
    pub duration: Duration,
    pub stdout: ArtifactRef,
    pub stderr: ArtifactRef,
    pub notes: ProcessNotes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportContent {
    /// Size-checked report bytes.
    Bytes(Vec<u8>),
    Missing,
    TooLarge {
        limit_bytes: u64,
    },
    Unreadable {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportBundle {
    pub format: ReportFormat,
    pub content: ReportContent,
}

/// Which files a suite executed. `Unknown` is explicit; it is never encoded
/// as an empty list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileAttribution {
    Known(BTreeSet<RepoPath>),
    Unknown,
}

/// Something about a task that a reader of the result must know. The code is
/// stable and machine readable.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Anomaly {
    pub code: &'static str,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuiteResult {
    pub suite_id: SuiteId,
    pub task_id: TaskId,
    pub outcome: TaskOutcome,
    pub executed_files: FileAttribution,
    /// `None` when the runner cannot report counts. Never invented.
    pub counts: Option<TestCounts>,
    /// testry never re-runs a task in v0.1, so this is 1 for an executed task.
    pub attempts: u32,
    pub cases: Vec<CaseResult>,
    pub anomalies: Vec<Anomaly>,
    pub duration: Duration,
    pub artifacts: Vec<ArtifactRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdapterError {
    #[error("configuration error: {message}")]
    Configuration { message: String },
    #[error("unsupported capability `{capability}`: {message}")]
    UnsupportedCapability { capability: String, message: String },
    #[error("invalid report: {message}")]
    InvalidReport { message: String },
}

impl From<AdapterError> for PortError {
    fn from(error: AdapterError) -> Self {
        match error {
            AdapterError::Configuration { message } => Self::Configuration { message },
            AdapterError::UnsupportedCapability {
                capability,
                message,
            } => Self::UnsupportedCapability {
                capability,
                message,
            },
            AdapterError::InvalidReport { message } => Self::InvalidReport { message },
        }
    }
}

pub trait RunnerAdapter: Send + Sync {
    fn kind(&self) -> RunnerKind;

    /// The argv that prints the runner version, or `None` when the adapter
    /// has no version to check (`command`).
    fn version_command(&self, suite: &SuiteDefinition) -> Option<Vec<String>>;

    /// Extracts the version from the output of [`Self::version_command`].
    fn parse_version(&self, output: &str) -> Option<ToolVersion>;

    fn capabilities(&self, version: Option<&ToolVersion>) -> RunnerCapabilities;

    /// Pure command construction. No subprocess or network access here. It
    /// may widen a selection; it must never narrow one silently.
    fn prepare(
        &self,
        suite: &SuiteDefinition,
        selection: &Selection,
        environment: &ExecutionDescriptor,
        artifacts: &ArtifactLocations,
    ) -> Result<PreparedInvocation, AdapterError>;

    /// Normalizes the structured report. Console output is never parsed as
    /// the source of truth, and exit code 0 alone is not a success.
    fn normalize(
        &self,
        invocation: &PreparedInvocation,
        process: &ProcessResult,
        reports: &ReportBundle,
    ) -> Result<SuiteResult, AdapterError>;
}

// ----------------------------------------------------------- environments --

/// Who owns an environment. Cleanup only touches resources whose persisted
/// record and real labels both match this ownership.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseOwnership {
    pub workspace_id: WorkspaceId,
    pub run_id: RunId,
    pub lease_id: EnvironmentLeaseId,
}

/// A workspace trust decision, stored outside the repository so a repository
/// cannot approve itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustApproval {
    pub approved_config_digest: Fingerprint,
    pub allow_initialize_command: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentRequest {
    pub provider: ProviderKind,
    pub workspace_root: PathBuf,
    pub config_fingerprint: Fingerprint,
    pub ownership: LeaseOwnership,
    /// argv arrays that must succeed before any test starts.
    pub readiness_commands: Vec<Vec<String>>,
    pub trust: Option<TrustApproval>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentLease {
    pub lease_id: EnvironmentLeaseId,
    pub owner_run_id: RunId,
    pub provider: ProviderKind,
    /// Identifiers of the resources this lease owns exclusively.
    pub owned_resources: Vec<String>,
    pub descriptor: ExecutionDescriptor,
    pub state: LeaseState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputPolicy {
    /// Artifact paths relative to the artifact root of the run.
    pub stdout: ArtifactRef,
    pub stderr: ArtifactRef,
    /// Bytes kept per stream. Output beyond the limit is still drained and
    /// the truncation is recorded.
    pub max_bytes_per_stream: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    /// Relative to the workspace.
    pub cwd: RepoDir,
    /// Variables added to the allowlisted environment of the provider. The
    /// environment of testry itself is not inherited.
    pub env: BTreeMap<String, String>,
    pub timeout: Duration,
    pub output: OutputPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseDisposition {
    Completed,
    Cancelled,
    Failed,
}

/// Cleanup runs under its own deadline. A cancelled run still cleans up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupContext {
    pub deadline: Instant,
    pub ownership: LeaseOwnership,
    pub max_retries: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CleanupReport {
    pub removed_resources: Vec<String>,
    pub residual_resources: Vec<String>,
    pub failures: Vec<String>,
    /// Lets `gc` retry the cleanup after re-checking ownership.
    pub cleanup_token: Option<String>,
}

impl CleanupReport {
    pub fn is_complete(&self) -> bool {
        self.residual_resources.is_empty() && self.failures.is_empty()
    }
}

pub trait RuntimeProvider: Send + Sync {
    fn prepare<'a>(
        &'a self,
        request: &'a EnvironmentRequest,
        ctx: &'a OperationContext,
    ) -> PortFuture<'a, EnvironmentLease>;

    fn execute<'a>(
        &'a self,
        lease: &'a EnvironmentLease,
        command: &'a CommandSpec,
        events: &'a dyn EventSink,
        ctx: &'a OperationContext,
    ) -> PortFuture<'a, ProcessResult>;

    fn release<'a>(
        &'a self,
        lease: EnvironmentLease,
        disposition: ReleaseDisposition,
        cleanup: &'a CleanupContext,
    ) -> PortFuture<'a, CleanupReport>;
}

// -------------------------------------------------------------- artifacts --

/// The artifacts of one task, relative to the artifact root of the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskArtifacts {
    /// Where the runner writes, as seen from the execution environment.
    pub locations: ArtifactLocations,
    pub report: ArtifactRef,
    pub stdout: ArtifactRef,
    pub stderr: ArtifactRef,
    /// Output of the version probe that runs before the task.
    pub probe_stdout: ArtifactRef,
    pub probe_stderr: ArtifactRef,
}

/// Access to the artifact directory of one run.
pub trait ArtifactStore: Send + Sync {
    /// Creates the directory of a task and names its artifacts.
    fn task_artifacts<'a>(
        &'a self,
        task: &'a TaskId,
        report_file_name: &'a str,
    ) -> PortFuture<'a, TaskArtifacts>;

    /// Reads the report a runner wrote. A missing, oversized or unreadable
    /// report is described in the bundle; it is not an error of this call.
    fn read_report<'a>(
        &'a self,
        invocation: &'a PreparedInvocation,
    ) -> PortFuture<'a, ReportBundle>;

    /// Reads a small text artifact, for example the output of a version probe.
    fn read_text<'a>(&'a self, artifact: &'a ArtifactRef, max_bytes: u64)
    -> PortFuture<'a, String>;
}

// ----------------------------------------------------------------- events --

/// What happened in a run. The sink that persists events is the single writer
/// that assigns the monotonically increasing `seq` and the UTC timestamp.
/// Payloads reference artifacts; they never embed log text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunEventPayload {
    RunStarted {
        mode: Mode,
    },
    PlanCreated {
        plan_id: PlanId,
    },
    SelectionExpanded {
        suite_id: SuiteId,
        from: &'static str,
        to: &'static str,
        reason_code: String,
    },
    EnvironmentPreparing {
        provider: ProviderKind,
    },
    EnvironmentReady {
        provider: ProviderKind,
    },
    TaskStarted {
        task_id: TaskId,
    },
    TaskFinished {
        task_id: TaskId,
        outcome: TaskOutcome,
    },
    CleanupStarted,
    CleanupFinished {
        complete: bool,
    },
    RunFinished {
        outcome: RunOutcome,
        exit_code: u8,
    },
}

impl RunEventPayload {
    /// The `type` field of the JSONL event contract.
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::RunStarted { .. } => "run_started",
            Self::PlanCreated { .. } => "plan_created",
            Self::SelectionExpanded { .. } => "selection_expanded",
            Self::EnvironmentPreparing { .. } => "environment_preparing",
            Self::EnvironmentReady { .. } => "environment_ready",
            Self::TaskStarted { .. } => "task_started",
            Self::TaskFinished { .. } => "task_finished",
            Self::CleanupStarted => "cleanup_started",
            Self::CleanupFinished { .. } => "cleanup_finished",
            Self::RunFinished { .. } => "run_finished",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunEvent {
    pub run_id: RunId,
    pub payload: RunEventPayload,
}

pub trait EventSink: Send + Sync {
    fn append<'a>(&'a self, event: RunEvent) -> PortFuture<'a, ()>;
}

// ------------------------------------------------------------------ store --

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRecord {
    pub run_id: RunId,
    pub plan_id: PlanId,
    /// The plan that is executed. It is stored with the run so that the
    /// reasons of the selection and what really ran can be compared later.
    pub plan: testry_domain::plan::ExecutionPlan,
    pub started_at: SystemTime,
    pub mode: Mode,
    pub scope: Vec<SuiteId>,
    pub workspace_id: WorkspaceId,
    /// The environment lease this run will use, and its provider. The lease
    /// is recorded before the environment exists, so a run that stops in
    /// between can be found again.
    pub lease_id: EnvironmentLeaseId,
    pub provider: ProviderKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RunMetrics {
    pub planning_ms: u64,
    pub environment_ready_ms: u64,
    pub first_failure_ms: Option<u64>,
    pub verification_ms: u64,
    pub cleanup_ms: u64,
    /// Wall clock of the whole run, not the sum of suite durations.
    pub total_ms: u64,
}

/// Whether the inputs were the same before and after execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputValidation {
    Unchanged,
    /// Inputs changed; the result does not describe the current code.
    Changed,
    /// The check could not run (for example the run was cancelled early).
    NotChecked,
}

/// A policy rule that a suite violated or that needs attention.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PolicyFinding {
    pub code: &'static str,
    pub detail: String,
    /// Whether the finding turned the suite into a failure.
    pub fails_suite: bool,
}

/// A failure of a `shadow` run in a test file that the affected candidate
/// would not have executed. It is a hint that the selection may have a gap;
/// it does not prove one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ShadowGap {
    pub suite_id: SuiteId,
    /// `None` when the runner cannot attribute failures to files.
    pub file: Option<RepoPath>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuiteRunResult {
    pub result: SuiteResult,
    pub effective_selection: Selection,
    pub runner: RunnerKind,
    pub findings: Vec<PolicyFinding>,
    /// The structured report of the runner, if there is one.
    pub report: Option<ArtifactRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunResult {
    pub run_id: RunId,
    pub plan_id: PlanId,
    pub mode: Mode,
    pub scope: Vec<SuiteId>,
    pub suites: Vec<SuiteRunResult>,
    pub assurance: Assurance,
    pub outcome: RunOutcome,
    pub input_validation: InputValidation,
    pub environment: Option<ExecutionDescriptor>,
    pub shadow_gaps: Option<Vec<ShadowGap>>,
    pub metrics: RunMetrics,
    pub cleanup: CleanupReport,
    pub exit_code: u8,
}

pub trait RunStore: Send + Sync {
    fn create<'a>(&'a self, record: &'a RunRecord) -> PortFuture<'a, ()>;

    /// A run only counts as finished once this terminal record is stored. A
    /// run without it is read back as interrupted, never as successful.
    fn finish<'a>(&'a self, result: &'a RunResult) -> PortFuture<'a, ()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_errors_keep_their_classification() {
        let configuration: PortError = AdapterError::Configuration {
            message: "m".to_owned(),
        }
        .into();
        assert_eq!(configuration.code(), "configuration");
        let report: PortError = AdapterError::InvalidReport {
            message: "m".to_owned(),
        }
        .into();
        assert_eq!(report.code(), "invalid_report");
        let capability: PortError = AdapterError::UnsupportedCapability {
            capability: "file_selection".to_owned(),
            message: "m".to_owned(),
        }
        .into();
        assert_eq!(capability.code(), "unsupported_capability");
    }

    /// R08: a cleanup with leftovers is not complete.
    #[test]
    fn r08_cleanup_with_residual_resources_is_not_complete() {
        assert!(CleanupReport::default().is_complete());
        let residual = CleanupReport {
            residual_resources: vec!["container:abc".to_owned()],
            ..CleanupReport::default()
        };
        assert!(!residual.is_complete());
        let failed = CleanupReport {
            failures: vec!["timeout".to_owned()],
            ..CleanupReport::default()
        };
        assert!(!failed.is_complete());
    }

    #[test]
    fn event_types_match_the_minimum_event_list() {
        let run_finished = RunEventPayload::RunFinished {
            outcome: RunOutcome::NoWork,
            exit_code: 4,
        };
        assert_eq!(run_finished.event_type(), "run_finished");
        assert_eq!(
            RunEventPayload::CleanupStarted.event_type(),
            "cleanup_started"
        );
    }
}
