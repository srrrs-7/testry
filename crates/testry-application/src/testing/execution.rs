//! Fakes of the execution ports: a runner, a runtime and an artifact store.
//!
//! Nothing here starts a process. The fake runtime answers each command with
//! a scripted exit status, and the fake runner reads a tiny line-based report
//! (`<file>|<title>|<status>`) so that tests exercise the real assembly and
//! policy code.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use testry_domain::environment::{LeaseState, ProviderKind};
use testry_domain::ids::TaskId;
use testry_domain::paths::RepoPath;
use testry_domain::results::{CaseResult, CaseStatus, ProcessExit};
use testry_domain::selection::Selection;
use testry_domain::suite::{RunnerKind, SuiteDefinition};

use super::lock;
use crate::ports::execution::{
    AdapterError, AdditionalFiles, ArtifactLocations, ArtifactRef, ArtifactStore, CleanupContext,
    CleanupReport, CommandSpec, EnvironmentLease, EnvironmentRequest, EventSink,
    ExecutionDescriptor, PreparedInvocation, ProcessNotes, ProcessResult, ReleaseDisposition,
    ReportBundle, ReportContent, ReportFormat, RunnerAdapter, RunnerCapabilities, RuntimeProvider,
    SuiteResult, TaskArtifacts, ToolVersion, WorkspaceMapping,
};
use crate::ports::{OperationContext, PortError, PortFuture};
use crate::results::{ParsedReport, ReportOutcome, assemble_suite_result};

/// The program name of the fake version probe.
pub const VERSION_PROBE: &str = "fake-version-probe";

/// A runner with scripted capabilities.
pub struct FakeRunner {
    pub kind: RunnerKind,
    pub capabilities: RunnerCapabilities,
    /// Whether the runner has a version to probe.
    pub has_version: bool,
    /// A command argument this adapter refuses, like a flag that narrows
    /// the run.
    pub rejected_argument: Option<&'static str>,
}

impl FakeRunner {
    /// A runner whose installed version is verified for everything.
    pub fn verified(kind: RunnerKind) -> Self {
        Self {
            kind,
            capabilities: RunnerCapabilities {
                full_run: true,
                file_selection: true,
                file_attribution: true,
                structured_report: true,
                worker_control: true,
                version_verified: true,
            },
            has_version: true,
            rejected_argument: None,
        }
    }

    /// A gate without a structured report.
    pub fn command() -> Self {
        Self {
            kind: RunnerKind::Command,
            capabilities: RunnerCapabilities {
                full_run: true,
                file_selection: false,
                file_attribution: false,
                structured_report: false,
                worker_control: false,
                version_verified: false,
            },
            has_version: false,
            rejected_argument: None,
        }
    }
}

fn parse_report(bytes: &[u8]) -> Result<ParsedReport, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "not UTF-8".to_owned())?;
    let mut report = ParsedReport::default();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let mut fields = line.split('|');
        let (Some(file), Some(title), Some(status), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Err(format!("malformed report line {line:?}"));
        };
        let file = RepoPath::try_new(file).map_err(|error| error.to_string())?;
        let status = match status {
            "passed" => CaseStatus::Passed,
            "failed" => CaseStatus::Failed,
            "skipped" => CaseStatus::Skipped,
            "todo" => CaseStatus::Todo,
            "flaky" => CaseStatus::Flaky,
            // A file that ran but has no case, as real runners report it.
            "file" => {
                report.files.insert(file);
                continue;
            }
            other => return Err(format!("unknown status {other:?}")),
        };
        report.files.insert(file.clone());
        report.cases.push(CaseResult {
            project: None,
            file: Some(file),
            title_path: vec![title.to_owned()],
            status,
            failure: (status == CaseStatus::Failed).then(|| "assertion failed".to_owned()),
        });
    }
    Ok(report)
}

impl RunnerAdapter for FakeRunner {
    fn kind(&self) -> RunnerKind {
        self.kind
    }

    fn version_command(&self, _suite: &SuiteDefinition) -> Option<Vec<String>> {
        self.has_version
            .then(|| vec![VERSION_PROBE.to_owned(), "--version".to_owned()])
    }

