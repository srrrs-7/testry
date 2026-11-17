//! Retention of what testry stored in a workspace (`testry gc`, spec
//! sections 13 and 14).
//!
//! Only things testry can prove it owns are ever removed:
//!
//! - a run directory is owned when it is a real directory directly below
//!   `<storage>/runs`, its name is a run ID, and it holds the start record
//!   and the plan of exactly that run;
//! - a plan file is owned when it is a regular file directly below
//!   `<storage>/plans` whose content is a valid plan with the ID its name
//!   says.
//!
//! Everything else in those two directories is reported as unmanaged and
//! left alone. A run is removed once it ended longer ago than the retention
//! period; a plan once it is that old and no kept run refers to it.
//!
//! [`survey`] reads and decides. [`apply`] removes, after checking ownership
//! of each item again.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use testry_application::ports::PortError;
use testry_domain::ids::RunId;
use testry_domain::paths::RepoPath;

use super::history::StoredState;
use super::plan_files::read_plan;
use super::run_files::{stored_start, stored_state};

const SECONDS_PER_DAY: i64 = 86_400;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcAction {
    Keep,
    Delete,
}

impl GcAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Keep => "keep",
            Self::Delete => "delete",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcRun {
    pub run_id: String,
    /// `finished`, `interrupted`, or `unverified` when the directory has the
    /// name of a run but not its start record.
    pub state: &'static str,
    /// When the run ended (finished) or started (interrupted), in seconds
    /// since the Unix epoch. `None` for an unverified directory.
    pub ended_at: Option<i64>,
    pub plan_id: Option<String>,
    pub bytes: u64,
    pub action: GcAction,
    pub reason: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcPlanFile {
    pub plan_id: String,
    pub file_name: String,
    pub modified_at: i64,
    pub bytes: u64,
    pub action: GcAction,
    pub reason: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcSurvey {
    /// Things that ended before this moment are older than the retention.
    pub cutoff: i64,
    pub runs: Vec<GcRun>,
    pub plans: Vec<GcPlanFile>,
    /// Entries below `runs/` and `plans/` that testry does not own, as paths
    /// relative to the storage directory.
    pub unmanaged: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GcApplied {
    pub removed_runs: Vec<String>,
    pub removed_plans: Vec<String>,
    pub freed_bytes: u64,
    /// What should have been removed but was not, with the reason.
    pub failures: Vec<String>,
}

fn storage_path(workspace_root: &Path, storage_directory: &RepoPath) -> PathBuf {
    workspace_root.join(storage_directory.as_str())
}

fn unix_seconds(time: std::time::SystemTime) -> i64 {
    time.duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

/// The entries of a directory with their names, sorted. A missing directory
/// has none.
fn entries(directory: &Path) -> Result<Vec<(String, PathBuf)>, PortError> {
    let read = match std::fs::read_dir(directory) {
        Ok(read) => read,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(PortError::Storage {
                message: format!("cannot list {}: {}", directory.display(), error.kind()),
            });
        }
    };
    let mut found: Vec<(String, PathBuf)> = read
        .flatten()
        .map(|entry| {
            (
                entry.file_name().to_string_lossy().into_owned(),
                entry.path(),
            )
        })
        .collect();
    found.sort();
    Ok(found)
}

/// The size of the regular files below a directory. Links are not followed.
fn size_of(directory: &Path) -> u64 {
    let mut total = 0;
    let mut pending = vec![directory.to_path_buf()];
    while let Some(current) = pending.pop() {
        let Ok(read) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in read.flatten() {
            let Ok(metadata) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if metadata.is_dir() {
                pending.push(entry.path());
            } else if metadata.is_file() {
                total += metadata.len();
            }
        }
    }
    total
}

fn is_real_directory(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

/// The plan ID a plan file name stands for: `blake3-<hex>.json` is
/// `blake3:<hex>`.
fn plan_id_of_file_name(name: &str) -> Option<String> {
    let stem = name.strip_suffix(".json")?;
    let hex = stem.strip_prefix("blake3-")?;
    (hex.len() == 64
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()))
    .then(|| format!("blake3:{hex}"))
}

/// Whether the file is a valid plan whose ID is the one its name says.
fn is_owned_plan(path: &Path, plan_id: &str) -> bool {
    read_plan(path).is_ok_and(|plan| plan.id().to_string() == plan_id)
}

/// Decides what to keep. Nothing is modified.
pub fn survey(
    workspace_root: &Path,
    storage_directory: &RepoPath,
    retention_days: u32,
    now: std::time::SystemTime,
) -> Result<GcSurvey, PortError> {
    let storage = storage_path(workspace_root, storage_directory);
    let cutoff = unix_seconds(now) - i64::from(retention_days) * SECONDS_PER_DAY;
    let mut survey = GcSurvey {
        cutoff,
        runs: Vec::new(),
        plans: Vec::new(),
        unmanaged: Vec::new(),
    };

    for (name, path) in entries(&storage.join("runs"))? {
        if !is_real_directory(&path) || RunId::try_new(name.as_str()).is_err() {
            survey.unmanaged.push(format!("runs/{name}"));
            continue;
        }
        let bytes = size_of(&path);
        let Some(start) = stored_start(workspace_root, storage_directory, &name) else {
            // It has the name of a run, but nothing proves it is one.
            survey.runs.push(GcRun {
                run_id: name,
                state: "unverified",
                ended_at: None,
                plan_id: None,
                bytes,
                action: GcAction::Keep,
                reason: "no_valid_start_record",
            });
            continue;
        };
        let (state, ended_at) = match stored_state(workspace_root, storage_directory, &name) {
            StoredState::Finished(finish) => ("finished", finish.finished_at),
            StoredState::Unfinished | StoredState::Missing => ("interrupted", start.started_at),
        };
        let (action, reason) = if ended_at < cutoff {
            (GcAction::Delete, "older_than_retention")
        } else {
            (GcAction::Keep, "within_retention")
        };
        survey.runs.push(GcRun {
            run_id: name,
            state,
            ended_at: Some(ended_at),
            plan_id: Some(start.plan_id),
            bytes,
            action,
            reason,
        });
    }

    let referenced: BTreeSet<&str> = survey
        .runs
        .iter()
        .filter(|run| run.action == GcAction::Keep)
        .filter_map(|run| run.plan_id.as_deref())
        .collect();
    let mut plans = Vec::new();
    for (name, path) in entries(&storage.join("plans"))? {
        let metadata = std::fs::symlink_metadata(&path).ok();
        let owned_id = plan_id_of_file_name(&name).filter(|plan_id| {
            metadata.as_ref().is_some_and(std::fs::Metadata::is_file)
                && is_owned_plan(&path, plan_id)
        });
        let (Some(plan_id), Some(metadata)) = (owned_id, metadata) else {
            survey.unmanaged.push(format!("plans/{name}"));
            continue;
        };
        let modified_at = metadata.modified().map_or(0, unix_seconds);
        let (action, reason) = if referenced.contains(plan_id.as_str()) {
            (GcAction::Keep, "referenced_by_a_kept_run")
        } else if modified_at < cutoff {
            (GcAction::Delete, "older_than_retention_and_unreferenced")
        } else {
            (GcAction::Keep, "within_retention")
        };
        plans.push(GcPlanFile {
            plan_id,
            file_name: name,
            modified_at,
            bytes: metadata.len(),
            action,
            reason,
        });
    }
    survey.plans = plans;
    Ok(survey)
}

/// Removes what the survey marked. Ownership of every item is checked again
/// right before it is removed; an item that no longer passes is left alone
/// and reported. The caller holds the workspace lock.
pub fn apply(workspace_root: &Path, storage_directory: &RepoPath, survey: &GcSurvey) -> GcApplied {
    let storage = storage_path(workspace_root, storage_directory);
    let mut applied = GcApplied::default();

    for run in survey
        .runs
        .iter()
        .filter(|run| run.action == GcAction::Delete)
    {
        let path = storage.join("runs").join(&run.run_id);
        let still_owned = is_real_directory(&path)
            && stored_start(workspace_root, storage_directory, &run.run_id).is_some();
        if !still_owned {
            applied.failures.push(format!(
                "run {} is no longer a run directory of this workspace; left alone",
                run.run_id
            ));
            continue;
        }
        // Links inside the directory are removed as links, never followed.
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {
                applied.freed_bytes += run.bytes;
                applied.removed_runs.push(run.run_id.clone());
            }
            Err(error) => applied.failures.push(format!(
                "run {} could not be removed: {}",
                run.run_id,
                error.kind()
            )),
        }
    }

    for plan in survey
        .plans
        .iter()
        .filter(|plan| plan.action == GcAction::Delete)
    {
        let path = storage.join("plans").join(&plan.file_name);
        let still_owned = std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_file())
            && is_owned_plan(&path, &plan.plan_id);
        if !still_owned {
            applied.failures.push(format!(
                "plan {} is no longer a plan file of this workspace; left alone",
                plan.plan_id
            ));
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                applied.freed_bytes += plan.bytes;
                applied.removed_plans.push(plan.plan_id.clone());
            }
            Err(error) => applied.failures.push(format!(
                "plan {} could not be removed: {}",
                plan.plan_id,
                error.kind()
            )),
        }
    }
    applied
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::storage::plan_files::PlanFileStore;
    use crate::storage::test_support::{finished_run, plan, started_run, storage};

    const DAY: Duration = Duration::from_secs(86_400);

    fn run_path(workspace: &Path, run_id: &str) -> PathBuf {
        workspace.join(".testry/runs").join(run_id)
    }

    fn found<'a>(survey: &'a GcSurvey, run_id: &str) -> &'a GcRun {
        survey
            .runs
            .iter()
            .find(|run| run.run_id == run_id)
            .unwrap_or_else(|| panic!("{run_id} is not in {survey:?}"))
    }

    /// Makes a finished run look as if it ended `age` ago.
    fn age_result(workspace: &Path, run_id: &str, age: Duration) {
        let result = std::fs::File::options()
            .write(true)
            .open(run_path(workspace, run_id).join("result.json"))
            .unwrap();
        result.set_modified(SystemTime::now() - age).unwrap();
    }

    #[tokio::test]
    async fn only_runs_that_ended_before_the_retention_period_are_removed() {
        let workspace = tempfile::tempdir().unwrap();
        let old = finished_run(workspace.path(), 1, None).await;
        let recent = finished_run(workspace.path(), 2, None).await;
        age_result(workspace.path(), old.as_str(), 15 * DAY);
        age_result(workspace.path(), recent.as_str(), 13 * DAY);

        let survey = survey(workspace.path(), &storage(), 14, SystemTime::now()).unwrap();
        let old_run = found(&survey, old.as_str());
        assert_eq!(old_run.state, "finished");
        assert_eq!(
            (old_run.action, old_run.reason),
            (GcAction::Delete, "older_than_retention")
        );
        assert!(old_run.bytes > 0);
        let recent_run = found(&survey, recent.as_str());
        assert_eq!(
            (recent_run.action, recent_run.reason),
            (GcAction::Keep, "within_retention")
        );

        // Surveying changes nothing.
        assert!(run_path(workspace.path(), old.as_str()).is_dir());

        let applied = apply(workspace.path(), &storage(), &survey);
        assert_eq!(applied.removed_runs, [old.as_str()]);
        assert_eq!(applied.freed_bytes, old_run.bytes);
        assert!(applied.failures.is_empty(), "{:?}", applied.failures);
        assert!(!run_path(workspace.path(), old.as_str()).exists());
        assert!(
            run_path(workspace.path(), recent.as_str())
                .join("result.json")
                .is_file()
        );
    }

    /// An interrupted run is evidence. It is kept as long as any other run,
    /// counted from its start.
    #[tokio::test]
    async fn an_interrupted_run_is_kept_until_it_is_old_as_well() {
        let workspace = tempfile::tempdir().unwrap();
        let (_, interrupted, _) = started_run(workspace.path(), 1, None).await;

        let now = survey(workspace.path(), &storage(), 14, SystemTime::now()).unwrap();
        let run = found(&now, interrupted.as_str());
        assert_eq!(run.state, "interrupted");
        assert_eq!(run.action, GcAction::Keep);

        let later = survey(
            workspace.path(),
            &storage(),
            14,
            SystemTime::now() + 15 * DAY,
        )
        .unwrap();
        assert_eq!(found(&later, interrupted.as_str()).action, GcAction::Delete);
    }

    /// Nothing is removed that testry cannot prove it owns, however old.
    #[tokio::test]
    async fn what_testry_does_not_own_is_reported_and_left_alone() {
        let workspace = tempfile::tempdir().unwrap();
        let owned = finished_run(workspace.path(), 1, None).await;
        let runs = workspace.path().join(".testry/runs");
        let plans = workspace.path().join(".testry/plans");
        std::fs::create_dir_all(&plans).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("precious.txt"), "keep me").unwrap();

        // In `runs/`: a file, a directory with another name, a link with the
        // name of a run, a directory with the name of a run but no start
        // record, and one with the start record of another run.
        std::fs::write(runs.join("notes.txt"), "mine").unwrap();
        std::fs::create_dir(runs.join("backup")).unwrap();
        std::fs::write(runs.join("backup/data"), "mine").unwrap();
        let linked = "00000000-0000-4000-8000-0000000000aa";
        std::os::unix::fs::symlink(outside.path(), runs.join(linked)).unwrap();
        let empty = "00000000-0000-4000-8000-0000000000bb";
        std::fs::create_dir(runs.join(empty)).unwrap();
        let copied = "00000000-0000-4000-8000-0000000000cc";
        std::fs::create_dir(runs.join(copied)).unwrap();
        for file in ["run.json", "plan.json", "result.json"] {
            std::fs::copy(
                runs.join(owned.as_str()).join(file),
                runs.join(copied).join(file),
            )
            .unwrap();
        }
        // In `plans/`: a file with another name, a file with the name of a
        // plan but other content, and a link.
        std::fs::write(plans.join("mine.json"), "{}").unwrap();
        let fake = format!("blake3-{}.json", "0".repeat(64));
        std::fs::write(plans.join(&fake), "{}").unwrap();
        std::os::unix::fs::symlink(outside.path().join("precious.txt"), plans.join("link.json"))
            .unwrap();

        let far_future = SystemTime::now() + 3650 * DAY;
        let survey = survey(workspace.path(), &storage(), 14, far_future).unwrap();
        assert_eq!(
            survey.unmanaged,
            [
                format!("runs/{linked}"),
                "runs/backup".to_owned(),
                "runs/notes.txt".to_owned(),
                format!("plans/{fake}"),
                "plans/link.json".to_owned(),
                "plans/mine.json".to_owned(),
            ]
        );
        for unverified in [empty, copied] {
            let run = found(&survey, unverified);
            assert_eq!(run.state, "unverified");
            assert_eq!(
                (run.action, run.reason),
                (GcAction::Keep, "no_valid_start_record")
            );
        }
        assert_eq!(found(&survey, owned.as_str()).action, GcAction::Delete);
        assert!(survey.plans.is_empty());

        let applied = apply(workspace.path(), &storage(), &survey);
        assert_eq!(applied.removed_runs, [owned.as_str()]);
        assert!(applied.removed_plans.is_empty());
        for kept in ["notes.txt", "backup/data", empty, copied] {
            assert!(runs.join(kept).exists(), "{kept} was removed");
        }
        assert!(std::fs::symlink_metadata(runs.join(linked)).is_ok());
        for kept in ["mine.json", fake.as_str(), "link.json"] {
            assert!(
                std::fs::symlink_metadata(plans.join(kept)).is_ok(),
                "{kept} was removed"
            );
        }
        assert_eq!(
            std::fs::read_to_string(outside.path().join("precious.txt")).unwrap(),
            "keep me"
        );
    }

    #[tokio::test]
    async fn a_plan_is_kept_while_a_kept_run_refers_to_it() {
        let workspace = tempfile::tempdir().unwrap();
        let plan_store = PlanFileStore::new(workspace.path(), &storage());
        // The plan of the run that is kept, and a plan no run refers to.
        let (_, _, used) = started_run(workspace.path(), 1, None).await;
        let used_path = plan_store.store(&used).unwrap();
        let unused = plan("nobody ran this");
        let unused_path = plan_store.store(&unused).unwrap();
        for path in [&used_path, &unused_path] {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(SystemTime::now() - 30 * DAY)
                .unwrap();
        }

        let survey = survey(workspace.path(), &storage(), 14, SystemTime::now()).unwrap();
        let of = |plan_id: String| {
            survey
                .plans
                .iter()
                .find(|plan| plan.plan_id == plan_id)
                .unwrap()
        };
        let kept = of(used.id().to_string());
        assert_eq!(
            (kept.action, kept.reason),
            (GcAction::Keep, "referenced_by_a_kept_run")
        );
        let removed = of(unused.id().to_string());
        assert_eq!(
            (removed.action, removed.reason),
            (GcAction::Delete, "older_than_retention_and_unreferenced")
        );

        let applied = apply(workspace.path(), &storage(), &survey);
        assert_eq!(applied.removed_plans, [unused.id().to_string()]);
        assert!(used_path.is_file());
        assert!(!unused_path.exists());

        // A fresh plan is kept although nothing refers to it yet.
        let fresh = plan("just planned");
        plan_store.store(&fresh).unwrap();
        let again = super::survey(workspace.path(), &storage(), 14, SystemTime::now()).unwrap();
        let fresh_plan = again
            .plans
            .iter()
            .find(|plan| plan.plan_id == fresh.id().to_string())
            .unwrap();
        assert_eq!(
            (fresh_plan.action, fresh_plan.reason),
            (GcAction::Keep, "within_retention")
        );
    }

    /// What was surveyed may have changed before it is removed. Ownership is
    /// checked again, and something that no longer passes is left alone.
    #[tokio::test]
    async fn ownership_is_checked_again_right_before_removing() {
        let workspace = tempfile::tempdir().unwrap();
        let replaced = finished_run(workspace.path(), 1, None).await;
        let emptied = finished_run(workspace.path(), 2, None).await;
        let far_future = SystemTime::now() + 3650 * DAY;
        let survey = survey(workspace.path(), &storage(), 14, far_future).unwrap();
        assert_eq!(found(&survey, replaced.as_str()).action, GcAction::Delete);

        // Between the survey and the removal, one run directory becomes a
        // link to somewhere else and the other loses its start record.
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("precious.txt"), "keep me").unwrap();
        let replaced_path = run_path(workspace.path(), replaced.as_str());
        std::fs::remove_dir_all(&replaced_path).unwrap();
        std::os::unix::fs::symlink(outside.path(), &replaced_path).unwrap();
        let emptied_path = run_path(workspace.path(), emptied.as_str());
        std::fs::remove_file(emptied_path.join("run.json")).unwrap();

        let applied = apply(workspace.path(), &storage(), &survey);
        assert!(
            applied.removed_runs.is_empty(),
            "{:?}",
            applied.removed_runs
        );
        assert_eq!(applied.failures.len(), 2, "{:?}", applied.failures);
        assert_eq!(applied.freed_bytes, 0);
        assert!(outside.path().join("precious.txt").is_file());
        assert!(emptied_path.join("result.json").is_file());
    }

    #[test]
    fn a_workspace_without_stored_state_has_nothing_to_remove() {
        let workspace = tempfile::tempdir().unwrap();
        let survey = survey(workspace.path(), &storage(), 14, SystemTime::now()).unwrap();
        assert!(survey.runs.is_empty() && survey.plans.is_empty() && survey.unmanaged.is_empty());
        assert_eq!(
            apply(workspace.path(), &storage(), &survey),
            GcApplied::default()
        );
        assert!(
            !workspace.path().join(".testry").exists(),
            "nothing is created"
        );
    }

    #[test]
    fn plan_file_names_are_recognized_exactly() {
        let hex = "ab".repeat(32);
        assert_eq!(
            plan_id_of_file_name(&format!("blake3-{hex}.json")),
            Some(format!("blake3:{hex}"))
        );
        for other in [
            format!("blake3-{hex}"),
            format!("blake3-{hex}.json.bak"),
            format!("blake3-{}.json", &hex[1..]),
            format!("blake3-{}.json", hex.to_uppercase()),
            format!("sha256-{hex}.json"),
            format!(".blake3-{hex}.json.123.tmp"),
        ] {
            assert_eq!(plan_id_of_file_name(&other), None, "{other}");
        }
    }
}
