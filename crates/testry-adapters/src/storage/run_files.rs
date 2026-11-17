//! The artifact directory of one run (spec section 13).
//!
//! ```text
//! <storage>/runs/<run-id>/
//! ├── run.json          written when the run starts
//! ├── plan.json         the executed plan
//! ├── events.jsonl      one event per line, seq assigned by this single writer
//! ├── result.json       the terminal record; absent while a run is unfinished
//! ├── environment.json
//! ├── junit.xml         unified report
//! └── tasks/<task-id>/{stdout.log, stderr.log, report.*}
//! ```
//!
//! A run without `result.json` was interrupted. It is never read as a
//! success. Every run has its own directory, so a report left behind by an
//! earlier run cannot be read as the result of a new one.
//!
//! The run history (`history.rs`) indexes these directories. It is written
//! after the files, and repaired from them by [`reconcile_history`].

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;
use testry_application::ports::execution::{
    ArtifactLocations, ArtifactRef, ArtifactStore, EventSink, PreparedInvocation, ReportBundle,
    ReportContent, RunEvent, RunEventPayload, RunRecord, RunResult, RunStore, TaskArtifacts,
};
use testry_application::ports::{PortError, PortFuture};
use testry_application::results::RunResultDocument;
use testry_domain::ids::{RunId, TaskId};
use testry_domain::paths::RepoPath;
use testry_domain::plan::ExecutionPlan;

use super::history::{
    ArtifactRow, HistoryDatabase, Reconciliation, RunFinish, RunStart, StoredState, TaskRow,
};
use super::private::{
    ensure_private_directory, ensure_storage_directory, storage_error, write_private_file,
};
use super::time::rfc3339_utc;
use crate::reports::junit;

/// Upper bound for a runner report that is read into memory.
pub const MAX_REPORT_BYTES: u64 = 64 * 1024 * 1024;

const RUNS_DIRECTORY: &str = "runs";
const RESULT_FILE: &str = "result.json";
const RUN_FILE: &str = "run.json";
const PLAN_FILE: &str = "plan.json";

struct EventWriter {
    file: Option<std::fs::File>,
    seq: u64,
}

pub struct RunDirectory {
    root: PathBuf,
    events: Mutex<EventWriter>,
    /// Also print every event line to stdout (`--format jsonl`).
    echo_events: bool,
    /// The index this run is recorded in, after its files.
    history: Option<HistoryDatabase>,
}

impl RunDirectory {
    /// Creates the directory of a new run. An existing directory for the same
    /// run ID is an error: a run never reuses artifacts.
    pub fn create(
        workspace_root: &Path,
        storage_directory: &RepoPath,
        run_id: &RunId,
        echo_events: bool,
    ) -> Result<Self, PortError> {
        let storage = ensure_storage_directory(workspace_root, storage_directory)?;
        let runs = storage.join(RUNS_DIRECTORY);
        ensure_private_directory(&runs)?;
        let root = runs.join(run_id.as_str());
        if std::fs::symlink_metadata(&root).is_ok() {
            return Err(PortError::Storage {
                message: format!("run directory {} already exists", root.display()),
            });
        }
        ensure_private_directory(&root)?;
        ensure_private_directory(&root.join("tasks"))?;
        Ok(Self {
            root,
            events: Mutex::new(EventWriter { file: None, seq: 0 }),
            echo_events,
            history: None,
        })
    }

    /// Also records the run in the history of the workspace.
    #[must_use]
    pub fn with_history(mut self, history: HistoryDatabase) -> Self {
        self.history = Some(history);
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolves an artifact reference. References are relative and cannot
    /// leave the run directory.
    fn resolve(&self, artifact: &ArtifactRef) -> Result<PathBuf, PortError> {
        let relative = &artifact.relative_path;
        let safe = relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
        if !safe || relative.as_os_str().is_empty() {
            return Err(PortError::Storage {
                message: format!(
                    "artifact reference {} is not inside the run directory",
                    relative.display()
                ),
            });
        }
        Ok(self.root.join(relative))
    }

    fn write_json(&self, name: &str, value: &impl serde::Serialize) -> Result<(), PortError> {
        let mut text = serde_json::to_string_pretty(value).map_err(|error| PortError::Storage {
            message: format!("{name} could not be serialized: {error}"),
        })?;
        text.push('\n');
        write_private_file(&self.root.join(name), text.as_bytes())
    }
}

fn event_payload(payload: &RunEventPayload) -> serde_json::Value {
    match payload {
        RunEventPayload::RunStarted { mode } => json!({ "mode": mode }),
        RunEventPayload::PlanCreated { plan_id } => json!({ "plan_id": plan_id }),
        RunEventPayload::SelectionExpanded {
            suite_id,
            from,
            to,
            reason_code,
        } => {
            json!({ "suite_id": suite_id, "from": from, "to": to, "reason_code": reason_code })
        }
        RunEventPayload::EnvironmentPreparing { provider }
        | RunEventPayload::EnvironmentReady { provider } => json!({ "provider": provider }),
        RunEventPayload::TaskStarted { task_id } => json!({ "task_id": task_id }),
        RunEventPayload::TaskFinished { task_id, outcome } => {
            json!({ "task_id": task_id, "outcome": outcome })
        }
        RunEventPayload::CleanupStarted => json!({}),
        RunEventPayload::CleanupFinished { complete } => json!({ "complete": complete }),
        RunEventPayload::RunFinished { outcome, exit_code } => {
            json!({ "outcome": outcome, "exit_code": exit_code })
        }
    }
}

impl EventSink for RunDirectory {
    fn append<'a>(&'a self, event: RunEvent) -> PortFuture<'a, ()> {
        let outcome = (|| {
            let mut writer = self
                .events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let path = self.root.join("events.jsonl");
            if writer.file.is_none() {
                let file = OpenOptions::new()
                    .append(true)
                    .create(true)
                    .mode(0o600)
                    .open(&path)
                    .map_err(|error| storage_error("open", &path, &error))?;
                writer.file = Some(file);
            }
            // The sequence number is assigned under the lock, so it is
            // strictly increasing in file order.
            writer.seq += 1;
            let line = json!({
                "schema_version": 1,
                "run_id": event.run_id,
                "seq": writer.seq,
                "timestamp": rfc3339_utc(SystemTime::now()),
                "type": event.payload.event_type(),
                "payload": event_payload(&event.payload),
            })
            .to_string();
            if let Some(file) = &mut writer.file {
                file.write_all(format!("{line}\n").as_bytes())
                    .and_then(|()| file.flush())
                    .map_err(|error| storage_error("write", &path, &error))?;
            }
            if self.echo_events {
                println!("{line}");
            }
            Ok(())
        })();
        Box::pin(async move { outcome })
    }
}

