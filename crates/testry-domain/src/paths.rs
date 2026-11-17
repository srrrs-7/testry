//! Repository-relative paths.
//!
//! Every path that crosses a testry boundary (Git output, configuration,
//! runner reports, JSON contracts) is converted into [`RepoPath`] first. The
//! type guarantees that the path is UTF-8, relative, free of NUL bytes and
//! cannot leave the repository through `..`.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Why a string was rejected as a repository-relative path.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RepoPathError {
    #[error("path is empty")]
    Empty,
    #[error("path contains a NUL byte")]
    ContainsNul,
    #[error("path is not valid UTF-8 (non-UTF-8 paths are not supported in v0.1)")]
    NotUtf8,
    #[error("absolute paths are not allowed: {0:?}")]
    Absolute(String),
    #[error("`..` segments are not allowed: {0:?}")]
    ParentSegment(String),
}

/// A normalized, repository-relative, `/`-separated UTF-8 file path.
///
/// Normalization removes empty and `.` segments (`./a//b/` becomes `a/b`).
/// Any `..` segment is rejected outright, even when it would not leave the
/// repository, so no caller has to reason about traversal. Characters such as
/// spaces, newlines and non-ASCII text are preserved byte for byte; no Unicode
/// normalization is applied.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RepoPath(String);

impl RepoPath {
    pub fn try_new(raw: impl AsRef<str>) -> Result<Self, RepoPathError> {
        let normalized = normalize(raw.as_ref())?;
        if normalized.is_empty() {
            return Err(RepoPathError::Empty);
        }
        Ok(Self(normalized))
    }

    /// Converts raw bytes (for example NUL-separated Git output). Non-UTF-8
    /// input is rejected instead of being converted lossily.
    pub fn try_from_bytes(raw: &[u8]) -> Result<Self, RepoPathError> {
        let text = std::str::from_utf8(raw).map_err(|_| RepoPathError::NotUtf8)?;
        Self::try_new(text)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// The last segment. Never empty.
    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    /// Text after the last `.` of the file name. Dot files such as `.env` have
    /// no extension.
    pub fn extension(&self) -> Option<&str> {
        let name = self.file_name();
        match name.rfind('.') {
            Some(0) | None => None,
            Some(index) => name.get(index + 1..).filter(|ext| !ext.is_empty()),
        }
    }

    /// The containing directory.
    pub fn parent(&self) -> RepoDir {
        match self.0.rfind('/') {
            Some(index) => RepoDir(self.0.get(..index).map(|dir| Self(dir.to_owned()))),
            None => RepoDir::root(),
        }
    }
}

impl fmt::Display for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RepoPath({:?})", self.0)
    }
}

impl TryFrom<String> for RepoPath {
    type Error = RepoPathError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<RepoPath> for String {
    fn from(value: RepoPath) -> Self {
        value.0
    }
}

/// A repository-relative directory. Unlike [`RepoPath`] it can denote the
/// repository root, written as `.`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RepoDir(Option<RepoPath>);

impl RepoDir {
    pub const fn root() -> Self {
        Self(None)
    }

    pub fn try_new(raw: impl AsRef<str>) -> Result<Self, RepoPathError> {
        let normalized = normalize(raw.as_ref())?;
        if normalized.is_empty() {
            Ok(Self::root())
        } else {
            Ok(Self(Some(RepoPath(normalized))))
        }
    }

    pub fn is_root(&self) -> bool {
        self.0.is_none()
    }

    /// `.` for the repository root.
    pub fn as_str(&self) -> &str {
        self.0.as_ref().map_or(".", RepoPath::as_str)
    }

    pub fn as_path(&self) -> Option<&RepoPath> {
        self.0.as_ref()
    }

    /// Appends a relative path. The result is validated like any other
    /// [`RepoPath`], so `..` cannot be used to leave this directory.
    pub fn join(&self, relative: impl AsRef<str>) -> Result<RepoPath, RepoPathError> {
        let relative = normalize(relative.as_ref())?;
        if relative.is_empty() {
            return Err(RepoPathError::Empty);
        }
        match &self.0 {
            Some(dir) => Ok(RepoPath(format!("{}/{relative}", dir.as_str()))),
            None => Ok(RepoPath(relative)),
        }
    }

    /// Whether `path` lies below this directory.
    pub fn contains(&self, path: &RepoPath) -> bool {
        match &self.0 {
            None => true,
            Some(dir) => path
                .as_str()
                .strip_prefix(dir.as_str())
                .is_some_and(|rest| rest.starts_with('/')),
        }
    }
}

impl fmt::Display for RepoDir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for RepoDir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RepoDir({:?})", self.as_str())
    }
}

