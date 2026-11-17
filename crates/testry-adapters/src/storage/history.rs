//! The run history of a workspace (`<storage>/state.sqlite3`, spec section 13).
//!
//! The history is an index. What a run did is in its artifact directory, and
//! `result.json` there is the terminal record of the run. The database makes
//! runs, their suites, their artifacts and their durations queryable without
//! reading every directory, and it is what `gc` works from.
//!
//! The two are written one after the other, files first, so a process can
//! stop between them. [`HistoryDatabase::reconcile`] repairs the index from
//! the files:
//!
//! | database                | run directory            | meaning                                   |
//! |-------------------------|--------------------------|-------------------------------------------|
//! | `running`               | has `result.json`        | finished; the index was not updated       |
//! | `running`               | no `result.json`         | interrupted, unless it is the active run  |
//! | no row                  | has `run.json`           | indexed from the files                    |
//! | any row                 | directory is gone        | the row is removed and reported           |
//!
//! A run is never read as finished from the database alone, and nothing in
//! the database can turn an interrupted run into a successful one.
//!
//! All calls block on file IO. Callers on an async executor run them on a
//! blocking thread.

use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use testry_application::ports::PortError;
use testry_domain::ids::RunId;
use testry_domain::paths::RepoPath;

use super::private::ensure_storage_directory;

const DATABASE_FILE: &str = "state.sqlite3";

/// How long a writer waits for another process that holds the database.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The schema version this build reads and writes.
pub const SCHEMA_VERSION: i64 = 1;

const MIGRATION_1: &str = "
CREATE TABLE workspaces (
    workspace_id   TEXT PRIMARY KEY,
    first_seen_at  INTEGER NOT NULL
);
CREATE TABLE plans (
    plan_id          TEXT PRIMARY KEY,
    workspace_id     TEXT NOT NULL REFERENCES workspaces(workspace_id),
    mode             TEXT NOT NULL,
    snapshot_id      TEXT NOT NULL,
    baseline_commit  TEXT,
    config_digest    TEXT NOT NULL,
    first_used_at    INTEGER NOT NULL
);
CREATE TABLE runs (
    run_id            TEXT PRIMARY KEY,
    workspace_id      TEXT NOT NULL REFERENCES workspaces(workspace_id),
    plan_id           TEXT NOT NULL REFERENCES plans(plan_id),
    mode              TEXT NOT NULL,
    state             TEXT NOT NULL CHECK (state IN ('running', 'finished', 'interrupted')),
    outcome           TEXT,
    exit_code         INTEGER,
    assurance         TEXT,
    input_validation  TEXT,
    started_at        INTEGER NOT NULL,
    finished_at       INTEGER,
    total_ms          INTEGER,
    -- A finished run has an outcome, a running or interrupted run has none.
    CHECK ((state = 'finished') = (outcome IS NOT NULL))
);
CREATE INDEX runs_by_start ON runs(started_at);
CREATE TABLE tasks (
    run_id       TEXT NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    task_id      TEXT NOT NULL,
    suite_id     TEXT NOT NULL,
    runner       TEXT NOT NULL,
    outcome      TEXT NOT NULL,
    selection    TEXT NOT NULL,
    -- NULL when the runner cannot count tests. Never 0 for unknown.
    passed       INTEGER,
    failed       INTEGER,
    skipped      INTEGER,
    flaky        INTEGER,
    duration_ms  INTEGER NOT NULL,
    PRIMARY KEY (run_id, task_id)
);
CREATE TABLE environment_leases (
    lease_id            TEXT PRIMARY KEY,
    run_id              TEXT NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    provider            TEXT NOT NULL,
    state               TEXT NOT NULL
        CHECK (state IN ('planned', 'not_prepared', 'released', 'cleanup_incomplete', 'abandoned')),
    residual_resources  TEXT
);
CREATE TABLE artifacts (
    run_id         TEXT NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    relative_path  TEXT NOT NULL,
    bytes          INTEGER NOT NULL,
    PRIMARY KEY (run_id, relative_path)
);
CREATE TABLE duration_samples (
    run_id       TEXT NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    suite_id     TEXT NOT NULL,
    runner       TEXT NOT NULL,
    selection    TEXT NOT NULL,
    outcome      TEXT NOT NULL,
    duration_ms  INTEGER NOT NULL,
    recorded_at  INTEGER NOT NULL,
    PRIMARY KEY (run_id, suite_id)
);
CREATE INDEX duration_samples_by_suite ON duration_samples(suite_id, recorded_at);
";

/// What is known when a run starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunStart {
    pub run_id: String,
    pub workspace_id: String,
    pub plan_id: String,
    pub mode: String,
    pub snapshot_id: String,
    pub baseline_commit: Option<String>,
    pub config_digest: String,
    pub lease_id: String,
    pub provider: String,
    /// Seconds since the Unix epoch.
    pub started_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRow {
    pub task_id: String,
    pub suite_id: String,
    pub runner: String,
    pub outcome: String,
    /// `all` or `files`.
    pub selection: String,
    /// `None` when the runner cannot count tests.
    pub counts: Option<(u64, u64, u64, u64)>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactRow {
    pub relative_path: String,
    pub bytes: u64,
}

/// The terminal facts of a run, as they are in its `result.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunFinish {
    pub run_id: String,
    pub outcome: String,
    pub exit_code: u8,
    pub assurance: String,
    pub input_validation: String,
    pub finished_at: i64,
    pub total_ms: u64,
    pub tasks: Vec<TaskRow>,
    /// `not_prepared`, `released` or `cleanup_incomplete`.
    pub lease_state: &'static str,
    pub residual_resources: Vec<String>,
    pub artifacts: Vec<ArtifactRow>,
}

