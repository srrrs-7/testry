//! A fixed view of the inputs a plan is computed from (spec section 6).
//!
//! The snapshot records *references* to file contents (digests and Git object
//! IDs), not the contents themselves. Its ID changes whenever a relevant file
//! is added, removed or modified, or when the baseline moves, which is how a
//! stale plan is detected before and after execution.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::fingerprint::{Fingerprint, FingerprintBuilder};
use crate::ids::{BaselineId, SourceSnapshotId, WorkspaceId};
use crate::paths::RepoPath;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    File,
    /// A symbolic link. It is never followed; its target text is fingerprinted.
    Symlink,
    /// A Git submodule entry. Its contents are neither fingerprinted nor
    /// analyzed in v0.1.
    Submodule,
}

/// Where a difference to the baseline was observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeSource {
    /// merge-base -> HEAD
    Committed,
    /// HEAD -> index
    Staged,
    /// index -> working tree
    Unstaged,
    /// Not tracked and not ignored.
    Untracked,
}

/// A file of the current working tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentFile {
    pub kind: FileKind,
    /// BLAKE3 of the working tree bytes (of the link target for symlinks).
    pub digest: Fingerprint,
}

/// A file of the baseline commit. Its content is read from the Git object
/// database, never from a checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineFile {
    pub kind: FileKind,
    /// Git object ID of the blob (or of the submodule commit).
    pub object_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotDiagnosticKind {
    /// A Git-ignored file matches a declared input. It is fingerprinted, but
    /// there is no baseline to compare it with, so it never counts as changed.
    IgnoredInputWithoutBaseline,
    /// Submodule contents are not fingerprinted or analyzed.
    SubmoduleNotAnalyzed,
}

impl SnapshotDiagnosticKind {
    pub fn code(self) -> &'static str {
        match self {
            Self::IgnoredInputWithoutBaseline => "ignored_input_without_baseline",
            Self::SubmoduleNotAnalyzed => "submodule_not_analyzed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SnapshotDiagnostic {
    pub kind: SnapshotDiagnosticKind,
    pub path: RepoPath,
}

/// Input for [`SourceSnapshot::new`].
#[derive(Debug, Clone, Default)]
pub struct SourceSnapshotParts {
    pub baseline: Option<BaselineId>,
    pub changes: BTreeMap<RepoPath, BTreeSet<ChangeSource>>,
    pub current: BTreeMap<RepoPath, CurrentFile>,
    pub baseline_files: BTreeMap<RepoPath, BaselineFile>,
    pub diagnostics: BTreeSet<SnapshotDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSnapshot {
    workspace_id: WorkspaceId,
    baseline: Option<BaselineId>,
    snapshot_id: SourceSnapshotId,
    changes: BTreeMap<RepoPath, BTreeSet<ChangeSource>>,
    current: BTreeMap<RepoPath, CurrentFile>,
    baseline_files: BTreeMap<RepoPath, BaselineFile>,
    diagnostics: BTreeSet<SnapshotDiagnostic>,
}

impl SourceSnapshot {
    pub fn new(workspace_id: WorkspaceId, parts: SourceSnapshotParts) -> Self {
        let snapshot_id = compute_id(&workspace_id, &parts);
        Self {
            workspace_id,
            baseline: parts.baseline,
            snapshot_id,
            changes: parts.changes,
            current: parts.current,
            baseline_files: parts.baseline_files,
            diagnostics: parts.diagnostics,
        }
    }

    pub fn workspace_id(&self) -> &WorkspaceId {
        &self.workspace_id
    }

    /// The resolved baseline commit. `None` when no diff was requested (full
    /// mode); never a mutable ref name.
    pub fn baseline(&self) -> Option<&BaselineId> {
        self.baseline.as_ref()
    }

    /// The input fingerprint.
    pub fn id(&self) -> &SourceSnapshotId {
        &self.snapshot_id
    }

    /// Every path that differs from the baseline, with where the difference
    /// was observed. A rename appears as the old and the new path.
    pub fn changes(&self) -> &BTreeMap<RepoPath, BTreeSet<ChangeSource>> {
        &self.changes
    }

    pub fn changed_paths(&self) -> impl Iterator<Item = &RepoPath> {
        self.changes.keys()
    }

    pub fn is_changed(&self, path: &RepoPath) -> bool {
        self.changes.contains_key(path)
    }

    /// Files of the working tree: tracked, untracked-but-not-ignored, and
    /// ignored files that match a declared input.
    pub fn current(&self) -> &BTreeMap<RepoPath, CurrentFile> {
        &self.current
    }

    /// Files of the baseline commit. Empty when there is no baseline.
    pub fn baseline_files(&self) -> &BTreeMap<RepoPath, BaselineFile> {
        &self.baseline_files
    }