impl ArtifactStore for RunDirectory {
    fn task_artifacts<'a>(
        &'a self,
        task: &'a TaskId,
        report_file_name: &'a str,
    ) -> PortFuture<'a, TaskArtifacts> {
        let outcome = (|| {
            let relative = PathBuf::from("tasks").join(task.as_str());
            let directory = self.root.join(&relative);
            ensure_private_directory(&directory)?;
            let artifact = |name: &str| ArtifactRef {
                relative_path: relative.join(name),
            };
            Ok(TaskArtifacts {
                locations: ArtifactLocations {
                    report_path: directory.join(report_file_name),
                    task_directory: directory,
                },
                report: artifact(report_file_name),
                stdout: artifact("stdout.log"),
                stderr: artifact("stderr.log"),
                probe_stdout: artifact("version-stdout.log"),
                probe_stderr: artifact("version-stderr.log"),
            })
        })();
        Box::pin(async move { outcome })
    }

    fn read_report<'a>(
        &'a self,
        invocation: &'a PreparedInvocation,
    ) -> PortFuture<'a, ReportBundle> {
        let content = match &invocation.report_path {
            None => ReportContent::Missing,
            Some(path) if !path.starts_with(&self.root) => ReportContent::Unreadable {
                message: "the report path is outside the run directory".to_owned(),
            },
            Some(path) => read_report_file(path),
        };
        let bundle = ReportBundle {
            format: invocation.report_format,
            content,
        };
        Box::pin(async move { Ok(bundle) })
    }

    fn read_text<'a>(
        &'a self,
        artifact: &'a ArtifactRef,
        max_bytes: u64,
    ) -> PortFuture<'a, String> {
        let outcome = self.resolve(artifact).and_then(|path| {
            let bytes =
                std::fs::read(&path).map_err(|error| storage_error("read", &path, &error))?;
            let limit = usize::try_from(max_bytes)
                .unwrap_or(usize::MAX)
                .min(bytes.len());
            Ok(String::from_utf8_lossy(bytes.get(..limit).unwrap_or_default()).into_owned())
        });
        Box::pin(async move { outcome })
    }
}

/// Reads the report a runner wrote. Only a regular file of bounded size is
/// read; anything else is described, not guessed.
fn read_report_file(path: &Path) -> ReportContent {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ReportContent::Missing;
        }
        Err(error) => {
            return ReportContent::Unreadable {
                message: error.kind().to_string(),
            };
        }
    };
    if !metadata.is_file() {
        return ReportContent::Unreadable {
            message: "the report is not a regular file".to_owned(),
        };
    }
    if metadata.len() > MAX_REPORT_BYTES {
        return ReportContent::TooLarge {
            limit_bytes: MAX_REPORT_BYTES,
        };
    }
    // The runner created the file with its own mode; reports can hold secrets.
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    match std::fs::read(path) {
        Ok(bytes) => ReportContent::Bytes(bytes),
        Err(error) => ReportContent::Unreadable {
            message: error.kind().to_string(),
        },
    }
}

fn unix_seconds(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |elapsed| {
        i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
    })
}

/// The text form of a value that serializes as a JSON string.
fn text(value: &impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Runs blocking storage work off the async executor.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, PortError> + Send + 'static,
) -> Result<T, PortError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| PortError::Storage {
            message: "the run history could not be written: its task was stopped".to_owned(),
        })?
}