impl TryFrom<String> for RepoDir {
    type Error = RepoPathError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl From<RepoDir> for String {
    fn from(value: RepoDir) -> Self {
        value.as_str().to_owned()
    }
}

fn normalize(raw: &str) -> Result<String, RepoPathError> {
    if raw.is_empty() {
        return Err(RepoPathError::Empty);
    }
    if raw.contains('\0') {
        return Err(RepoPathError::ContainsNul);
    }
    if raw.starts_with('/') {
        return Err(RepoPathError::Absolute(raw.to_owned()));
    }
    let mut normalized = String::with_capacity(raw.len());
    for segment in raw.split('/') {
        match segment {
            "" | "." => {}
            ".." => return Err(RepoPathError::ParentSegment(raw.to_owned())),
            other => {
                if !normalized.is_empty() {
                    normalized.push('/');
                }
                normalized.push_str(other);
            }
        }
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn normalizes_redundant_segments() {
        assert_eq!(
            RepoPath::try_new("./a//b/./c.ts").unwrap().as_str(),
            "a/b/c.ts"
        );
        assert_eq!(RepoPath::try_new("a/b/").unwrap().as_str(), "a/b");
    }

    #[test]
    fn rejects_path_traversal() {
        for raw in ["..", "../a", "a/../b", "a/..", "a/b/../../../etc/passwd"] {
            assert!(
                matches!(RepoPath::try_new(raw), Err(RepoPathError::ParentSegment(_))),
                "{raw} must be rejected"
            );
        }
    }

    #[test]
    fn rejects_absolute_nul_and_empty() {
        assert!(matches!(
            RepoPath::try_new("/etc/passwd"),
            Err(RepoPathError::Absolute(_))
        ));
        assert_eq!(RepoPath::try_new("a\0b"), Err(RepoPathError::ContainsNul));
        assert_eq!(RepoPath::try_new(""), Err(RepoPathError::Empty));
        assert_eq!(RepoPath::try_new("."), Err(RepoPathError::Empty));
        assert_eq!(RepoPath::try_new("./"), Err(RepoPathError::Empty));
    }

    #[test]
    fn rejects_non_utf8_without_lossy_conversion() {
        assert_eq!(
            RepoPath::try_from_bytes(b"src/\xff\xfe.ts"),
            Err(RepoPathError::NotUtf8)
        );
    }

    #[test]
    fn keeps_spaces_newlines_and_japanese() {
        let raw = "apps/日本語 の/改行\nあり.test.ts";
        assert_eq!(RepoPath::try_new(raw).unwrap().as_str(), raw);
    }

    #[test]
    fn a_dot_dot_prefix_inside_a_name_is_not_traversal() {
        assert_eq!(
            RepoPath::try_new("a/..b/c...d").unwrap().as_str(),
            "a/..b/c...d"
        );
    }

    #[test]
    fn file_name_extension_and_parent() {
        let path = RepoPath::try_new("apps/api/user.test.ts").unwrap();
        assert_eq!(path.file_name(), "user.test.ts");
        assert_eq!(path.extension(), Some("ts"));
        assert_eq!(path.parent().as_str(), "apps/api");
        let top = RepoPath::try_new(".env").unwrap();
        assert_eq!(top.extension(), None);
        assert!(top.parent().is_root());
        assert_eq!(RepoPath::try_new("Makefile").unwrap().extension(), None);
        assert_eq!(RepoPath::try_new("a/b.").unwrap().extension(), None);
    }

    #[test]
    fn serde_rejects_invalid_paths() {
        assert!(serde_json::from_str::<RepoPath>("\"../x\"").is_err());
        assert!(serde_json::from_str::<RepoPath>("\"/x\"").is_err());
        let path: RepoPath = serde_json::from_str("\"./a/b\"").unwrap();
        assert_eq!(serde_json::to_string(&path).unwrap(), "\"a/b\"");
    }

    #[test]
    fn repo_dir_root_and_join() {
        let root = RepoDir::try_new(".").unwrap();
        assert!(root.is_root());
        assert_eq!(root.as_str(), ".");
        assert_eq!(root.join("a/b.ts").unwrap().as_str(), "a/b.ts");
        let dir = RepoDir::try_new("apps/api/").unwrap();
        assert_eq!(dir.join("src/x.ts").unwrap().as_str(), "apps/api/src/x.ts");
        assert!(matches!(
            dir.join("../web/x.ts"),
            Err(RepoPathError::ParentSegment(_))
        ));
        assert!(matches!(
            RepoDir::try_new("/abs"),
            Err(RepoPathError::Absolute(_))
        ));
        assert_eq!(RepoDir::try_new(""), Err(RepoPathError::Empty));
    }

    #[test]
    fn repo_dir_contains_respects_segment_boundaries() {
        let dir = RepoDir::try_new("apps/api").unwrap();
        assert!(dir.contains(&RepoPath::try_new("apps/api/x.ts").unwrap()));
        assert!(!dir.contains(&RepoPath::try_new("apps/api-v2/x.ts").unwrap()));
        assert!(!dir.contains(&RepoPath::try_new("apps/api").unwrap()));
        assert!(RepoDir::root().contains(&RepoPath::try_new("x.ts").unwrap()));
    }

    proptest! {
        /// Path normalization is idempotent and never yields an escaping path.
        #[test]
        fn normalization_is_idempotent_and_safe(raw in "[a-z./ あ\n-]{0,24}") {
            if let Ok(path) = RepoPath::try_new(&raw) {
                let again = RepoPath::try_new(path.as_str()).unwrap();
                prop_assert_eq!(&again, &path);
                prop_assert!(!path.as_str().starts_with('/'));
                prop_assert!(!path.as_str().ends_with('/'));
                prop_assert!(path.segments().all(|s| !s.is_empty() && s != "." && s != ".."));
            }
        }

        /// A path that contains a `..` segment is always rejected.
        #[test]
        fn parent_segments_never_pass(prefix in "[a-z/]{0,8}", suffix in "[a-z/]{0,8}") {
            let raw = format!("{prefix}/../{suffix}");
            prop_assert!(RepoPath::try_new(&raw).is_err());
        }
    }
}
