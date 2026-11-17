//! Plan documents on disk (spec section 13).
//!
//! Plans are written below `<storage>/plans/`. The storage directory is
//! created with mode 0700 and files with mode 0600. A document is written to a
//! temporary file first and renamed into place, so a reader never sees a
//! half-written plan. Symbolic links are refused: nothing is read or written
//! through one.

use std::path::{Path, PathBuf};

use testry_application::ports::PortError;
use testry_domain::paths::RepoPath;
use testry_domain::plan::{ExecutionPlan, PlanDocumentError};

use super::private::{ensure_private_directory, write_private_file};

/// Upper bound for a plan document that is read back.
const MAX_PLAN_BYTES: u64 = 64 * 1024 * 1024;

const PLANS_DIRECTORY: &str = "plans";

pub struct PlanFileStore {
    storage_directory: PathBuf,
}

impl PlanFileStore {
    /// `storage_directory` is `storage.directory`, relative to the workspace.
    pub fn new(workspace_root: &Path, storage_directory: &RepoPath) -> Self {
        Self {
            storage_directory: workspace_root.join(storage_directory.as_str()),
        }
    }

    /// Stores a plan under its ID and returns the path of the document.
    pub fn store(&self, plan: &ExecutionPlan) -> Result<PathBuf, PortError> {
        ensure_private_directory(&self.storage_directory)?;
        // State must never be committed, whatever the repository ignores.
        let ignore = self.storage_directory.join(".gitignore");
        if std::fs::symlink_metadata(&ignore).is_err() {
            write_private_file(&ignore, b"*\n")?;
        }
        let plans = self.storage_directory.join(PLANS_DIRECTORY);
        ensure_private_directory(&plans)?;

        let json = plan.to_json().map_err(|error| PortError::Storage {
            message: error.to_string(),
        })?;
        // `blake3:<hex>` is not a portable file name.
        let path = plans.join(format!("{}.json", plan.id().to_string().replace(':', "-")));
        write_private_file(&path, format!("{json}\n").as_bytes())?;
        Ok(path)
    }
}