fn start_of_plan(
    run_id: &str,
    plan: &ExecutionPlan,
    lease_id: String,
    provider: String,
    started_at: i64,
) -> RunStart {
    let body = plan.body();
    RunStart {
        run_id: run_id.to_owned(),
        workspace_id: body.workspace_id.as_str().to_owned(),
        plan_id: text(plan.id()),
        mode: body.mode.as_str().to_owned(),
        snapshot_id: text(&body.snapshot_id),
        baseline_commit: body.baseline_commit.as_ref().map(text),
        config_digest: text(&body.config_digest),
        lease_id,
        provider,
        started_at,
    }
}

/// The terminal facts in a result document. `None` when the document is not
/// a usable terminal record; such a run is not finished.
fn finish_of_document(
    run_id: &str,
    document: &serde_json::Value,
    finished_at: i64,
    artifacts: Vec<ArtifactRow>,
) -> Option<RunFinish> {
    if document.get("run_id")?.as_str()? != run_id {
        return None;
    }
    let mut tasks = Vec::new();
    for suite in document.get("suites")?.as_array()? {
        let count = |name: &str| suite.get("counts")?.get(name)?.as_u64();
        let suite_id = suite.get("suite_id")?.as_str()?.to_owned();
        tasks.push(TaskRow {
            // One task per suite; the task carries the ID of its suite.
            task_id: suite_id.clone(),
            suite_id,
            runner: suite.get("runner")?.as_str()?.to_owned(),
            outcome: suite.get("outcome")?.as_str()?.to_owned(),
            selection: suite.get("selection")?.get("kind")?.as_str()?.to_owned(),
            counts: count("passed").and_then(|passed| {
                Some((passed, count("failed")?, count("skipped")?, count("flaky")?))
            }),
            duration_ms: suite.get("duration_ms")?.as_u64()?,
        });
    }
    let cleanup = document.get("cleanup")?;
    let lease_state = if document.get("environment")?.is_null() {
        "not_prepared"
    } else if cleanup.get("outcome")?.as_str()? == "succeeded" {
        "released"
    } else {
        "cleanup_incomplete"
    };
    Some(RunFinish {
        run_id: run_id.to_owned(),
        outcome: document.get("outcome")?.as_str()?.to_owned(),
        exit_code: u8::try_from(document.get("exit_code")?.as_u64()?).ok()?,
        assurance: document.get("assurance")?.as_str()?.to_owned(),
        input_validation: document.get("input_validation")?.as_str()?.to_owned(),
        finished_at,
        total_ms: document.get("metrics")?.get("total_ms")?.as_u64()?,
        tasks,
        lease_state,
        residual_resources: cleanup
            .get("residual_resources")?
            .as_array()?
            .iter()
            .filter_map(|resource| resource.as_str().map(str::to_owned))
            .collect(),
        artifacts,
    })
}

/// Every regular file below a run directory, with its size. Links are not
/// followed.
fn artifacts_of(root: &Path) -> Vec<ArtifactRow> {
    let mut artifacts = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.is_dir() {
                pending.push(path);
            } else if metadata.is_file()
                && let Ok(relative) = path.strip_prefix(root)
                && let Some(relative) = relative.to_str()
            {
                artifacts.push(ArtifactRow {
                    relative_path: relative.to_owned(),
                    bytes: metadata.len(),
                });
            }
        }
    }
    artifacts.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    artifacts
}

impl RunStore for RunDirectory {
    fn create<'a>(&'a self, record: &'a RunRecord) -> PortFuture<'a, ()> {
        let outcome: Result<RunStart, PortError> = (|| {
            let plan = record.plan.to_json().map_err(|error| PortError::Storage {
                message: format!("the plan could not be serialized: {error}"),
            })?;
            write_private_file(&self.root.join(PLAN_FILE), format!("{plan}\n").as_bytes())?;
            let started_at = unix_seconds(record.started_at);
            self.write_json(
                RUN_FILE,
                &json!({
                    "schema_version": 1,
                    "run_id": record.run_id,
                    "plan_id": record.plan_id,
                    "mode": record.mode,
                    "scope": record.scope,
                    "workspace_id": record.workspace_id,
                    "lease_id": record.lease_id,
                    "provider": record.provider.as_str(),
                    "started_at": rfc3339_utc(record.started_at),
                    "started_at_unix": started_at,
                }),
            )?;
            Ok(start_of_plan(
                record.run_id.as_str(),
                &record.plan,
                record.lease_id.as_str().to_owned(),
                record.provider.as_str().to_owned(),
                started_at,
            ))
        })();
        let history = self.history.clone();
        Box::pin(async move {
            let start = outcome?;
            // The files come first: a run that stops here is found again
            // from its directory.
            if let Some(history) = history {
                blocking(move || history.record_started(&start)).await?;
            }
            Ok(())
        })
    }