    pub fn diagnostics(&self) -> &BTreeSet<SnapshotDiagnostic> {
        &self.diagnostics
    }
}

fn compute_id(workspace_id: &WorkspaceId, parts: &SourceSnapshotParts) -> SourceSnapshotId {
    let mut fingerprint = FingerprintBuilder::new("testry.source-snapshot.v1");
    fingerprint.text("workspace", workspace_id.as_str());
    match &parts.baseline {
        Some(baseline) => fingerprint.text("baseline", baseline.as_str()),
        None => fingerprint.text("no-baseline", ""),
    };
    for path in parts.changes.keys() {
        fingerprint.text("changed", path.as_str());
    }
    for (path, file) in &parts.current {
        fingerprint.text("path", path.as_str());
        fingerprint.text(
            "kind",
            match file.kind {
                FileKind::File => "file",
                FileKind::Symlink => "symlink",
                FileKind::Submodule => "submodule",
            },
        );
        fingerprint.fingerprint("digest", &file.digest);
    }
    SourceSnapshotId::new(fingerprint.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(raw: &str) -> RepoPath {
        RepoPath::try_new(raw).unwrap()
    }

    fn workspace() -> WorkspaceId {
        WorkspaceId::try_new("ws-test").unwrap()
    }

    fn file(content: &str) -> CurrentFile {
        CurrentFile {
            kind: FileKind::File,
            digest: Fingerprint::of_bytes(content.as_bytes()),
        }
    }

    fn parts(files: &[(&str, &str)]) -> SourceSnapshotParts {
        SourceSnapshotParts {
            baseline: Some(BaselineId::try_new("a".repeat(40)).unwrap()),
            current: files
                .iter()
                .map(|(name, content)| (path(name), file(content)))
                .collect(),
            ..SourceSnapshotParts::default()
        }
    }

    #[test]
    fn id_is_stable_for_identical_inputs() {
        let a = SourceSnapshot::new(workspace(), parts(&[("a.ts", "1"), ("b.ts", "2")]));
        let b = SourceSnapshot::new(workspace(), parts(&[("b.ts", "2"), ("a.ts", "1")]));
        assert_eq!(a.id(), b.id());
    }

    #[test]
    fn id_changes_with_content_additions_and_removals() {
        let base = SourceSnapshot::new(workspace(), parts(&[("a.ts", "1")]));
        let modified = SourceSnapshot::new(workspace(), parts(&[("a.ts", "2")]));
        let added = SourceSnapshot::new(workspace(), parts(&[("a.ts", "1"), ("b.ts", "")]));
        let removed = SourceSnapshot::new(workspace(), parts(&[]));
        assert_ne!(base.id(), modified.id());
        assert_ne!(base.id(), added.id());
        assert_ne!(base.id(), removed.id());
    }

    #[test]
    fn id_changes_with_baseline_and_change_set() {
        let base = SourceSnapshot::new(workspace(), parts(&[("a.ts", "1")]));

        let mut other_baseline = parts(&[("a.ts", "1")]);
        other_baseline.baseline = Some(BaselineId::try_new("b".repeat(40)).unwrap());
        assert_ne!(
            base.id(),
            SourceSnapshot::new(workspace(), other_baseline).id()
        );

        let mut no_baseline = parts(&[("a.ts", "1")]);
        no_baseline.baseline = None;
        assert_ne!(
            base.id(),
            SourceSnapshot::new(workspace(), no_baseline).id()
        );

        let mut changed = parts(&[("a.ts", "1")]);
        changed
            .changes
            .insert(path("a.ts"), BTreeSet::from([ChangeSource::Unstaged]));
        assert_ne!(base.id(), SourceSnapshot::new(workspace(), changed).id());
    }

    #[test]
    fn where_a_change_was_observed_does_not_change_the_id() {
        let mut staged = parts(&[("a.ts", "1")]);
        staged
            .changes
            .insert(path("a.ts"), BTreeSet::from([ChangeSource::Staged]));
        let mut unstaged = parts(&[("a.ts", "1")]);
        unstaged
            .changes
            .insert(path("a.ts"), BTreeSet::from([ChangeSource::Unstaged]));
        assert_eq!(
            SourceSnapshot::new(workspace(), staged).id(),
            SourceSnapshot::new(workspace(), unstaged).id()
        );
    }

    #[test]
    fn file_kind_is_part_of_the_id() {
        let regular = SourceSnapshot::new(workspace(), parts(&[("a", "x")]));
        let mut link = parts(&[("a", "x")]);
        link.current.insert(
            path("a"),
            CurrentFile {
                kind: FileKind::Symlink,
                digest: Fingerprint::of_bytes(b"x"),
            },
        );
        assert_ne!(regular.id(), SourceSnapshot::new(workspace(), link).id());
    }
}