/// Reads a plan document. The document is validated: another schema version,
/// a modified document or a violated plan invariant is an error.
pub fn read_plan(path: &Path) -> Result<ExecutionPlan, PortError> {
    let invalid = |message: String| PortError::Configuration { message };
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        invalid(format!(
            "cannot read plan {}: {}",
            path.display(),
            error.kind()
        ))
    })?;
    if !metadata.is_file() {
        return Err(invalid(format!(
            "plan {} is not a regular file",
            path.display()
        )));
    }
    if metadata.len() > MAX_PLAN_BYTES {
        return Err(invalid(format!(
            "plan {} is larger than {MAX_PLAN_BYTES} bytes",
            path.display()
        )));
    }
    let bytes = std::fs::read(path).map_err(|error| {
        invalid(format!(
            "cannot read plan {}: {}",
            path.display(),
            error.kind()
        ))
    })?;
    ExecutionPlan::from_json(&bytes).map_err(|error: PlanDocumentError| {
        invalid(format!("plan {} was rejected: {error}", path.display()))
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use testry_domain::fingerprint::Fingerprint;
    use testry_domain::ids::{SourceSnapshotId, SuiteId, WorkspaceId};
    use testry_domain::plan::{
        CapabilityVerification, Mode, PlanDraft, SelectionReason, SuitePlan,
    };
    use testry_domain::resources::{ResourceBudget, ResourceRequest};
    use testry_domain::selection::Selection;
    use testry_domain::suite::RunnerKind;

    use super::*;

    fn plan() -> ExecutionPlan {
        ExecutionPlan::try_new(PlanDraft {
            workspace_id: WorkspaceId::try_new("ws-test").unwrap(),
            snapshot_id: SourceSnapshotId::new(Fingerprint::of_bytes(b"snapshot")),
            baseline_commit: None,
            config_digest: Fingerprint::of_bytes(b"config"),
            mode: Mode::Full,
            scope_limited: false,
            changed_paths: None,
            suites: vec![SuitePlan {
                suite_id: SuiteId::try_new("unit").unwrap(),
                runner: RunnerKind::Bun,
                project: None,
                requested_selection: Selection::All,
                effective_selection: Selection::All,
                reasons: vec![SelectionReason::FullVerification],
                candidate: None,
                capability_verification: CapabilityVerification::NotRequired,
                resources: ResourceRequest::try_new(1, 256, []).unwrap(),
            }],
            fallbacks: Vec::new(),
            resources: ResourceBudget::try_new(2, 1024, 1).unwrap(),
            diagnostics: Vec::new(),
        })
        .unwrap()
    }

    fn store(root: &Path) -> PlanFileStore {
        PlanFileStore::new(root, &RepoPath::try_new(".testry").unwrap())
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn stores_a_plan_privately_and_reads_it_back() {
        let root = tempfile::tempdir().unwrap();
        let plan = plan();
        let path = store(root.path()).store(&plan).unwrap();

        assert!(path.starts_with(root.path().join(".testry/plans")));
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("blake3-") && name.ends_with(".json") && !name.contains(':'));
        assert_eq!(mode_of(&root.path().join(".testry")), 0o700);
        assert_eq!(mode_of(&root.path().join(".testry/plans")), 0o700);
        assert_eq!(mode_of(&path), 0o600);
        assert_eq!(
            std::fs::read_to_string(root.path().join(".testry/.gitignore")).unwrap(),
            "*\n"
        );
        assert_eq!(read_plan(&path).unwrap(), plan);

        // Storing the same plan again is idempotent and leaves no temporary file.
        assert_eq!(store(root.path()).store(&plan).unwrap(), path);
        let leftovers: Vec<_> = std::fs::read_dir(root.path().join(".testry/plans"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn refuses_to_write_through_a_symbolic_link() {
        let root = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), root.path().join(".testry")).unwrap();
        let error = store(root.path()).store(&plan()).unwrap_err();
        assert_eq!(error.code(), "storage");
        assert_eq!(std::fs::read_dir(elsewhere.path()).unwrap().count(), 0);

        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".testry")).unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), root.path().join(".testry/plans")).unwrap();
        assert_eq!(
            store(root.path()).store(&plan()).unwrap_err().code(),
            "storage"
        );
        assert_eq!(std::fs::read_dir(elsewhere.path()).unwrap().count(), 0);
    }

    #[test]
    fn an_existing_gitignore_is_kept() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".testry")).unwrap();
        std::fs::write(root.path().join(".testry/.gitignore"), "custom\n").unwrap();
        store(root.path()).store(&plan()).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join(".testry/.gitignore")).unwrap(),
            "custom\n"
        );
    }

    #[test]
    fn rejects_documents_that_are_not_valid_plans() {
        let root = tempfile::tempdir().unwrap();
        let path = store(root.path()).store(&plan()).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let other_version = root.path().join("v2.json");
        std::fs::write(
            &other_version,
            text.replace("\"schema_version\": 1", "\"schema_version\": 2"),
        )
        .unwrap();
        let error = read_plan(&other_version).unwrap_err();
        assert_eq!(error.code(), "configuration");
        assert!(error.to_string().contains("schema version 2"), "{error}");

        let tampered = root.path().join("tampered.json");
        std::fs::write(
            &tampered,
            text.replace("\"mode\": \"full\"", "\"mode\": \"affected\""),
        )
        .unwrap();
        assert!(read_plan(&tampered).is_err());

        let garbage = root.path().join("garbage.json");
        std::fs::write(&garbage, "{").unwrap();
        assert!(read_plan(&garbage).is_err());

        assert!(read_plan(&root.path().join("missing.json")).is_err());
        assert!(read_plan(root.path()).is_err(), "a directory is not a plan");

        let link = root.path().join("link.json");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(
            read_plan(&link).is_err(),
            "a plan is not read through a symbolic link"
        );
    }
}