    fn finish<'a>(&'a self, result: &'a RunResult) -> PortFuture<'a, ()> {
        let outcome: Result<RunFinish, PortError> = (|| {
            let document = RunResultDocument::from(result);
            self.write_json("environment.json", &document.environment)?;
            write_private_file(
                &self.root.join("junit.xml"),
                junit::render(result).as_bytes(),
            )?;
            // The terminal record is written last: its presence means the
            // run and its other artifacts are complete.
            self.write_json(RESULT_FILE, &document)?;
            let value = serde_json::to_value(&document).map_err(|error| PortError::Storage {
                message: format!("the result could not be serialized: {error}"),
            })?;
            finish_of_document(
                result.run_id.as_str(),
                &value,
                unix_seconds(SystemTime::now()),
                artifacts_of(&self.root),
            )
            .ok_or_else(|| PortError::Storage {
                message: "the result document cannot be indexed".to_owned(),
            })
        })();
        let history = self.history.clone();
        Box::pin(async move {
            let finish = outcome?;
            if let Some(history) = history {
                blocking(move || history.record_finished(&finish)).await?;
            }
            Ok(())
        })
    }
}

fn runs_directory(workspace_root: &Path, storage_directory: &RepoPath) -> PathBuf {
    workspace_root
        .join(storage_directory.as_str())
        .join(RUNS_DIRECTORY)
}

/// A regular file of a run directory, read as text. `None` when it is
/// absent, not a regular file or not readable.
fn read_regular(path: &Path) -> Option<String> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// The IDs of the runs that have a directory. Only real directories with a
/// valid run ID as their name are runs; anything else in the same place is
/// not managed by testry and is ignored.
pub fn stored_run_ids(workspace_root: &Path, storage_directory: &RepoPath) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(runs_directory(workspace_root, storage_directory)) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = entries
        .flatten()
        .filter(|entry| {
            std::fs::symlink_metadata(entry.path()).is_ok_and(|metadata| metadata.is_dir())
        })
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .filter(|name| RunId::try_new(name.as_str()).is_ok())
        .collect();
    ids.sort();
    ids
}

/// What the directory of a run says about it.
pub fn stored_state(
    workspace_root: &Path,
    storage_directory: &RepoPath,
    run_id: &str,
) -> StoredState {
    let root = runs_directory(workspace_root, storage_directory).join(run_id);
    if !std::fs::symlink_metadata(&root).is_ok_and(|metadata| metadata.is_dir()) {
        return StoredState::Missing;
    }
    let result_path = root.join(RESULT_FILE);
    let finished = read_regular(&result_path)
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|document| {
            let written = std::fs::metadata(&result_path)
                .and_then(|metadata| metadata.modified())
                .map_or(0, unix_seconds);
            finish_of_document(run_id, &document, written, artifacts_of(&root))
        });
    match finished {
        Some(finish) => StoredState::Finished(Box::new(finish)),
        // No terminal record, or one that cannot be read: not finished.
        None => StoredState::Unfinished,
    }
}

/// The start record of a run, read from its directory. `None` when the
/// directory does not hold a valid start record and plan of that run.
pub fn stored_start(
    workspace_root: &Path,
    storage_directory: &RepoPath,
    run_id: &str,
) -> Option<RunStart> {
    let root = runs_directory(workspace_root, storage_directory).join(run_id);
    let record: serde_json::Value =
        serde_json::from_str(&read_regular(&root.join(RUN_FILE))?).ok()?;
    if record.get("run_id")?.as_str()? != run_id {
        return None;
    }
    let plan = ExecutionPlan::from_json(read_regular(&root.join(PLAN_FILE))?.as_bytes()).ok()?;
    if record.get("plan_id")?.as_str()? != text(plan.id()) {
        return None;
    }
    let field = |name: &str| record.get(name).and_then(serde_json::Value::as_str);
    let started_at = record
        .get("started_at_unix")
        .and_then(serde_json::Value::as_i64)
        .or_else(|| {
            std::fs::metadata(root.join(RUN_FILE))
                .and_then(|metadata| metadata.modified())
                .ok()
                .map(unix_seconds)
        })?;
    Some(start_of_plan(
        run_id,
        &plan,
        // Records written before leases were stored have none.
        field("lease_id").map_or_else(|| format!("unrecorded-{run_id}"), str::to_owned),
        field("provider").unwrap_or("host").to_owned(),
        started_at,
    ))
}

/// Brings the history in line with the run directories of a workspace. The
/// caller holds the workspace lock; `active_run` is its own run, if any.
pub fn reconcile_history(
    workspace_root: &Path,
    storage_directory: &RepoPath,
    history: &HistoryDatabase,
    active_run: Option<&RunId>,
) -> Result<Reconciliation, PortError> {
    history.reconcile(
        &stored_run_ids(workspace_root, storage_directory),
        active_run,
        &|run_id| stored_state(workspace_root, storage_directory, run_id),
        &|run_id| stored_start(workspace_root, storage_directory, run_id),
    )
}

/// What is stored about a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoredRun {
    /// The terminal result document.
    Finished(String),
    /// The run started but has no terminal record: it was interrupted. This
    /// is the start record.
    Interrupted(String),
    NotFound,
}