    fn parse_version(&self, output: &str) -> Option<ToolVersion> {
        let version = output.trim();
        (!version.is_empty()).then(|| ToolVersion {
            tool: self.kind.as_str().to_owned(),
            version: version.to_owned(),
        })
    }

    fn capabilities(&self, version: Option<&ToolVersion>) -> RunnerCapabilities {
        let mut capabilities = self.capabilities;
        if self.has_version && version.is_none() {
            // An unreadable version is never treated as verified.
            capabilities.file_selection = false;
            capabilities.version_verified = false;
        }
        capabilities
    }

    fn prepare(
        &self,
        suite: &SuiteDefinition,
        selection: &Selection,
        environment: &ExecutionDescriptor,
        artifacts: &ArtifactLocations,
    ) -> Result<PreparedInvocation, AdapterError> {
        if let Some(rejected) = self.rejected_argument
            && suite.command().iter().any(|argument| argument == rejected)
        {
            return Err(AdapterError::Configuration {
                message: format!("the suite command of {} contains `{rejected}`", suite.id()),
            });
        }
        let mut argv = suite.command().to_vec();
        let expected_files = match selection {
            Selection::All => None,
            Selection::Files { files } => {
                argv.extend(files.iter().map(|file| file.as_str().to_owned()));
                Some(files.as_set().clone())
            }
            Selection::Omit { .. } => {
                return Err(AdapterError::Configuration {
                    message: "an omitted suite has no invocation".to_owned(),
                });
            }
        };
        let structured = self.capabilities.structured_report;
        Ok(PreparedInvocation {
            suite_id: suite.id().clone(),
            task_id: TaskId::for_suite(suite.id()),
            requested_selection: selection.clone(),
            effective_selection: selection.clone(),
            widened_because: None,
            argv,
            env: BTreeMap::new(),
            cwd: suite.cwd().clone(),
            workspace_root: environment.workspace.execution_root.clone(),
            expected_files,
            report_format: if structured {
                ReportFormat::VitestJson
            } else {
                ReportFormat::None
            },
            report_path: structured.then(|| artifacts.report_path.clone()),
            additional_files: AdditionalFiles::Unexpected,
        })
    }

    fn normalize(
        &self,
        invocation: &PreparedInvocation,
        process: &ProcessResult,
        reports: &ReportBundle,
    ) -> Result<SuiteResult, AdapterError> {
        let report = if !self.capabilities.structured_report {
            ReportOutcome::NotApplicable
        } else {
            match &reports.content {
                ReportContent::Bytes(bytes) => match parse_report(bytes) {
                    Ok(parsed) => ReportOutcome::Parsed(parsed),
                    Err(problem) => ReportOutcome::invalid(problem),
                },
                other => ReportOutcome::content_problem(other)
                    .unwrap_or_else(|| ReportOutcome::invalid("no content")),
            }
        };
        Ok(assemble_suite_result(invocation, process, report))
    }
}

type Hook = Box<dyn Fn(&CommandSpec) + Send + Sync>;

/// A runtime that starts nothing. Commands are answered with scripted exit
/// statuses, keyed by the task directory of their stdout artifact.
pub struct FakeRuntime {
    pub provider: ProviderKind,
    pub prepare_error: Option<PortError>,
    pub release_report: Result<CleanupReport, PortError>,
    /// How long each task appears to run.
    pub task_duration: Duration,
    exits: Mutex<BTreeMap<String, ProcessExit>>,
    spawn_failures: Mutex<BTreeSet<String>>,
    hook: Option<Hook>,
    executed: Mutex<Vec<CommandSpec>>,
    prepared: Mutex<Vec<EnvironmentRequest>>,
    released: Mutex<Vec<ReleaseDisposition>>,
    running: Mutex<usize>,
    max_running: Mutex<usize>,
}

impl Default for FakeRuntime {
    fn default() -> Self {
        Self {
            provider: ProviderKind::Host,
            prepare_error: None,
            release_report: Ok(CleanupReport::default()),
            task_duration: Duration::ZERO,
            exits: Mutex::new(BTreeMap::new()),
            spawn_failures: Mutex::new(BTreeSet::new()),
            hook: None,
            executed: Mutex::new(Vec::new()),
            prepared: Mutex::new(Vec::new()),
            released: Mutex::new(Vec::new()),
            running: Mutex::new(0),
            max_running: Mutex::new(0),
        }
    }
}

