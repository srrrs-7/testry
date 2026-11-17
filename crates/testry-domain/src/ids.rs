//! Identifier value objects (spec section 4).
//!
//! Each identifier validates its format on construction. Identifiers that end
//! up in artifact paths or container labels use a restricted character set so
//! they can never contain path separators or shell metacharacters.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::fingerprint::Fingerprint;
use crate::paths::RepoPath;

/// A rejected identifier. `value` is truncated so that oversized input does not
/// flood logs.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid {kind} {value:?}: {reason}")]
pub struct IdError {
    pub kind: &'static str,
    pub value: String,
    pub reason: &'static str,
}

const MAX_REPORTED_VALUE_CHARS: usize = 80;

fn id_error(kind: &'static str, raw: &str, reason: &'static str) -> IdError {
    IdError {
        kind,
        value: raw.chars().take(MAX_REPORTED_VALUE_CHARS).collect(),
        reason,
    }
}

macro_rules! validated_string_id {
    ($(#[$meta:meta])* $name:ident, $kind:literal, $validate:path) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn try_new(raw: impl Into<String>) -> Result<Self, IdError> {
                let raw = raw.into();
                match $validate(&raw) {
                    Ok(()) => Ok(Self(raw)),
                    Err(reason) => Err(id_error($kind, &raw, reason)),
                }
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::try_new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({:?})"), self.0)
            }
        }
    };
}

validated_string_id!(
    /// Normalized repository identifier. Distinct Git worktrees get distinct IDs.
    WorkspaceId,
    "workspace id",
    validate_workspace_id
);

validated_string_id!(
    /// A resolved Git commit ID (SHA-1 or SHA-256). Mutable ref names such as
    /// `origin/main` are never stored as a baseline.
    BaselineId,
    "baseline commit",
    validate_commit_id
);

validated_string_id!(
    /// Suite identifier, unique within the configuration. It is used as an
    /// artifact directory name, hence the restricted character set.
    SuiteId,
    "suite id",
    validate_slug
);

validated_string_id!(
    /// Name of a runner-level project (Vitest project, Playwright project).
    RunnerProject,
    "runner project",
    validate_project
);

validated_string_id!(
    /// Unique per execution. Never derived from a plan.
    RunId,
    "run id",
    validate_uuid
);

validated_string_id!(
    /// Execution unit, unique within a run. In v0.1 one task is one suite batch.
    TaskId,
    "task id",
    validate_slug
);

validated_string_id!(
    /// Identifier of an exclusively leased execution environment. It is
    /// deliberately not a container ID.
    EnvironmentLeaseId,
    "environment lease id",
    validate_uuid
);

validated_string_id!(
    /// Key of a resource that only one task may hold at a time.
    ExclusiveKey,
    "exclusive key",
    validate_slug
);

validated_string_id!(
    /// Name of an execution environment in the configuration.
    EnvironmentName,
    "environment name",
    validate_slug
);

impl TaskId {
    /// v0.1 runs one task per suite, so the task ID equals the suite ID.
    pub fn for_suite(suite: &SuiteId) -> Self {
        Self(suite.as_str().to_owned())
    }
}

fn validate_slug(raw: &str) -> Result<(), &'static str> {
    const RULE: &str = "must be 1-64 characters of [A-Za-z0-9._-] and start with a letter or digit";
    let mut bytes = raw.bytes();
    match bytes.next() {
        Some(first) if first.is_ascii_alphanumeric() => {}
        _ => return Err(RULE),
    }
    if raw.len() > 64
        || !bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(RULE);
    }
    Ok(())
}

fn validate_workspace_id(raw: &str) -> Result<(), &'static str> {
    const RULE: &str = "must be 1-64 characters of [a-z0-9-] and start with a letter or digit";
    let mut bytes = raw.bytes();
    match bytes.next() {
        Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit() => {}
        _ => return Err(RULE),
    }
    if raw.len() > 64 || !bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
        return Err(RULE);
    }
    Ok(())
}

fn validate_commit_id(raw: &str) -> Result<(), &'static str> {
    let is_hex = raw.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if is_hex && matches!(raw.len(), 40 | 64) {
        Ok(())
    } else {
        Err("must be a full 40 or 64 character lowercase hexadecimal commit id")
    }
}

fn validate_project(raw: &str) -> Result<(), &'static str> {
    if raw.is_empty() {
        return Err("must not be empty");
    }
    if raw.chars().count() > 128 {
        return Err("must be at most 128 characters");
    }
    if raw.chars().any(char::is_control) {
        return Err("must not contain control characters");
    }
    Ok(())
}