/// One run in the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRow {
    pub run_id: String,
    pub plan_id: String,
    pub mode: String,
    /// `running`, `finished` or `interrupted`.
    pub state: String,
    pub outcome: Option<String>,
    pub exit_code: Option<u8>,
    pub assurance: Option<String>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub total_ms: Option<u64>,
}

/// What a run directory says about its run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoredState {
    /// The terminal record exists. These are its facts.
    Finished(Box<RunFinish>),
    /// The run started and has no terminal record.
    Unfinished,
    /// There is no directory for the run any more.
    Missing,
}

/// What reconciling the index with the run directories changed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Reconciliation {
    /// Runs that had finished but were still indexed as running.
    pub completed: Vec<String>,
    /// Runs that stopped without a terminal record.
    pub interrupted: Vec<String>,
    /// Rows whose run directory no longer exists.
    pub removed: Vec<String>,
}

impl Reconciliation {
    pub fn is_empty(&self) -> bool {
        self.completed.is_empty() && self.interrupted.is_empty() && self.removed.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct HistoryDatabase {
    path: PathBuf,
}

fn storage(action: &str, path: &Path, error: &rusqlite::Error) -> PortError {
    // The message of SQLite may quote values; only its kind is reported.
    let kind = match error {
        rusqlite::Error::SqliteFailure(failure, _) => format!("{:?}", failure.code),
        rusqlite::Error::QueryReturnedNoRows => "no rows".to_owned(),
        _ => "database error".to_owned(),
    };
    PortError::Storage {
        message: format!("cannot {action} the run history {}: {kind}", path.display()),
    }
}

fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

impl HistoryDatabase {
    /// Opens the history of a workspace, creating and migrating it if needed.
    pub fn open(workspace_root: &Path, storage_directory: &RepoPath) -> Result<Self, PortError> {
        let directory = ensure_storage_directory(workspace_root, storage_directory)?;
        Self::open_at(directory.join(DATABASE_FILE))
    }

    /// The history file of a workspace if it exists. Nothing is created;
    /// read-only commands use this.
    pub fn existing(workspace_root: &Path, storage_directory: &RepoPath) -> Option<Self> {
        let path = workspace_root
            .join(storage_directory.as_str())
            .join(DATABASE_FILE);
        std::fs::symlink_metadata(&path)
            .is_ok_and(|metadata| metadata.is_file())
            .then_some(Self { path })
    }

    fn open_at(path: PathBuf) -> Result<Self, PortError> {
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => {
                return Err(PortError::Storage {
                    message: format!(
                        "{} is not a regular file; refusing to use it as the run history",
                        path.display()
                    ),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // Created here with a private mode; SQLite gives its journal
                // the mode of the database file.
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&path)
                    .map_err(|error| PortError::Storage {
                        message: format!("cannot create {}: {}", path.display(), error.kind()),
                    })?;
            }
            Err(error) => {
                return Err(PortError::Storage {
                    message: format!("cannot inspect {}: {}", path.display(), error.kind()),
                });
            }
        }
        let database = Self { path };
        database.migrate()?;
        Ok(database)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn connect(&self) -> Result<Connection, PortError> {
        let connection =
            Connection::open(&self.path).map_err(|error| storage("open", &self.path, &error))?;
        connection
            .busy_timeout(BUSY_TIMEOUT)
            .and_then(|()| connection.pragma_update(None, "foreign_keys", true))
            .map_err(|error| storage("configure", &self.path, &error))?;
        Ok(connection)
    }

    fn migrate(&self) -> Result<(), PortError> {
        let mut connection = self.connect()?;
        let fail = |error: rusqlite::Error| storage("migrate", &self.path, &error);
        let transaction = connection.transaction().map_err(fail)?;
        transaction
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS schema_migrations (
                     version     INTEGER PRIMARY KEY,
                     applied_at  INTEGER NOT NULL
                 );",
            )
            .map_err(fail)?;
        let current: i64 = transaction
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
                [],
                |row| row.get(0),
            )
            .map_err(fail)?;
        if current > SCHEMA_VERSION {
            return Err(PortError::Storage {
                message: format!(
                    "the run history {} has schema version {current}, but this testry reads \
                     version {SCHEMA_VERSION}; it was written by a newer testry and is left untouched",
                    self.path.display()
                ),
            });
        }
        if current < 1 {
            transaction.execute_batch(MIGRATION_1).map_err(fail)?;
            transaction
                .execute(
                    "INSERT INTO schema_migrations (version, applied_at) VALUES (1, unixepoch())",
                    [],
                )
                .map_err(fail)?;
        }
        transaction.commit().map_err(fail)
    }

    /// Indexes a run that is about to start.
    pub fn record_started(&self, start: &RunStart) -> Result<(), PortError> {
        let mut connection = self.connect()?;
        let fail = |error: rusqlite::Error| storage("write", &self.path, &error);
        let transaction = connection.transaction().map_err(fail)?;
        insert_start(&transaction, start, "running").map_err(fail)?;
        transaction.commit().map_err(fail)
    }

    /// Stores the terminal facts of a run. The run must be indexed; a run
    /// that is already finished is left as it is.
    pub fn record_finished(&self, finish: &RunFinish) -> Result<(), PortError> {
        let mut connection = self.connect()?;
        let fail = |error: rusqlite::Error| storage("write", &self.path, &error);
        let transaction = connection.transaction().map_err(fail)?;
        let known = apply_finish(&transaction, finish).map_err(fail)?;
        transaction.commit().map_err(fail)?;
        if known {
            Ok(())
        } else {
            Err(PortError::Storage {
                message: format!(
                    "run {} is not in the run history, so its result cannot be indexed",
                    finish.run_id
                ),
            })
        }
    }

    /// Runs, newest first. Start times have a resolution of one second;
    /// runs of the same second are ordered by when they were recorded, which
    /// is the order they ran in (one run at a time holds the workspace).
    pub fn list_runs(&self, limit: u32) -> Result<Vec<RunRow>, PortError> {
        let connection = self.connect()?;
        let fail = |error: rusqlite::Error| storage("read", &self.path, &error);
        let mut statement = connection
            .prepare(
                "SELECT run_id, plan_id, mode, state, outcome, exit_code, assurance,
                        started_at, finished_at, total_ms
                 FROM runs ORDER BY started_at DESC, rowid DESC LIMIT ?1",
            )
            .map_err(fail)?;
        let rows = statement
            .query_map(params![limit], |row| {
                Ok(RunRow {
                    run_id: row.get(0)?,
                    plan_id: row.get(1)?,
                    mode: row.get(2)?,
                    state: row.get(3)?,
                    outcome: row.get(4)?,
                    exit_code: row
                        .get::<_, Option<i64>>(5)?
                        .map(|code| u8::try_from(code).unwrap_or(u8::MAX)),
                    assurance: row.get(6)?,
                    started_at: row.get(7)?,
                    finished_at: row.get(8)?,
                    total_ms: row.get::<_, Option<i64>>(9)?.map(to_u64),
                })
            })
            .map_err(fail)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(fail)
    }

    /// Durations of a suite in finished runs, newest first, in milliseconds.
    pub fn duration_samples(&self, suite_id: &str, limit: u32) -> Result<Vec<u64>, PortError> {
        let connection = self.connect()?;
        let fail = |error: rusqlite::Error| storage("read", &self.path, &error);
        let mut statement = connection
            .prepare(
                "SELECT duration_ms FROM duration_samples
                 WHERE suite_id = ?1 ORDER BY recorded_at DESC, rowid DESC LIMIT ?2",
            )
            .map_err(fail)?;
        let rows = statement
            .query_map(params![suite_id, limit], |row| row.get::<_, i64>(0))
            .map_err(fail)?;
        rows.map(|row| row.map(to_u64))
            .collect::<Result<Vec<_>, _>>()
            .map_err(fail)
    }

    /// The number of rows of a table that belong to a run. For tests and
    /// consistency checks.
    pub fn count_rows(&self, table: HistoryTable, run_id: &str) -> Result<u64, PortError> {
        let connection = self.connect()?;
        let sql = format!("SELECT COUNT(*) FROM {} WHERE run_id = ?1", table.name());
        connection
            .query_row(&sql, params![run_id], |row| row.get::<_, i64>(0))
            .map(to_u64)
            .map_err(|error| storage("read", &self.path, &error))
    }

    /// The state of the lease of a run.
    pub fn lease_state(&self, run_id: &str) -> Result<Option<String>, PortError> {
        let connection = self.connect()?;
        connection
            .query_row(
                "SELECT state FROM environment_leases WHERE run_id = ?1",
                params![run_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| storage("read", &self.path, &error))
    }

    /// Removes runs from the index, with everything that belongs to them,
    /// and plans no run refers to any more.
    pub fn delete_runs(&self, run_ids: &[String]) -> Result<(), PortError> {
        let mut connection = self.connect()?;
        let fail = |error: rusqlite::Error| storage("write", &self.path, &error);
        let transaction = connection.transaction().map_err(fail)?;
        for run_id in run_ids {
            transaction
                .execute("DELETE FROM runs WHERE run_id = ?1", params![run_id])
                .map_err(fail)?;
        }
        transaction
            .execute(
                "DELETE FROM plans WHERE plan_id NOT IN (SELECT plan_id FROM runs)",
                [],
            )
            .map_err(fail)?;
        transaction.commit().map_err(fail)
    }

    /// Brings the index in line with the run directories. `stored` reads
    /// what the directory of a run says; `start_of` reads the start record
    /// of a run that is in a directory but not in the index.
    ///
    /// `active_run` is the run of the calling process, which is running and
    /// must not be marked as interrupted. The caller holds the workspace
    /// lock, so no other run is active.
    pub fn reconcile(
        &self,
        directories: &[String],
        active_run: Option<&RunId>,
        stored: &dyn Fn(&str) -> StoredState,
        start_of: &dyn Fn(&str) -> Option<RunStart>,
    ) -> Result<Reconciliation, PortError> {
        let mut connection = self.connect()?;
        let fail = |error: rusqlite::Error| storage("reconcile", &self.path, &error);
        let transaction = connection.transaction().map_err(fail)?;
        let mut report = Reconciliation::default();
        let active = active_run.map(RunId::as_str);

        let indexed: Vec<(String, String)> = {
            let mut statement = transaction
                .prepare("SELECT run_id, state FROM runs ORDER BY run_id")
                .map_err(fail)?;
            let rows = statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .map_err(fail)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(fail)?
        };

        for (run_id, state) in &indexed {
            if Some(run_id.as_str()) == active {
                continue;
            }
            match stored(run_id) {
                StoredState::Missing => {
                    transaction
                        .execute("DELETE FROM runs WHERE run_id = ?1", params![run_id])
                        .map_err(fail)?;
                    report.removed.push(run_id.clone());
                }
                StoredState::Finished(finish) if state != "finished" => {
                    apply_finish(&transaction, &finish).map_err(fail)?;
                    report.completed.push(run_id.clone());
                }
                StoredState::Unfinished if state == "running" => {
                    mark_interrupted(&transaction, run_id).map_err(fail)?;
                    report.interrupted.push(run_id.clone());
                }
                // A finished row stays finished: the terminal record of a
                // run is never taken back.
                StoredState::Finished(_) | StoredState::Unfinished => {}
            }
        }

        // Runs that are in a directory but were never indexed.
        for run_id in directories {
            if Some(run_id.as_str()) == active
                || indexed.iter().any(|(indexed_id, _)| indexed_id == run_id)
            {
                continue;
            }
            let Some(start) = start_of(run_id) else {
                // Without a start record the directory is not a run.
                continue;
            };
            match stored(run_id) {
                StoredState::Finished(finish) => {
                    insert_start(&transaction, &start, "running").map_err(fail)?;
                    apply_finish(&transaction, &finish).map_err(fail)?;
                    report.completed.push(run_id.clone());
                }
                StoredState::Unfinished => {
                    insert_start(&transaction, &start, "running").map_err(fail)?;
                    mark_interrupted(&transaction, run_id).map_err(fail)?;
                    report.interrupted.push(run_id.clone());
                }
                StoredState::Missing => {}
            }
        }
        transaction
            .execute(
                "DELETE FROM plans WHERE plan_id NOT IN (SELECT plan_id FROM runs)",
                [],
            )
            .map_err(fail)?;
        transaction.commit().map_err(fail)?;
        Ok(report)
    }
}

/// The tables that hold rows per run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryTable {
    Runs,
    Tasks,
    EnvironmentLeases,
    Artifacts,
    DurationSamples,
}

impl HistoryTable {
    fn name(self) -> &'static str {
        match self {
            Self::Runs => "runs",
            Self::Tasks => "tasks",
            Self::EnvironmentLeases => "environment_leases",
            Self::Artifacts => "artifacts",
            Self::DurationSamples => "duration_samples",
        }
    }
}