/// Reads a stored run without modifying anything.
pub fn read_run(
    workspace_root: &Path,
    storage_directory: &RepoPath,
    run_id: &RunId,
) -> Result<StoredRun, PortError> {
    let root = workspace_root
        .join(storage_directory.as_str())
        .join(RUNS_DIRECTORY)
        .join(run_id.as_str());
    let read = |name: &str| -> Result<Option<String>, PortError> {
        let path = root.join(name);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => std::fs::read_to_string(&path)
                .map(Some)
                .map_err(|error| storage_error("read", &path, &error)),
            Ok(_) => Err(PortError::Storage {
                message: format!("{} is not a regular file", path.display()),
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(storage_error("inspect", &path, &error)),
        }
    };
    if let Some(result) = read(RESULT_FILE)? {
        return Ok(StoredRun::Finished(result));
    }
    Ok(match read(RUN_FILE)? {
        Some(record) => StoredRun::Interrupted(record),
        None => StoredRun::NotFound,
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use testry_domain::results::RunOutcome;

    use super::*;

    const RUN: &str = "123e4567-e89b-42d3-a456-426614174000";

    fn storage() -> RepoPath {
        RepoPath::try_new(".testry").unwrap()
    }

    fn run_id() -> RunId {
        RunId::try_new(RUN).unwrap()
    }

    fn new_run(workspace: &Path) -> RunDirectory {
        RunDirectory::create(workspace, &storage(), &run_id(), false).unwrap()
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn event(payload: RunEventPayload) -> RunEvent {
        RunEvent {
            run_id: run_id(),
            payload,
        }
    }

    #[test]
    fn a_run_directory_is_private_ignored_by_git_and_never_reused() {
        let workspace = tempfile::tempdir().unwrap();
        let run = new_run(workspace.path());
        assert_eq!(
            run.root(),
            workspace.path().join(".testry/runs").join(RUN).as_path()
        );
        assert_eq!(mode_of(run.root()), 0o700);
        assert_eq!(mode_of(&run.root().join("tasks")), 0o700);
        assert_eq!(
            std::fs::read_to_string(workspace.path().join(".testry/.gitignore")).unwrap(),
            "*\n"
        );

        // P15: a second run with the same ID cannot pick up these artifacts.
        let again = RunDirectory::create(workspace.path(), &storage(), &run_id(), false);
        assert!(
            matches!(again, Err(PortError::Storage { .. })),
            "a run directory must never be reused"
        );
    }

    #[test]
    fn a_link_in_place_of_the_run_directory_is_refused() {
        let workspace = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(workspace.path().join(".testry/runs")).unwrap();
        std::os::unix::fs::symlink(
            elsewhere.path(),
            workspace.path().join(".testry/runs").join(RUN),
        )
        .unwrap();
        let run = RunDirectory::create(workspace.path(), &storage(), &run_id(), false);
        assert!(matches!(run, Err(PortError::Storage { .. })));
        assert_eq!(std::fs::read_dir(elsewhere.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn events_are_numbered_by_the_single_writer_in_file_order() {
        let workspace = tempfile::tempdir().unwrap();
        let run = new_run(workspace.path());
        run.append(event(RunEventPayload::CleanupStarted))
            .await
            .unwrap();
        run.append(event(RunEventPayload::CleanupFinished { complete: true }))
            .await
            .unwrap();
        run.append(event(RunEventPayload::RunFinished {
            outcome: RunOutcome::NoWork,
            exit_code: 4,
        }))
        .await
        .unwrap();

        let path = run.root().join("events.jsonl");
        assert_eq!(mode_of(&path), 0o600);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.ends_with('\n'));
        let events: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events.len(), 3);
        for (index, event) in events.iter().enumerate() {
            assert_eq!(event["schema_version"], 1);
            assert_eq!(event["run_id"], RUN);
            assert_eq!(event["seq"], index as u64 + 1);
            assert!(event["timestamp"].as_str().unwrap().ends_with('Z'));
        }
        assert_eq!(events[0]["type"], "cleanup_started");
        assert_eq!(events[0]["payload"], json!({}));
        assert_eq!(events[1]["payload"], json!({ "complete": true }));
        assert_eq!(
            events[2]["payload"],
            json!({ "outcome": "no_work", "exit_code": 4 })
        );
    }

    /// A crash while the terminal record was being written leaves a start
    /// record, events with a torn last line and a temporary file. That is an
    /// interrupted run; the partial document is never read as a result.
    #[test]
    fn a_run_that_stopped_while_writing_its_result_is_interrupted() {
        let workspace = tempfile::tempdir().unwrap();
        let run = new_run(workspace.path());
        std::fs::write(run.root().join(RUN_FILE), "{\"mode\":\"full\"}\n").unwrap();
        std::fs::write(
            run.root().join("events.jsonl"),
            "{\"seq\":1,\"type\":\"run_started\"}\n{\"seq\":2,\"type\":\"task_fin",
        )
        .unwrap();
        std::fs::write(
            run.root().join(".result.json.4242.tmp"),
            "{\"outcome\":\"passed\",\"exit_co",
        )
        .unwrap();

        let stored = read_run(workspace.path(), &storage(), &run_id()).unwrap();
        assert_eq!(
            stored,
            StoredRun::Interrupted("{\"mode\":\"full\"}\n".to_owned())
        );
    }

    #[test]
    fn stored_runs_are_read_by_their_terminal_record() {
        let workspace = tempfile::tempdir().unwrap();
        assert_eq!(
            read_run(workspace.path(), &storage(), &run_id()).unwrap(),
            StoredRun::NotFound
        );

        let run = new_run(workspace.path());
        // The directory alone is not a run: nothing was recorded as started.
        assert_eq!(
            read_run(workspace.path(), &storage(), &run_id()).unwrap(),
            StoredRun::NotFound
        );
        run.write_json(RUN_FILE, &json!({ "mode": "full" }))
            .unwrap();
        assert!(matches!(
            read_run(workspace.path(), &storage(), &run_id()).unwrap(),
            StoredRun::Interrupted(_)
        ));
        run.write_json(RESULT_FILE, &json!({ "outcome": "failed" }))
            .unwrap();
        let StoredRun::Finished(result) =
            read_run(workspace.path(), &storage(), &run_id()).unwrap()
        else {
            panic!("the terminal record must win");
        };
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&result).unwrap(),
            json!({ "outcome": "failed" })
        );

        // Documents are replaced atomically: no temporary file stays behind.
        let leftovers: Vec<String> = std::fs::read_dir(run.root())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        assert_eq!(mode_of(&run.root().join(RESULT_FILE)), 0o600);
    }

    #[test]
    fn a_terminal_record_that_is_a_link_is_not_followed() {
        let workspace = tempfile::tempdir().unwrap();
        let run = new_run(workspace.path());
        let outside = workspace.path().join("outside.json");
        std::fs::write(&outside, "{\"outcome\":\"passed\"}").unwrap();
        std::os::unix::fs::symlink(&outside, run.root().join(RESULT_FILE)).unwrap();
        assert!(matches!(
            read_run(workspace.path(), &storage(), &run_id()),
            Err(PortError::Storage { .. })
        ));
    }

    #[tokio::test]
    async fn artifact_references_cannot_leave_the_run_directory() {
        let workspace = tempfile::tempdir().unwrap();
        let run = new_run(workspace.path());
        std::fs::write(workspace.path().join("secret.txt"), "secret").unwrap();
        std::fs::write(run.root().join("tasks/out.log"), "0123456789").unwrap();

        for escaping in ["../../../secret.txt", "/etc/hostname", "tasks/../../x", ""] {
            let artifact = ArtifactRef {
                relative_path: PathBuf::from(escaping),
            };
            assert!(
                matches!(
                    run.read_text(&artifact, 1024).await,
                    Err(PortError::Storage { .. })
                ),
                "{escaping:?} must be refused"
            );
        }

        let inside = ArtifactRef {
            relative_path: PathBuf::from("tasks/out.log"),
        };
        assert_eq!(run.read_text(&inside, 1024).await.unwrap(), "0123456789");
        assert_eq!(
            run.read_text(&inside, 4).await.unwrap(),
            "0123",
            "reads are bounded"
        );
    }

    #[tokio::test]
    async fn every_task_gets_its_own_private_directory() {
        let workspace = tempfile::tempdir().unwrap();
        let run = new_run(workspace.path());
        let api = TaskId::try_new("api-unit").unwrap();
        let web = TaskId::try_new("web-unit").unwrap();
        let api = run.task_artifacts(&api, "report.xml").await.unwrap();
        let web = run.task_artifacts(&web, "report.json").await.unwrap();

        assert_eq!(mode_of(&api.locations.task_directory), 0o700);
        assert_ne!(api.locations.task_directory, web.locations.task_directory);
        assert!(api.locations.report_path.starts_with(run.root()));
        assert_eq!(
            api.report.relative_path,
            PathBuf::from("tasks/api-unit/report.xml")
        );
        assert_eq!(
            web.stdout.relative_path,
            PathBuf::from("tasks/web-unit/stdout.log")
        );
        assert!(
            !api.locations.report_path.exists(),
            "P15: a new run starts without any report"
        );
    }

    /// P08 / P14: what a runner left at the report path is described, never
    /// guessed, and nothing is read through a link.
    #[test]
    fn only_a_regular_report_file_is_read() {
        let directory = tempfile::tempdir().unwrap();
        let report = directory.path().join("report.xml");
        assert_eq!(read_report_file(&report), ReportContent::Missing);

        std::fs::write(&report, "<testsuites/>").unwrap();
        std::fs::set_permissions(&report, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            read_report_file(&report),
            ReportContent::Bytes(b"<testsuites/>".to_vec())
        );
        assert_eq!(mode_of(&report), 0o600, "a report can hold secrets");

        let empty = directory.path().join("empty.xml");
        std::fs::write(&empty, "").unwrap();
        assert_eq!(read_report_file(&empty), ReportContent::Bytes(Vec::new()));

        let link = directory.path().join("link.xml");
        std::os::unix::fs::symlink(&report, &link).unwrap();
        assert!(matches!(
            read_report_file(&link),
            ReportContent::Unreadable { .. }
        ));
        assert!(matches!(
            read_report_file(directory.path()),
            ReportContent::Unreadable { .. }
        ));
    }
}

/// The run directory together with the run history: what is written when,
/// and what the two say after a run stopped at each possible point.
#[cfg(test)]
mod history_tests {
    use testry_domain::results::RunOutcome;

    use super::*;
    use crate::storage::history::HistoryTable;
    use crate::storage::test_support::{finished_run, lease_of, result, started_run, storage};

    fn history(workspace: &Path) -> HistoryDatabase {
        HistoryDatabase::open(workspace, &storage()).unwrap()
    }

    fn row(history: &HistoryDatabase, run: &RunId) -> Option<crate::storage::history::RunRow> {
        history
            .list_runs(100)
            .unwrap()
            .into_iter()
            .find(|row| row.run_id == run.as_str())
    }

    /// The same run directory without the history: what a process that
    /// stopped before it could update the index has written.
    fn files_only(run: &RunDirectory) -> RunDirectory {
        RunDirectory {
            root: run.root.clone(),
            events: Mutex::new(EventWriter { file: None, seq: 0 }),
            echo_events: false,
            history: None,
        }
    }

    #[tokio::test]
    async fn a_run_is_indexed_when_it_starts_and_when_it_finishes() {
        let workspace = tempfile::tempdir().unwrap();
        let history = history(workspace.path());
        let (directory, run, plan) = started_run(workspace.path(), 1, Some(&history)).await;

        let running = row(&history, &run).unwrap();
        assert_eq!(running.state, "running");
        assert_eq!(running.outcome, None);
        assert_eq!(running.mode, "full");
        assert_eq!(running.plan_id, plan.id().to_string());
        // The start record names the lease, so it can be found again later.
        let record: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(directory.root().join(RUN_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(record["lease_id"], lease_of(&run).as_str());
        assert_eq!(record["provider"], "host");
        assert!(record["started_at_unix"].as_i64().unwrap() > 1_700_000_000);

        directory
            .finish(&result(&run, &plan, RunOutcome::Failed))
            .await
            .unwrap();
        let finished = row(&history, &run).unwrap();
        assert_eq!(finished.state, "finished");
        assert_eq!(finished.outcome.as_deref(), Some("failed"));
        assert_eq!(finished.exit_code, Some(1));
        assert_eq!(finished.assurance.as_deref(), Some("full_suite"));
        assert_eq!(finished.total_ms, Some(1500));
        assert_eq!(
            history
                .count_rows(HistoryTable::Tasks, run.as_str())
                .unwrap(),
            2
        );
        assert_eq!(history.duration_samples("unit", 5).unwrap(), [120]);
        // No environment was prepared for this result.
        assert_eq!(
            history.lease_state(run.as_str()).unwrap().as_deref(),
            Some("not_prepared")
        );

        // Every file of the run is indexed, the terminal record included.
        let indexed = history
            .count_rows(HistoryTable::Artifacts, run.as_str())
            .unwrap();
        let on_disk = artifacts_of(directory.root());
        assert_eq!(indexed, on_disk.len() as u64);
        let names: Vec<&str> = on_disk
            .iter()
            .map(|artifact| artifact.relative_path.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "environment.json",
                "junit.xml",
                "plan.json",
                "result.json",
                "run.json"
            ]
        );
    }

    /// R15: a process can stop between any two writes. Afterwards the index
    /// is repaired from the files, and no run is finished without its
    /// terminal record.
    #[tokio::test]
    async fn the_history_is_repaired_after_a_stop_at_every_point() {
        let workspace = tempfile::tempdir().unwrap();
        let history = history(workspace.path());

        // 1: stopped before the start was indexed (files only, unfinished).
        let (_, before_index, _) = started_run(workspace.path(), 1, None).await;
        // 2: stopped while running (indexed, no terminal record).
        let (_, while_running, _) = started_run(workspace.path(), 2, Some(&history)).await;
        // 3: stopped between the terminal record and its index update.
        let (directory, after_result, plan) =
            started_run(workspace.path(), 3, Some(&history)).await;
        files_only(&directory)
            .finish(&result(&after_result, &plan, RunOutcome::Passed))
            .await
            .unwrap();
        // 4: never indexed at all, but complete on disk.
        let unindexed = finished_run(workspace.path(), 4, None).await;
        // 5: finished normally.
        let normal = finished_run(workspace.path(), 5, Some(&history)).await;

        assert_eq!(row(&history, &after_result).unwrap().state, "running");
        assert!(row(&history, &before_index).is_none());

        let report = reconcile_history(workspace.path(), &storage(), &history, None).unwrap();
        assert_eq!(
            report.completed,
            [after_result.as_str(), unindexed.as_str()]
        );
        assert_eq!(
            report.interrupted,
            [while_running.as_str(), before_index.as_str()]
        );
        assert!(report.removed.is_empty());

        for (run, state, outcome) in [
            (&before_index, "interrupted", None),
            (&while_running, "interrupted", None),
            (&after_result, "finished", Some("passed")),
            (&unindexed, "finished", Some("passed")),
            (&normal, "finished", Some("passed")),
        ] {
            let row = row(&history, run).unwrap();
            assert_eq!(row.state, state, "{run}");
            assert_eq!(row.outcome.as_deref(), outcome, "{run}");
        }
        assert_eq!(
            history
                .count_rows(HistoryTable::Tasks, after_result.as_str())
                .unwrap(),
            2,
            "the suites of the repaired run come from its terminal record"
        );
        assert_eq!(
            history
                .lease_state(while_running.as_str())
                .unwrap()
                .as_deref(),
            Some("abandoned")
        );

        // Nothing is left to repair.
        assert!(
            reconcile_history(workspace.path(), &storage(), &history, None)
                .unwrap()
                .is_empty()
        );
    }

    /// A terminal record that was cut off, or that belongs to another run,
    /// is not a terminal record.
    #[tokio::test]
    async fn a_damaged_or_foreign_terminal_record_is_not_a_result() {
        let workspace = tempfile::tempdir().unwrap();
        let history = history(workspace.path());
        let (cut, cut_run, _) = started_run(workspace.path(), 1, Some(&history)).await;
        std::fs::write(
            cut.root().join(RESULT_FILE),
            "{\"outcome\":\"passed\",\"exit_co",
        )
        .unwrap();

        let foreign = finished_run(workspace.path(), 2, None).await;
        let (target, target_run, _) = started_run(workspace.path(), 3, Some(&history)).await;
        std::fs::copy(
            workspace
                .path()
                .join(".testry/runs")
                .join(foreign.as_str())
                .join(RESULT_FILE),
            target.root().join(RESULT_FILE),
        )
        .unwrap();

        assert_eq!(
            stored_state(workspace.path(), &storage(), cut_run.as_str()),
            StoredState::Unfinished
        );
        assert_eq!(
            stored_state(workspace.path(), &storage(), target_run.as_str()),
            StoredState::Unfinished,
            "the result of another run is not the result of this one"
        );
        reconcile_history(workspace.path(), &storage(), &history, None).unwrap();
        assert_eq!(row(&history, &cut_run).unwrap().state, "interrupted");
        assert_eq!(row(&history, &target_run).unwrap().state, "interrupted");
        assert_eq!(row(&history, &target_run).unwrap().outcome, None);
    }

    #[tokio::test]
    async fn only_real_run_directories_are_runs() {
        let workspace = tempfile::tempdir().unwrap();
        let run = finished_run(workspace.path(), 1, None).await;
        let runs = workspace.path().join(".testry/runs");
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(runs.join("notes.txt"), "mine").unwrap();
        std::fs::create_dir(runs.join("backup")).unwrap();
        std::os::unix::fs::symlink(
            outside.path(),
            runs.join("00000000-0000-4000-8000-0000000000aa"),
        )
        .unwrap();
        std::fs::create_dir(runs.join("00000000-0000-4000-8000-0000000000bb")).unwrap();

        assert_eq!(
            stored_run_ids(workspace.path(), &storage()),
            [run.as_str(), "00000000-0000-4000-8000-0000000000bb"]
        );
        // A directory with the name of a run but without its start record.
        assert!(
            stored_start(
                workspace.path(),
                &storage(),
                "00000000-0000-4000-8000-0000000000bb"
            )
            .is_none()
        );
        // A link is never a run, whatever it points to.
        assert_eq!(
            stored_state(
                workspace.path(),
                &storage(),
                "00000000-0000-4000-8000-0000000000aa"
            ),
            StoredState::Missing
        );
        let start = stored_start(workspace.path(), &storage(), run.as_str()).unwrap();
        assert_eq!(start.run_id, run.as_str());
        assert_eq!(start.provider, "host");
        assert_eq!(start.mode, "full");
    }

    /// The start record must be the one of the run its directory is named
    /// after, with the plan it names.
    #[tokio::test]
    async fn a_start_record_of_another_run_or_plan_proves_nothing() {
        let workspace = tempfile::tempdir().unwrap();
        let first = finished_run(workspace.path(), 1, None).await;
        let second = finished_run(workspace.path(), 2, None).await;
        let runs = workspace.path().join(".testry/runs");

        // The plan of another run in place of its own.
        std::fs::copy(
            runs.join(second.as_str()).join(PLAN_FILE),
            runs.join(first.as_str()).join(PLAN_FILE),
        )
        .unwrap();
        assert!(stored_start(workspace.path(), &storage(), first.as_str()).is_none());

        // The start record of another run.
        let copy = runs.join("00000000-0000-4000-8000-0000000000cc");
        std::fs::create_dir(&copy).unwrap();
        for file in [RUN_FILE, PLAN_FILE] {
            std::fs::copy(runs.join(second.as_str()).join(file), copy.join(file)).unwrap();
        }
        assert!(
            stored_start(
                workspace.path(),
                &storage(),
                "00000000-0000-4000-8000-0000000000cc"
            )
            .is_none()
        );
        assert!(stored_start(workspace.path(), &storage(), second.as_str()).is_some());
    }
}
