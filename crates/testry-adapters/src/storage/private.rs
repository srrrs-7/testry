//! Private files and directories below the storage directory.
//!
//! Directories are created with mode 0700 and files with mode 0600: logs,
//! reports and traces can contain secrets. A document is written to a
//! temporary file and renamed into place, so a reader never sees half of it.
//! Symbolic links are refused; nothing is read or written through one.

use std::fs::{DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;

use testry_application::ports::PortError;
use testry_domain::paths::RepoPath;

pub(crate) fn storage_error(action: &str, path: &Path, error: &std::io::Error) -> PortError {
    PortError::Storage {
        message: format!("cannot {action} {}: {}", path.display(), error.kind()),
    }
}

/// Creates a directory that only the owner can enter. An existing directory is
/// accepted; a symbolic link in its place is not.
pub(crate) fn ensure_private_directory(path: &Path) -> Result<(), PortError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(PortError::Storage {
            message: format!(
                "{} is a symbolic link; refusing to store state through it",
                path.display()
            ),
        }),
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(PortError::Storage {
            message: format!("{} exists and is not a directory", path.display()),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => DirBuilder::new()
            .mode(0o700)
            .create(path)
            .map_err(|error| storage_error("create", path, &error)),
        Err(error) => Err(storage_error("inspect", path, &error)),
    }
}

/// Creates the storage directory of a workspace and makes sure Git never
/// tracks it, whatever the repository ignores.
pub(crate) fn ensure_storage_directory(
    workspace_root: &Path,
    storage_directory: &RepoPath,
) -> Result<std::path::PathBuf, PortError> {
    let directory = workspace_root.join(storage_directory.as_str());
    ensure_private_directory(&directory)?;
    let ignore = directory.join(".gitignore");
    if std::fs::symlink_metadata(&ignore).is_err() {
        write_private_file(&ignore, b"*\n")?;
    }
    Ok(directory)
}

/// Writes `bytes` to `path` atomically with mode 0600.
pub(crate) fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), PortError> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("document");
    let temporary = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    // `create_new` fails if the temporary name exists, including as a link.
    let _ = std::fs::remove_file(&temporary);
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    result.map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        storage_error("write", path, &error)
    })
}