fn insert_start(
    transaction: &Transaction<'_>,
    start: &RunStart,
    state: &str,
) -> rusqlite::Result<()> {
    transaction.execute(
        "INSERT INTO workspaces (workspace_id, first_seen_at) VALUES (?1, ?2)
         ON CONFLICT (workspace_id) DO NOTHING",
        params![start.workspace_id, start.started_at],
    )?;
    transaction.execute(
        "INSERT INTO plans (plan_id, workspace_id, mode, snapshot_id, baseline_commit,
                            config_digest, first_used_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (plan_id) DO NOTHING",
        params![
            start.plan_id,
            start.workspace_id,
            start.mode,
            start.snapshot_id,
            start.baseline_commit,
            start.config_digest,
            start.started_at
        ],
    )?;
    transaction.execute(
        "INSERT INTO runs (run_id, workspace_id, plan_id, mode, state, started_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            start.run_id,
            start.workspace_id,
            start.plan_id,
            start.mode,
            state,
            start.started_at
        ],
    )?;
    transaction.execute(
        "INSERT INTO environment_leases (lease_id, run_id, provider, state)
         VALUES (?1, ?2, ?3, 'planned')",
        params![start.lease_id, start.run_id, start.provider],
    )?;
    Ok(())
}

/// Writes the terminal facts of a run. Returns whether the run is indexed.
/// A run that is already finished keeps its record.
fn apply_finish(transaction: &Transaction<'_>, finish: &RunFinish) -> rusqlite::Result<bool> {
    let state: Option<String> = transaction
        .query_row(
            "SELECT state FROM runs WHERE run_id = ?1",
            params![finish.run_id],
            |row| row.get(0),
        )
        .optional()?;
    match state.as_deref() {
        None => return Ok(false),
        Some("finished") => return Ok(true),
        Some(_) => {}
    }
    transaction.execute(
        "UPDATE runs SET state = 'finished', outcome = ?2, exit_code = ?3, assurance = ?4,
                         input_validation = ?5, finished_at = ?6, total_ms = ?7
         WHERE run_id = ?1",
        params![
            finish.run_id,
            finish.outcome,
            i64::from(finish.exit_code),
            finish.assurance,
            finish.input_validation,
            finish.finished_at,
            to_i64(finish.total_ms)
        ],
    )?;
    for task in &finish.tasks {
        let (passed, failed, skipped, flaky) = match task.counts {
            Some((passed, failed, skipped, flaky)) => (
                Some(to_i64(passed)),
                Some(to_i64(failed)),
                Some(to_i64(skipped)),
                Some(to_i64(flaky)),
            ),
            None => (None, None, None, None),
        };
        transaction.execute(
            "INSERT OR REPLACE INTO tasks (run_id, task_id, suite_id, runner, outcome, selection,
                                           passed, failed, skipped, flaky, duration_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                finish.run_id,
                task.task_id,
                task.suite_id,
                task.runner,
                task.outcome,
                task.selection,
                passed,
                failed,
                skipped,
                flaky,
                to_i64(task.duration_ms)
            ],
        )?;
        transaction.execute(
            "INSERT OR REPLACE INTO duration_samples (run_id, suite_id, runner, selection, outcome,
                                                      duration_ms, recorded_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                finish.run_id,
                task.suite_id,
                task.runner,
                task.selection,
                task.outcome,
                to_i64(task.duration_ms),
                finish.finished_at
            ],
        )?;
    }
    let residual =
        (!finish.residual_resources.is_empty()).then(|| finish.residual_resources.join("\n"));
    transaction.execute(
        "UPDATE environment_leases SET state = ?2, residual_resources = ?3 WHERE run_id = ?1",
        params![finish.run_id, finish.lease_state, residual],
    )?;
    for artifact in &finish.artifacts {
        transaction.execute(
            "INSERT OR REPLACE INTO artifacts (run_id, relative_path, bytes) VALUES (?1, ?2, ?3)",
            params![
                finish.run_id,
                artifact.relative_path,
                to_i64(artifact.bytes)
            ],
        )?;
    }
    Ok(true)
}