/// The task a command belongs to, from its stdout artifact
/// (`tasks/<task>/stdout.log`).
fn task_of(command: &CommandSpec) -> String {
    command
        .output
        .stdout
        .relative_path
        .parent()
        .and_then(|directory| directory.file_name())
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_owned()
}

fn is_probe(command: &CommandSpec) -> bool {
    command.program == VERSION_PROBE
}

impl FakeRuntime {
    /// The exit status of the task of `suite`. The default is exit code 0.
    pub fn exit(self, suite: &str, exit: ProcessExit) -> Self {
        lock(&self.exits).insert(suite.to_owned(), exit);
        self
    }

    /// Makes the command of `suite` fail to start.
    pub fn spawn_failure(self, suite: &str) -> Self {
        lock(&self.spawn_failures).insert(suite.to_owned());
        self
    }

    /// Called with every task command (not with version probes) before it
    /// "runs".
    pub fn on_task(mut self, hook: impl Fn(&CommandSpec) + Send + Sync + 'static) -> Self {
        self.hook = Some(Box::new(hook));
        self
    }

    /// Task commands in the order they started, without version probes.
    pub fn tasks(&self) -> Vec<CommandSpec> {
        lock(&self.executed)
            .iter()
            .filter(|command| !is_probe(command))
            .cloned()
            .collect()
    }

    /// The suites whose task command started, in order.
    pub fn started_suites(&self) -> Vec<String> {
        self.tasks().iter().map(task_of).collect()
    }

    pub fn probes(&self) -> usize {
        lock(&self.executed)
            .iter()
            .filter(|command| is_probe(command))
            .count()
    }

    pub fn prepared(&self) -> Vec<EnvironmentRequest> {
        lock(&self.prepared).clone()
    }

    pub fn released(&self) -> Vec<ReleaseDisposition> {
        lock(&self.released).clone()
    }

    /// The highest number of task commands that ran at the same time.
    pub fn max_parallel_tasks(&self) -> usize {
        *lock(&self.max_running)
    }
}

impl RuntimeProvider for FakeRuntime {
    fn prepare<'a>(
        &'a self,
        request: &'a EnvironmentRequest,
        _ctx: &'a OperationContext,
    ) -> PortFuture<'a, EnvironmentLease> {
        lock(&self.prepared).push(request.clone());
        let response = match &self.prepare_error {
            Some(error) => Err(error.clone()),
            None => Ok(EnvironmentLease {
                lease_id: request.ownership.lease_id.clone(),
                owner_run_id: request.ownership.run_id.clone(),
                provider: self.provider,
                owned_resources: Vec::new(),
                descriptor: ExecutionDescriptor {
                    provider: self.provider,
                    os: "linux".to_owned(),
                    arch: "aarch64".to_owned(),
                    tool_versions: BTreeMap::new(),
                    workspace: WorkspaceMapping {
                        local_root: request.workspace_root.clone(),
                        execution_root: request.workspace_root.clone(),
                    },
                    image: None,
                    capabilities: BTreeSet::new(),
                },
                state: LeaseState::Ready,
            }),
        };
        Box::pin(async move { response })
    }

    fn execute<'a>(
        &'a self,
        _lease: &'a EnvironmentLease,
        command: &'a CommandSpec,
        _events: &'a dyn EventSink,
        ctx: &'a OperationContext,
    ) -> PortFuture<'a, ProcessResult> {
        Box::pin(async move {
            if ctx.cancellation.is_cancelled() {
                return Err(PortError::Cancelled);
            }
            lock(&self.executed).push(command.clone());
            let task = task_of(command);
            let probe = is_probe(command);
            if !probe {
                if lock(&self.spawn_failures).contains(&task) {
                    return Err(PortError::Spawn {
                        program: command.program.clone(),
                        message: "not found".to_owned(),
                    });
                }
                if let Some(hook) = &self.hook {
                    hook(command);
                }
                let mut running = lock(&self.running);
                *running += 1;
                let mut max_running = lock(&self.max_running);
                *max_running = (*max_running).max(*running);
            }
            if !probe && !self.task_duration.is_zero() {
                // A scripted duration without a timer: yield until it passed.
                let deadline = std::time::Instant::now() + self.task_duration;
                while std::time::Instant::now() < deadline {
                    YieldNow::default().await;
                }
            }
            let exit = if ctx.cancellation.is_cancelled() {
                ProcessExit::Cancelled
            } else if probe {
                ProcessExit::Exited { code: 0 }
            } else {
                lock(&self.exits)
                    .get(&task)
                    .copied()
                    .unwrap_or(ProcessExit::Exited { code: 0 })
            };
            if !probe {
                *lock(&self.running) -= 1;
            }
            Ok(ProcessResult {
                exit,
                started_at: SystemTime::UNIX_EPOCH,
                finished_at: SystemTime::UNIX_EPOCH,
                duration: self.task_duration,
                stdout: command.output.stdout.clone(),
                stderr: command.output.stderr.clone(),
                notes: ProcessNotes::default(),
            })
        })
    }

    fn release<'a>(
        &'a self,
        _lease: EnvironmentLease,
        disposition: ReleaseDisposition,
        _cleanup: &'a CleanupContext,
    ) -> PortFuture<'a, CleanupReport> {
        lock(&self.released).push(disposition);
        let response = self.release_report.clone();
        Box::pin(async move { response })
    }
}

