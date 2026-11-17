//! Workspace trust (spec section 17).
//!
//! testry runs the commands a repository's configuration names. Before the
//! first command of a workspace runs, someone has to approve exactly that
//! configuration. The approval is stored in the user's state directory,
//! outside the repository, keyed by the workspace path and the configuration
//! digest. A repository can therefore not approve itself, and a changed
//! configuration needs a new approval.
//!
//! State directory: `$TESTRY_STATE_HOME`, else `$XDG_STATE_HOME/testry`, else
//! `$HOME/.local/state/testry`.
//!
//! CI has no interactive user. There the job definition, which is outside the
//! change under test, pins the digest in `TESTRY_TRUSTED_CONFIG_DIGEST`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use testry_application::ports::PortError;
use testry_application::ports::execution::TrustApproval;
use testry_domain::fingerprint::Fingerprint;

use super::private::{ensure_private_directory, storage_error, write_private_file};

pub const STATE_HOME_VARIABLE: &str = "TESTRY_STATE_HOME";
pub const TRUSTED_DIGEST_VARIABLE: &str = "TESTRY_TRUSTED_CONFIG_DIGEST";

const TRUST_FILE: &str = "trust.json";
const MAX_TRUST_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Default, Serialize, Deserialize)]
struct TrustDocument {
    schema_version: u32,
    /// Canonical workspace path -> approval.
    workspaces: BTreeMap<String, TrustEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TrustEntry {
    config_digest: Fingerprint,
}

#[derive(Debug, Clone)]
pub struct TrustStore {
    directory: PathBuf,
}

fn trust_error(message: impl Into<String>) -> PortError {
    PortError::TrustRequired {
        message: message.into(),
    }
}

impl TrustStore {
    /// Finds the state directory from environment variables. `variable`
    /// looks one up; it is a parameter so tests need not touch the process
    /// environment.
    pub fn locate(variable: impl Fn(&str) -> Option<String>) -> Result<Self, PortError> {
        let non_empty = |name: &str| variable(name).filter(|value| !value.is_empty());
        let directory = if let Some(state_home) = non_empty(STATE_HOME_VARIABLE) {
            PathBuf::from(state_home)
        } else if let Some(xdg) = non_empty("XDG_STATE_HOME") {
            PathBuf::from(xdg).join("testry")
        } else if let Some(home) = non_empty("HOME") {
            PathBuf::from(home)
                .join(".local")
                .join("state")
                .join("testry")
        } else {
            return Err(trust_error(format!(
                "no state directory is available to look up workspace trust; set {STATE_HOME_VARIABLE}"
            )));
        };
        if !directory.is_absolute() {
            return Err(trust_error("the state directory must be an absolute path"));
        }
        Ok(Self { directory })
    }

    pub fn at(directory: PathBuf) -> Self {
        Self { directory }
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    fn workspace_key(&self, workspace_root: &Path) -> Result<String, PortError> {
        let canonical = std::fs::canonicalize(workspace_root)
            .map_err(|error| storage_error("resolve", workspace_root, &error))?;
        // Trust kept inside the repository could be committed or rewritten by
        // the very change that should be reviewed.
        let state =
            std::fs::canonicalize(&self.directory).unwrap_or_else(|_| self.directory.clone());
        if state.starts_with(&canonical) {
            return Err(trust_error(format!(
                "the state directory {} is inside the workspace; trust must be stored outside the repository",
                self.directory.display()
            )));
        }
        canonical
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| PortError::Storage {
                message: "the workspace path is not valid UTF-8".to_owned(),
            })
    }

    fn load(&self) -> Result<TrustDocument, PortError> {
        let path = self.directory.join(TRUST_FILE);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(TrustDocument {
                    schema_version: 1,
                    workspaces: BTreeMap::new(),
                });
            }
            Err(error) => return Err(storage_error("inspect", &path, &error)),
        };
        if !metadata.is_file() || metadata.len() > MAX_TRUST_BYTES {
            return Err(PortError::Storage {
                message: format!("{} is not a usable trust file", path.display()),
            });
        }
        let bytes = std::fs::read(&path).map_err(|error| storage_error("read", &path, &error))?;
        let document: TrustDocument =
            serde_json::from_slice(&bytes).map_err(|error| PortError::Storage {
                message: format!("{} is not a valid trust file: {error}", path.display()),
            })?;
        if document.schema_version != 1 {
            return Err(PortError::Storage {
                message: format!(
                    "{} has schema version {}, which this build does not read",
                    path.display(),
                    document.schema_version
                ),
            });
        }
        Ok(document)
    }

    fn save(&self, document: &TrustDocument) -> Result<(), PortError> {
        // Parent directories are the user's; only the testry directory itself
        // is created with a private mode.
        if let Some(parent) = self.directory.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| storage_error("create", parent, &error))?;
        }
        ensure_private_directory(&self.directory)?;
        let mut text =
            serde_json::to_string_pretty(document).map_err(|error| PortError::Storage {
                message: format!("the trust file could not be serialized: {error}"),
            })?;
        text.push('\n');
        write_private_file(&self.directory.join(TRUST_FILE), text.as_bytes())
    }

    /// The stored approval of a workspace, if any.
    pub fn approval(&self, workspace_root: &Path) -> Result<Option<TrustApproval>, PortError> {
        let key = self.workspace_key(workspace_root)?;
        Ok(self
            .load()?
            .workspaces
            .get(&key)
            .map(|entry| TrustApproval {
                approved_config_digest: entry.config_digest,
                allow_initialize_command: false,
            }))
    }

    /// Records that the user approved this configuration for this workspace.
    pub fn approve(
        &self,
        workspace_root: &Path,
        config_digest: &Fingerprint,
    ) -> Result<(), PortError> {
        let key = self.workspace_key(workspace_root)?;
        let mut document = self.load()?;
        document.schema_version = 1;
        document.workspaces.insert(
            key,
            TrustEntry {
                config_digest: *config_digest,
            },
        );
        self.save(&document)
    }

    /// Removes the approval of a workspace. Returns whether there was one.
    pub fn revoke(&self, workspace_root: &Path) -> Result<bool, PortError> {
        let key = self.workspace_key(workspace_root)?;
        let mut document = self.load()?;
        let existed = document.workspaces.remove(&key).is_some();
        if existed {
            self.save(&document)?;
        }
        Ok(existed)
    }
}

