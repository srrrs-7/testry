//! Reads the file contents a snapshot refers to.
//!
//! Current files are read from the working tree and checked against the
//! digest recorded in the snapshot, so a file that changed after the snapshot
//! is reported as stale instead of being analyzed. Baseline files are read
//! from the Git object database; the baseline is never checked out and never
//! executed.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use testry_application::ports::{PortError, SourceUnavailableReason};
use testry_domain::fingerprint::Fingerprint;
use testry_domain::paths::RepoPath;

use super::git::unavailable;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceContent {
    Bytes(Vec<u8>),
    /// The content exceeds the requested limit and was not read.
    TooLarge {
        size: u64,
    },
}

/// Blocking content access. It is called from blocking worker threads only.
pub trait SourceReader: Send + Sync {
    fn read_current(
        &self,
        path: &RepoPath,
        expected: &Fingerprint,
        max_bytes: u64,
    ) -> Result<SourceContent, PortError>;

    fn read_baseline(
        &self,
        path: &RepoPath,
        object_id: &str,
        max_bytes: u64,
    ) -> Result<SourceContent, PortError>;
}

#[derive(Debug, Clone)]
pub struct GitWorkspaceReader {
    program: String,
    root: PathBuf,
}

impl GitWorkspaceReader {
    /// `root` is the workspace root, the top level of the Git work tree.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            program: "git".to_owned(),
            root: root.into(),
        }
    }

    fn cat_file(&self, arguments: &[&str]) -> Result<Vec<u8>, PortError> {
        let output = Command::new(&self.program)
            .arg("-C")
            .arg(&self.root)
            .arg("--no-pager")
            .arg("cat-file")
            .args(arguments)
            .stdin(Stdio::null())
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .output()
            .map_err(|error| {
                unavailable(
                    SourceUnavailableReason::GitUnavailable,
                    format!("could not start `{}`: {}", self.program, error.kind()),
                )
            })?;
        if !output.status.success() {
            return Err(unavailable(
                SourceUnavailableReason::GitFailed,
                "`git cat-file` could not read a baseline object",
            ));
        }
        Ok(output.stdout)
    }
}

fn stale(path: &RepoPath, what: &str) -> PortError {
    PortError::StaleInputs {
        message: format!("{path} {what} after the snapshot was taken"),
    }
}

impl SourceReader for GitWorkspaceReader {
    fn read_current(
        &self,
        path: &RepoPath,
        expected: &Fingerprint,
        max_bytes: u64,
    ) -> Result<SourceContent, PortError> {
        let full = self.root.join(path.as_str());
        // No symbolic link is followed: the snapshot lists links as links.
        let metadata = std::fs::symlink_metadata(&full).map_err(|_| stale(path, "disappeared"))?;
        if !metadata.is_file() {
            return Err(stale(path, "is no longer a regular file"));
        }
        if metadata.len() > max_bytes {
            return Ok(SourceContent::TooLarge {
                size: metadata.len(),
            });
        }
        let bytes = std::fs::read(&full).map_err(|_| stale(path, "became unreadable"))?;
        if &Fingerprint::of_bytes(&bytes) != expected {
            return Err(stale(path, "changed"));
        }
        Ok(SourceContent::Bytes(bytes))
    }

    fn read_baseline(
        &self,
        _path: &RepoPath,
        object_id: &str,
        max_bytes: u64,
    ) -> Result<SourceContent, PortError> {
        let is_object_id = matches!(object_id.len(), 40 | 64)
            && object_id.bytes().all(|byte| byte.is_ascii_hexdigit());
        if !is_object_id {
            return Err(unavailable(
                SourceUnavailableReason::GitFailed,
                "a baseline entry has no valid Git object id",
            ));
        }
        let size = self.cat_file(&["-s", object_id])?;
        let size: u64 = String::from_utf8_lossy(&size).trim().parse().map_err(|_| {
            unavailable(
                SourceUnavailableReason::GitFailed,
                "`git cat-file -s` printed no size",
            )
        })?;
        if size > max_bytes {
            return Ok(SourceContent::TooLarge { size });
        }
        Ok(SourceContent::Bytes(self.cat_file(&["blob", object_id])?))
    }
}

#[cfg(test)]
pub(crate) mod memory {
    //! An in-memory reader for analyzer tests.

    use std::collections::BTreeMap;

    use super::*;

    #[derive(Debug, Default, Clone)]
    pub(crate) struct MemoryReader {
        pub(crate) current: BTreeMap<RepoPath, Vec<u8>>,
        pub(crate) baseline: BTreeMap<String, Vec<u8>>,
    }

    impl SourceReader for MemoryReader {
        fn read_current(
            &self,
            path: &RepoPath,
            expected: &Fingerprint,
            max_bytes: u64,
        ) -> Result<SourceContent, PortError> {
            let bytes = self
                .current
                .get(path)
                .ok_or_else(|| stale(path, "disappeared"))?;
            if bytes.len() as u64 > max_bytes {
                return Ok(SourceContent::TooLarge {
                    size: bytes.len() as u64,
                });
            }
            if &Fingerprint::of_bytes(bytes) != expected {
                return Err(stale(path, "changed"));
            }
            Ok(SourceContent::Bytes(bytes.clone()))
        }

        fn read_baseline(
            &self,
            path: &RepoPath,
            object_id: &str,
            max_bytes: u64,
        ) -> Result<SourceContent, PortError> {
            let bytes = self.baseline.get(object_id).ok_or_else(|| {
                unavailable(
                    SourceUnavailableReason::GitFailed,
                    format!("no baseline object for {path}"),
                )
            })?;
            if bytes.len() as u64 > max_bytes {
                return Ok(SourceContent::TooLarge {
                    size: bytes.len() as u64,
                });
            }
            Ok(SourceContent::Bytes(bytes.clone()))
        }
    }
}