fn validate_uuid(raw: &str) -> Result<(), &'static str> {
    const RULE: &str = "must be a canonical lowercase UUID";
    if raw.len() != 36 {
        return Err(RULE);
    }
    for (index, byte) in raw.bytes().enumerate() {
        let valid = match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => matches!(byte, b'0'..=b'9' | b'a'..=b'f'),
        };
        if !valid {
            return Err(RULE);
        }
    }
    Ok(())
}

macro_rules! fingerprint_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Fingerprint);

        impl $name {
            pub fn new(fingerprint: Fingerprint) -> Self {
                Self(fingerprint)
            }

            pub fn fingerprint(&self) -> &Fingerprint {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.0)
            }
        }
    };
}

fingerprint_id!(
    /// Fingerprint of every input a plan depends on: relevant file contents,
    /// additions, deletions and configuration.
    SourceSnapshotId
);

fingerprint_id!(
    /// Derived from the canonical form of a plan. Never mixes in run IDs or
    /// timestamps.
    PlanId
);

/// Identity of a test file. The same path in two suites or two runner
/// projects is two different test files.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TestFileId {
    pub suite: SuiteId,
    pub project: Option<RunnerProject>,
    pub path: RepoPath,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suite_id_rejects_empty_and_unsafe_values() {
        assert!(SuiteId::try_new("").is_err());
        assert!(SuiteId::try_new("../x").is_err());
        assert!(SuiteId::try_new("a/b").is_err());
        assert!(SuiteId::try_new(".hidden").is_err());
        assert!(SuiteId::try_new("-flag").is_err());
        assert!(SuiteId::try_new("a b").is_err());
        assert!(SuiteId::try_new("a".repeat(65)).is_err());
        assert_eq!(SuiteId::try_new("api-unit").unwrap().as_str(), "api-unit");
        assert!(SuiteId::try_new("web_unit.v2").is_ok());
    }

    #[test]
    fn baseline_must_be_a_full_commit_id() {
        assert!(BaselineId::try_new("a".repeat(40)).is_ok());
        assert!(BaselineId::try_new("0".repeat(64)).is_ok());
        assert!(BaselineId::try_new("origin/main").is_err());
        assert!(BaselineId::try_new("HEAD").is_err());
        assert!(BaselineId::try_new("abc123").is_err());
        assert!(BaselineId::try_new("A".repeat(40)).is_err());
    }

    #[test]
    fn run_id_must_be_a_canonical_uuid() {
        assert!(RunId::try_new("123e4567-e89b-42d3-a456-426614174000").is_ok());
        assert!(RunId::try_new("123E4567-E89B-42D3-A456-426614174000").is_err());
        assert!(RunId::try_new("123e4567e89b42d3a456426614174000").is_err());
        assert!(RunId::try_new("").is_err());
    }

    #[test]
    fn workspace_id_is_lowercase_slug() {
        assert!(WorkspaceId::try_new("workspace-example").is_ok());
        assert!(WorkspaceId::try_new("Workspace").is_err());
        assert!(WorkspaceId::try_new("").is_err());
    }

    #[test]
    fn task_id_equals_suite_id_in_v0_1() {
        let suite = SuiteId::try_new("api-unit").unwrap();
        assert_eq!(TaskId::for_suite(&suite).as_str(), "api-unit");
    }

    #[test]
    fn test_file_ids_distinguish_suite_and_project() {
        let path = RepoPath::try_new("a.test.ts").unwrap();
        let unit = TestFileId {
            suite: SuiteId::try_new("web").unwrap(),
            project: Some(RunnerProject::try_new("unit").unwrap()),
            path: path.clone(),
        };
        let storybook = TestFileId {
            suite: SuiteId::try_new("web").unwrap(),
            project: Some(RunnerProject::try_new("storybook").unwrap()),
            path,
        };
        assert_ne!(unit, storybook);
    }

    #[test]
    fn serde_validates_identifiers() {
        assert!(serde_json::from_str::<SuiteId>("\"\"").is_err());
        assert!(serde_json::from_str::<RunId>("\"not-a-uuid\"").is_err());
        let error = SuiteId::try_new("x".repeat(500)).unwrap_err();
        assert!(error.value.len() <= MAX_REPORTED_VALUE_CHARS);
    }
}