/// The approval a trusted job definition pinned through the environment.
/// A value that is not a digest is an error, not "no approval".
pub fn approval_from_environment(value: Option<&str>) -> Result<Option<TrustApproval>, PortError> {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let digest = Fingerprint::parse(value).map_err(|error| {
        trust_error(format!(
            "{TRUSTED_DIGEST_VARIABLE} is not a configuration digest: {error}"
        ))
    })?;
    Ok(Some(TrustApproval {
        approved_config_digest: digest,
        allow_initialize_command: false,
    }))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn digest(text: &str) -> Fingerprint {
        Fingerprint::of_bytes(text.as_bytes())
    }

    fn environment<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    #[test]
    fn approvals_are_per_workspace_and_per_configuration() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let store = TrustStore::at(state.path().join("testry"));

        assert_eq!(store.approval(workspace.path()).unwrap(), None);
        store
            .approve(workspace.path(), &digest("config v1"))
            .unwrap();
        let approval = store.approval(workspace.path()).unwrap().unwrap();
        assert_eq!(approval.approved_config_digest, digest("config v1"));
        assert!(!approval.allow_initialize_command);
        assert_eq!(
            store.approval(other.path()).unwrap(),
            None,
            "trust does not carry to another workspace"
        );

        // Approving a new configuration replaces the old approval.
        store
            .approve(workspace.path(), &digest("config v2"))
            .unwrap();
        assert_eq!(
            store
                .approval(workspace.path())
                .unwrap()
                .unwrap()
                .approved_config_digest,
            digest("config v2")
        );

        assert!(store.revoke(workspace.path()).unwrap());
        assert!(!store.revoke(workspace.path()).unwrap());
        assert_eq!(store.approval(workspace.path()).unwrap(), None);
    }

    #[test]
    fn the_trust_file_is_private() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = TrustStore::at(state.path().join("nested").join("testry"));
        store.approve(workspace.path(), &digest("c")).unwrap();
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(store.directory()), 0o700);
        assert_eq!(mode(&store.directory().join("trust.json")), 0o600);
    }

    /// Spec section 17: a repository cannot approve itself.
    #[test]
    fn trust_cannot_be_stored_inside_the_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let store = TrustStore::at(workspace.path().join(".testry").join("state"));
        let error = store.approve(workspace.path(), &digest("c")).unwrap_err();
        assert_eq!(error.code(), "trust_required");
        assert!(!workspace.path().join(".testry").exists());
        assert_eq!(
            store.approval(workspace.path()).unwrap_err().code(),
            "trust_required"
        );
    }

    #[test]
    fn the_state_directory_follows_the_usual_variables() {
        let explicit = TrustStore::locate(environment(&[
            ("TESTRY_STATE_HOME", "/state/explicit"),
            ("XDG_STATE_HOME", "/xdg"),
            ("HOME", "/home/dev"),
        ]))
        .unwrap();
        assert_eq!(explicit.directory(), Path::new("/state/explicit"));

        let xdg = TrustStore::locate(environment(&[
            ("XDG_STATE_HOME", "/xdg"),
            ("HOME", "/home/dev"),
        ]))
        .unwrap();
        assert_eq!(xdg.directory(), Path::new("/xdg/testry"));

        let home = TrustStore::locate(environment(&[("HOME", "/home/dev")])).unwrap();
        assert_eq!(home.directory(), Path::new("/home/dev/.local/state/testry"));

        assert_eq!(
            TrustStore::locate(environment(&[])).unwrap_err().code(),
            "trust_required"
        );
        assert_eq!(
            TrustStore::locate(environment(&[("TESTRY_STATE_HOME", "relative/dir")]))
                .unwrap_err()
                .code(),
            "trust_required"
        );
        // Empty values are treated as unset.
        let fallback = TrustStore::locate(environment(&[
            ("TESTRY_STATE_HOME", ""),
            ("HOME", "/home/dev"),
        ]))
        .unwrap();
        assert_eq!(
            fallback.directory(),
            Path::new("/home/dev/.local/state/testry")
        );
    }

    #[test]
    fn a_damaged_trust_file_is_an_error_not_an_approval() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = TrustStore::at(state.path().to_path_buf());
        for content in ["{", "[]", r#"{"schema_version":2,"workspaces":{}}"#] {
            std::fs::write(state.path().join("trust.json"), content).unwrap();
            assert_eq!(
                store.approval(workspace.path()).unwrap_err().code(),
                "storage",
                "{content}"
            );
        }
    }

    #[test]
    fn a_pinned_digest_from_the_environment_is_validated() {
        let pinned = digest("config").to_string();
        let approval = approval_from_environment(Some(&pinned)).unwrap().unwrap();
        assert_eq!(approval.approved_config_digest, digest("config"));
        assert_eq!(approval_from_environment(None).unwrap(), None);
        assert_eq!(approval_from_environment(Some("")).unwrap(), None);
        for invalid in ["1", "true", "yes", "blake3:abc"] {
            assert_eq!(
                approval_from_environment(Some(invalid)).unwrap_err().code(),
                "trust_required",
                "{invalid}"
            );
        }
    }
}