/// Yields to the executor once.
#[derive(Default)]
struct YieldNow {
    yielded: bool,
}

impl Future for YieldNow {
    type Output = ();

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        if self.yielded {
            std::task::Poll::Ready(())
        } else {
            self.yielded = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    }
}

/// Reports and probe outputs held in memory.
pub struct FakeArtifacts {
    /// What the version probe "prints".
    pub version_output: String,
    reports: Mutex<BTreeMap<String, ReportContent>>,
}

impl Default for FakeArtifacts {
    fn default() -> Self {
        Self {
            version_output: "1.0.0\n".to_owned(),
            reports: Mutex::new(BTreeMap::new()),
        }
    }
}

impl FakeArtifacts {
    /// Scripts the report of a suite as `<file>|<title>|<status>` lines.
    pub fn report(self, suite: &str, lines: &str) -> Self {
        lock(&self.reports).insert(
            suite.to_owned(),
            ReportContent::Bytes(lines.as_bytes().to_vec()),
        );
        self
    }

    /// Scripts a report problem. Without a script a report is `Missing`.
    pub fn report_content(self, suite: &str, content: ReportContent) -> Self {
        lock(&self.reports).insert(suite.to_owned(), content);
        self
    }
}

impl ArtifactStore for FakeArtifacts {
    fn task_artifacts<'a>(
        &'a self,
        task: &'a TaskId,
        report_file_name: &'a str,
    ) -> PortFuture<'a, TaskArtifacts> {
        let relative = |name: &str| ArtifactRef {
            relative_path: PathBuf::from(format!("tasks/{task}/{name}")),
        };
        let directory = PathBuf::from(format!("/artifacts/tasks/{task}"));
        let artifacts = TaskArtifacts {
            locations: ArtifactLocations {
                report_path: directory.join(report_file_name),
                task_directory: directory,
            },
            report: relative(report_file_name),
            stdout: relative("stdout.log"),
            stderr: relative("stderr.log"),
            probe_stdout: relative("version-stdout.log"),
            probe_stderr: relative("version-stderr.log"),
        };
        Box::pin(async move { Ok(artifacts) })
    }

    fn read_report<'a>(
        &'a self,
        invocation: &'a PreparedInvocation,
    ) -> PortFuture<'a, ReportBundle> {
        let content = lock(&self.reports)
            .get(invocation.suite_id.as_str())
            .cloned()
            .unwrap_or(ReportContent::Missing);
        let bundle = ReportBundle {
            format: invocation.report_format,
            content,
        };
        Box::pin(async move { Ok(bundle) })
    }

    fn read_text<'a>(
        &'a self,
        _artifact: &'a ArtifactRef,
        _max_bytes: u64,
    ) -> PortFuture<'a, String> {
        let text = self.version_output.clone();
        Box::pin(async move { Ok(text) })
    }
}