fn mark_interrupted(transaction: &Transaction<'_>, run_id: &str) -> rusqlite::Result<()> {
    transaction.execute(
        "UPDATE runs SET state = 'interrupted' WHERE run_id = ?1 AND state = 'running'",
        params![run_id],
    )?;
    // Whatever the run prepared was not released by it.
    transaction.execute(
        "UPDATE environment_leases SET state = 'abandoned'
         WHERE run_id = ?1 AND state = 'planned'",
        params![run_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    const RUN: &str = "00000000-0000-4000-8000-000000000001";
    const OTHER: &str = "00000000-0000-4000-8000-000000000002";

    fn database(directory: &Path) -> HistoryDatabase {
        HistoryDatabase::open(directory, &RepoPath::try_new(".testry").unwrap()).unwrap()
    }

    fn start(run_id: &str, plan_id: &str, started_at: i64) -> RunStart {
        RunStart {
            run_id: run_id.to_owned(),
            workspace_id: "ws-test".to_owned(),
            plan_id: plan_id.to_owned(),
            mode: "full".to_owned(),
            snapshot_id: "blake3:snapshot".to_owned(),
            baseline_commit: None,
            config_digest: "blake3:config".to_owned(),
            lease_id: format!("lease-{run_id}"),
            provider: "host".to_owned(),
            started_at,
        }
    }

    fn finish(run_id: &str, outcome: &str) -> RunFinish {
        RunFinish {
            run_id: run_id.to_owned(),
            outcome: outcome.to_owned(),
            exit_code: u8::from(outcome != "passed"),
            assurance: "full_suite".to_owned(),
            input_validation: "unchanged".to_owned(),
            finished_at: 2_000,
            total_ms: 1500,
            tasks: vec![
                TaskRow {
                    task_id: "unit".to_owned(),
                    suite_id: "unit".to_owned(),
                    runner: "bun".to_owned(),
                    outcome: "passed".to_owned(),
                    selection: "all".to_owned(),
                    counts: Some((3, 0, 1, 0)),
                    duration_ms: 120,
                },
                TaskRow {
                    task_id: "typecheck".to_owned(),
                    suite_id: "typecheck".to_owned(),
                    runner: "command".to_owned(),
                    outcome: "passed".to_owned(),
                    selection: "all".to_owned(),
                    counts: None,
                    duration_ms: 900,
                },
            ],
            lease_state: "released",
            residual_resources: Vec::new(),
            artifacts: vec![ArtifactRow {
                relative_path: "result.json".to_owned(),
                bytes: 10,
            }],
        }
    }

    fn state_of(database: &HistoryDatabase, run_id: &str) -> Option<String> {
        database
            .list_runs(100)
            .unwrap()
            .into_iter()
            .find(|row| row.run_id == run_id)
            .map(|row| row.state)
    }

    #[test]
    fn a_new_history_is_private_and_has_every_table() {
        let workspace = tempfile::tempdir().unwrap();
        let database = database(workspace.path());
        assert_eq!(
            database.path(),
            workspace.path().join(".testry/state.sqlite3")
        );
        let mode = std::fs::metadata(database.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);

        let connection = database.connect().unwrap();
        let mut tables: Vec<String> = connection
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        tables.sort();
        assert_eq!(
            tables,
            [
                "artifacts",
                "duration_samples",
                "environment_leases",
                "plans",
                "runs",
                "schema_migrations",
                "tasks",
                "workspaces"
            ]
        );

        // Opening again migrates nothing twice.
        let again =
            super::HistoryDatabase::open(workspace.path(), &RepoPath::try_new(".testry").unwrap())
                .unwrap();
        let versions: i64 = again
            .connect()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(versions, 1);
    }

    #[test]
    fn a_history_of_a_newer_testry_is_refused_and_left_untouched() {
        let workspace = tempfile::tempdir().unwrap();
        let database = database(workspace.path());
        database
            .record_started(&start(RUN, "blake3:p1", 1_000))
            .unwrap();
        database
            .connect()
            .unwrap()
            .execute(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (99, 0)",
                [],
            )
            .unwrap();

        let error =
            super::HistoryDatabase::open(workspace.path(), &RepoPath::try_new(".testry").unwrap())
                .unwrap_err();
        assert!(error.to_string().contains("newer testry"), "{error}");
        // The rows are still there.
        assert_eq!(state_of(&database, RUN).as_deref(), Some("running"));
    }

    #[test]
    fn something_else_in_place_of_the_history_is_refused() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir(workspace.path().join(".testry")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("elsewhere.sqlite3"),
            workspace.path().join(".testry/state.sqlite3"),
        )
        .unwrap();
        let storage_directory = RepoPath::try_new(".testry").unwrap();
        assert!(HistoryDatabase::open(workspace.path(), &storage_directory).is_err());
        assert!(HistoryDatabase::existing(workspace.path(), &storage_directory).is_none());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_read_only_command_creates_no_history() {
        let workspace = tempfile::tempdir().unwrap();
        let storage_directory = RepoPath::try_new(".testry").unwrap();
        assert!(HistoryDatabase::existing(workspace.path(), &storage_directory).is_none());
        assert!(!workspace.path().join(".testry").exists());
    }

    #[test]
    fn a_run_is_running_until_its_terminal_facts_are_stored() {
        let workspace = tempfile::tempdir().unwrap();
        let database = database(workspace.path());
        database
            .record_started(&start(RUN, "blake3:p1", 1_000))
            .unwrap();

        let running = &database.list_runs(10).unwrap()[0];
        assert_eq!(running.state, "running");
        assert_eq!(running.outcome, None, "a running run has no outcome");
        assert_eq!(running.exit_code, None);
        assert_eq!(
            database.lease_state(RUN).unwrap().as_deref(),
            Some("planned")
        );

        database.record_finished(&finish(RUN, "passed")).unwrap();
        let finished = &database.list_runs(10).unwrap()[0];
        assert_eq!(finished.state, "finished");
        assert_eq!(finished.outcome.as_deref(), Some("passed"));
        assert_eq!(finished.exit_code, Some(0));
        assert_eq!(finished.total_ms, Some(1500));
        assert_eq!(finished.finished_at, Some(2_000));
        assert_eq!(database.count_rows(HistoryTable::Tasks, RUN).unwrap(), 2);
        assert_eq!(
            database.count_rows(HistoryTable::Artifacts, RUN).unwrap(),
            1
        );
        assert_eq!(
            database
                .count_rows(HistoryTable::DurationSamples, RUN)
                .unwrap(),
            2
        );
        assert_eq!(
            database.lease_state(RUN).unwrap().as_deref(),
            Some("released")
        );
        assert_eq!(database.duration_samples("unit", 10).unwrap(), [120]);
    }

    /// P12: a runner that cannot count has no counts. They are not zeros.
    #[test]
    fn unknown_counts_are_stored_as_null_not_as_zero() {
        let workspace = tempfile::tempdir().unwrap();
        let database = database(workspace.path());
        database
            .record_started(&start(RUN, "blake3:p1", 1_000))
            .unwrap();
        database.record_finished(&finish(RUN, "passed")).unwrap();
        let connection = database.connect().unwrap();
        let counts = |task: &str| -> (Option<i64>, Option<i64>) {
            connection
                .query_row(
                    "SELECT passed, skipped FROM tasks WHERE run_id = ?1 AND task_id = ?2",
                    params![RUN, task],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap()
        };
        assert_eq!(counts("unit"), (Some(3), Some(1)));
        assert_eq!(counts("typecheck"), (None, None));
    }

    #[test]
    fn the_terminal_record_of_a_run_is_never_replaced() {
        let workspace = tempfile::tempdir().unwrap();
        let database = database(workspace.path());
        database
            .record_started(&start(RUN, "blake3:p1", 1_000))
            .unwrap();
        database.record_finished(&finish(RUN, "failed")).unwrap();
        // A second terminal record for the same run changes nothing.
        database.record_finished(&finish(RUN, "passed")).unwrap();
        assert_eq!(
            database.list_runs(10).unwrap()[0].outcome.as_deref(),
            Some("failed")
        );

        let unknown = database
            .record_finished(&finish(OTHER, "passed"))
            .unwrap_err();
        assert!(
            unknown.to_string().contains("not in the run history"),
            "{unknown}"
        );
        assert_eq!(state_of(&database, OTHER), None);
    }

    /// A write that stops in the middle leaves nothing of itself: the run is
    /// still running, without tasks, and it can be finished afterwards.
    #[test]
    fn a_terminal_record_that_fails_halfway_is_rolled_back_completely() {
        let workspace = tempfile::tempdir().unwrap();
        let database = database(workspace.path());
        database
            .record_started(&start(RUN, "blake3:p1", 1_000))
            .unwrap();

        // The lease update is the step after the run and its tasks. A state
        // the schema does not allow makes that step fail.
        let mut broken = finish(RUN, "passed");
        broken.lease_state = "not a lease state";
        assert!(database.record_finished(&broken).is_err());

        assert_eq!(state_of(&database, RUN).as_deref(), Some("running"));
        assert_eq!(database.list_runs(10).unwrap()[0].outcome, None);
        assert_eq!(database.count_rows(HistoryTable::Tasks, RUN).unwrap(), 0);
        assert_eq!(
            database.count_rows(HistoryTable::Artifacts, RUN).unwrap(),
            0
        );
        assert_eq!(
            database
                .count_rows(HistoryTable::DurationSamples, RUN)
                .unwrap(),
            0
        );
        assert_eq!(
            database.lease_state(RUN).unwrap().as_deref(),
            Some("planned")
        );

        database.record_finished(&finish(RUN, "passed")).unwrap();
        assert_eq!(state_of(&database, RUN).as_deref(), Some("finished"));
    }

    /// The schema itself refuses a finished run without an outcome and an
    /// outcome on a run that is not finished.
    #[test]
    fn the_schema_ties_the_outcome_to_the_finished_state() {
        let workspace = tempfile::tempdir().unwrap();
        let database = database(workspace.path());
        database
            .record_started(&start(RUN, "blake3:p1", 1_000))
            .unwrap();
        let connection = database.connect().unwrap();
        assert!(
            connection
                .execute(
                    "UPDATE runs SET state = 'finished' WHERE run_id = ?1",
                    params![RUN]
                )
                .is_err()
        );
        assert!(
            connection
                .execute(
                    "UPDATE runs SET outcome = 'passed' WHERE run_id = ?1",
                    params![RUN]
                )
                .is_err()
        );
        assert!(
            connection
                .execute(
                    "UPDATE runs SET state = 'succeeded' WHERE run_id = ?1",
                    params![RUN]
                )
                .is_err()
        );
    }

    /// R15: the index is repaired from what the run directories say.
    #[test]
    fn reconciling_follows_the_run_directories() {
        let workspace = tempfile::tempdir().unwrap();
        let database = database(workspace.path());
        let id = |number: u32| format!("00000000-0000-4000-8000-{number:012x}");
        // 1: finished on disk, still running in the index (stopped between
        //    the terminal record and the index update).
        // 2: running in the index, no terminal record.
        // 3: finished in the index, directory removed by hand.
        // 4: the run of the caller.
        for number in 1..=4 {
            database
                .record_started(&start(&id(number), "blake3:p1", 1_000 + i64::from(number)))
                .unwrap();
        }
        database.record_finished(&finish(&id(3), "passed")).unwrap();
        // 5 and 6 have a directory but were never indexed; 7 has a directory
        // without a start record.
        let directories: Vec<String> = [1, 2, 4, 5, 6, 7].into_iter().map(id).collect();

        let stored = |run_id: &str| -> StoredState {
            let number = u32::from_str_radix(&run_id[run_id.len() - 4..], 16).unwrap();
            match number {
                1 | 5 => StoredState::Finished(Box::new(finish(run_id, "failed"))),
                2 | 4 | 6 | 7 => StoredState::Unfinished,
                _ => StoredState::Missing,
            }
        };
        let start_of = |run_id: &str| -> Option<RunStart> {
            (!run_id.ends_with('7')).then(|| start(run_id, "blake3:p2", 900))
        };
        let active = RunId::try_new(id(4)).unwrap();
        let report = database
            .reconcile(&directories, Some(&active), &stored, &start_of)
            .unwrap();

        assert_eq!(report.completed, [id(1), id(5)]);
        assert_eq!(report.interrupted, [id(2), id(6)]);
        assert_eq!(report.removed, [id(3)]);

        assert_eq!(state_of(&database, &id(1)).as_deref(), Some("finished"));
        assert_eq!(state_of(&database, &id(2)).as_deref(), Some("interrupted"));
        assert_eq!(state_of(&database, &id(3)), None);
        assert_eq!(
            state_of(&database, &id(4)).as_deref(),
            Some("running"),
            "the run of the caller is running, not interrupted"
        );
        assert_eq!(state_of(&database, &id(5)).as_deref(), Some("finished"));
        assert_eq!(state_of(&database, &id(6)).as_deref(), Some("interrupted"));
        assert_eq!(
            state_of(&database, &id(7)),
            None,
            "not a run: no start record"
        );

        // The outcome comes from the terminal record on disk.
        let rows = database.list_runs(10).unwrap();
        let outcome = |run_id: &str| {
            rows.iter()
                .find(|row| row.run_id == run_id)
                .and_then(|row| row.outcome.clone())
        };
        assert_eq!(outcome(&id(1)).as_deref(), Some("failed"));
        assert_eq!(outcome(&id(2)), None, "an interrupted run has no outcome");
        assert_eq!(
            database.lease_state(&id(2)).unwrap().as_deref(),
            Some("abandoned")
        );
        assert_eq!(
            database.lease_state(&id(1)).unwrap().as_deref(),
            Some("released")
        );

        // Doing it again changes nothing.
        let again = database
            .reconcile(&directories, Some(&active), &stored, &start_of)
            .unwrap();
        assert!(again.is_empty(), "{again:?}");
    }

    /// A finished run stays finished even if its terminal record disappears
    /// from disk later: the index never takes a result back, and it never
    /// invents one either.
    #[test]
    fn reconciling_never_takes_back_or_invents_a_result() {
        let workspace = tempfile::tempdir().unwrap();
        let database = database(workspace.path());
        database
            .record_started(&start(RUN, "blake3:p1", 1_000))
            .unwrap();
        database.record_finished(&finish(RUN, "failed")).unwrap();
        database
            .record_started(&start(OTHER, "blake3:p1", 1_001))
            .unwrap();

        let unfinished = |_: &str| StoredState::Unfinished;
        let report = database
            .reconcile(
                &[RUN.to_owned(), OTHER.to_owned()],
                None,
                &unfinished,
                &|_| None,
            )
            .unwrap();
        assert_eq!(report.interrupted, [OTHER]);
        let rows = database.list_runs(10).unwrap();
        let of = |run_id: &str| rows.iter().find(|row| row.run_id == run_id).unwrap();
        assert_eq!(of(RUN).state, "finished");
        assert_eq!(of(RUN).outcome.as_deref(), Some("failed"));
        assert_eq!(of(OTHER).state, "interrupted");
        assert_eq!(of(OTHER).outcome, None);

        // A terminal record that claims another outcome later does not
        // rewrite the finished run.
        let passed = |run_id: &str| StoredState::Finished(Box::new(finish(run_id, "passed")));
        database
            .reconcile(&[RUN.to_owned()], None, &passed, &|_| None)
            .unwrap();
        assert_eq!(
            database
                .list_runs(10)
                .unwrap()
                .iter()
                .find(|row| row.run_id == RUN)
                .unwrap()
                .outcome
                .as_deref(),
            Some("failed")
        );
    }

    #[test]
    fn removing_runs_removes_everything_that_belongs_to_them() {
        let workspace = tempfile::tempdir().unwrap();
        let database = database(workspace.path());
        database
            .record_started(&start(RUN, "blake3:p1", 1_000))
            .unwrap();
        database.record_finished(&finish(RUN, "passed")).unwrap();
        database
            .record_started(&start(OTHER, "blake3:p2", 1_001))
            .unwrap();
        database.record_finished(&finish(OTHER, "passed")).unwrap();

        database.delete_runs(&[RUN.to_owned()]).unwrap();
        assert_eq!(state_of(&database, RUN), None);
        for table in [
            HistoryTable::Tasks,
            HistoryTable::Artifacts,
            HistoryTable::DurationSamples,
            HistoryTable::EnvironmentLeases,
        ] {
            assert_eq!(database.count_rows(table, RUN).unwrap(), 0, "{table:?}");
            assert!(database.count_rows(table, OTHER).unwrap() > 0, "{table:?}");
        }
        let plans: Vec<String> = database
            .connect()
            .unwrap()
            .prepare("SELECT plan_id FROM plans ORDER BY plan_id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(plans, ["blake3:p2"], "a plan without runs is removed");
    }

    /// Several runs can start within one second. They are still listed in
    /// the order they ran in.
    #[test]
    fn runs_of_the_same_second_are_listed_in_the_order_they_ran() {
        let workspace = tempfile::tempdir().unwrap();
        let database = database(workspace.path());
        // IDs that sort the other way round than the runs ran.
        let ids = [
            "ffffffff-ffff-4fff-8fff-ffffffffffff",
            "00000000-0000-4000-8000-000000000000",
            "88888888-8888-4888-8888-888888888888",
        ];
        for run_id in ids {
            database
                .record_started(&start(run_id, "blake3:p1", 5_000))
                .unwrap();
            database.record_finished(&finish(run_id, "passed")).unwrap();
        }
        let listed: Vec<String> = database
            .list_runs(10)
            .unwrap()
            .into_iter()
            .map(|row| row.run_id)
            .collect();
        assert_eq!(listed, [ids[2], ids[1], ids[0]]);
    }

    #[test]
    fn runs_are_listed_newest_first_and_durations_too() {
        let workspace = tempfile::tempdir().unwrap();
        let database = database(workspace.path());
        for (number, started_at) in [(1, 300), (2, 100), (3, 200)] {
            let run_id = format!("00000000-0000-4000-8000-{number:012x}");
            database
                .record_started(&start(&run_id, "blake3:p1", started_at))
                .unwrap();
            let mut finished = finish(&run_id, "passed");
            finished.finished_at = started_at + 1;
            finished.tasks[0].duration_ms = u64::try_from(started_at).unwrap();
            database.record_finished(&finished).unwrap();
        }
        let order: Vec<i64> = database
            .list_runs(2)
            .unwrap()
            .iter()
            .map(|row| row.started_at)
            .collect();
        assert_eq!(order, [300, 200], "newest first, limited");
        assert_eq!(
            database.duration_samples("unit", 10).unwrap(),
            [300, 200, 100]
        );
        assert!(database.duration_samples("unknown", 10).unwrap().is_empty());
    }
}
