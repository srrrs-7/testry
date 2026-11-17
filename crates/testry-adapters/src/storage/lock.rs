//! Exclusive lock for runs in one workspace (spec section 6).
//!
//! Two runs in the same work tree would fingerprint each other's artifacts
//! and compete for the same resources. The lock is an advisory file lock on
//! `<storage>/locks/workspace.lock`; it is released when the guard is dropped
//! or the process dies. It does not, and cannot, stop an editor from changing
//! files; that is what the input fingerprint is for.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use testry_application::ports::PortError;
use testry_domain::ids::RunId;
use testry_domain::paths::RepoPath;

use super::private::{ensure_private_directory, ensure_storage_directory, storage_error};

/// Holds the workspace lock until it is dropped.
#[derive(Debug)]
pub struct WorkspaceLock {
    _file: File,
}

impl WorkspaceLock {
    /// Takes the lock or fails at once when another run holds it. It never
    /// waits: a second run would work on inputs the first one may change.
    pub fn acquire(
        workspace_root: &Path,
        storage_directory: &RepoPath,
        run_id: &RunId,
    ) -> Result<Self, PortError> {
        let storage = ensure_storage_directory(workspace_root, storage_directory)?;
        let locks = storage.join("locks");
        ensure_private_directory(&locks)?;
        let path = locks.join("workspace.lock");
        if std::fs::symlink_metadata(&path).is_ok_and(|metadata| !metadata.is_file()) {
            return Err(PortError::Storage {
                message: format!("{} is not a regular file", path.display()),
            });
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .map_err(|error| storage_error("open", &path, &error))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                let holder = std::fs::read_to_string(&path).unwrap_or_default();
                return Err(PortError::WorkspaceBusy {
                    message: format!(
                        "another testry run is active in this workspace ({}); wait for it to finish",
                        holder.trim()
                    ),
                });
            }
            Err(TryLockError::Error(error)) => return Err(storage_error("lock", &path, &error)),
        }
        // Informational only; the lock itself is what excludes other runs.
        let note = format!("run {run_id}, pid {}\n", std::process::id());
        file.set_len(0)
            .and_then(|()| file.write_all(note.as_bytes()))
            .map_err(|error| storage_error("write", &path, &error))?;
        Ok(Self { _file: file })
    }
}

/// Whether a run currently holds the lock of the workspace. Nothing is
/// created or written: a workspace without a lock file has no active run.
pub fn workspace_is_busy(workspace_root: &Path, storage_directory: &RepoPath) -> bool {
    let path = workspace_root
        .join(storage_directory.as_str())
        .join("locks")
        .join("workspace.lock");
    if !std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
        return false;
    }
    let Ok(file) = OpenOptions::new().read(true).open(&path) else {
        return false;
    };
    // The lock is released again as soon as `file` is dropped.
    matches!(file.try_lock(), Err(TryLockError::WouldBlock))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn storage() -> RepoPath {
        RepoPath::try_new(".testry").unwrap()
    }

    fn run(suffix: u8) -> RunId {
        RunId::try_new(format!("00000000-0000-4000-8000-0000000000{suffix:02x}")).unwrap()
    }

    /// R17: a second run in the same workspace is refused while one is active.
    #[test]
    fn r17_only_one_run_holds_the_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let first = WorkspaceLock::acquire(workspace.path(), &storage(), &run(1)).unwrap();

        let error = WorkspaceLock::acquire(workspace.path(), &storage(), &run(2)).unwrap_err();
        assert_eq!(error.code(), "workspace_busy");
        assert_eq!(error.exit_condition().exit_code(), 3);
        assert!(
            error
                .to_string()
                .contains("00000000-0000-4000-8000-000000000001"),
            "{error}"
        );

        assert!(workspace_is_busy(workspace.path(), &storage()));

        // Releasing the first lock lets the next run in.
        drop(first);
        assert!(!workspace_is_busy(workspace.path(), &storage()));
        let second = WorkspaceLock::acquire(workspace.path(), &storage(), &run(2)).unwrap();
        drop(second);
    }

    #[test]
    fn other_workspaces_are_independent() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let _a = WorkspaceLock::acquire(first.path(), &storage(), &run(1)).unwrap();
        let _b = WorkspaceLock::acquire(second.path(), &storage(), &run(2)).unwrap();
    }

    #[test]
    fn the_lock_lives_in_the_private_storage_directory() {
        use std::os::unix::fs::PermissionsExt;
        let workspace = tempfile::tempdir().unwrap();
        let _lock = WorkspaceLock::acquire(workspace.path(), &storage(), &run(1)).unwrap();
        let mode = |path: &str| {
            std::fs::metadata(workspace.path().join(path))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(".testry"), 0o700);
        assert_eq!(mode(".testry/locks"), 0o700);
        assert_eq!(mode(".testry/locks/workspace.lock"), 0o600);
        assert_eq!(
            std::fs::read_to_string(workspace.path().join(".testry/.gitignore")).unwrap(),
            "*\n"
        );
    }

    #[test]
    fn a_symbolic_link_in_place_of_the_storage_is_refused() {
        let workspace = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), workspace.path().join(".testry")).unwrap();
        let error = WorkspaceLock::acquire(workspace.path(), &storage(), &run(1)).unwrap_err();
        assert_eq!(error.code(), "storage");
        assert_eq!(std::fs::read_dir(elsewhere.path()).unwrap().count(), 0);
    }
}
