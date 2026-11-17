//! What a suite runs (spec sections 4 and 8).
//!
//! `Selection` forms a small lattice: `Omit < Files(set) < All`. The only
//! combining operation is [`Selection::widen`], which can grow a selection but
//! never shrink it. An empty file list cannot be constructed, so it can never
//! reach a runner and turn into an accidental "run everything" or "run nothing".

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::paths::RepoPath;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SelectionError {
    #[error("a file selection must contain at least one test file")]
    EmptyFileSet,
    #[error("duplicate test file in selection: {0}")]
    DuplicateFile(RepoPath),
}

/// A non-empty, duplicate-free, ordered set of test files.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "Vec<RepoPath>", into = "Vec<RepoPath>")]
pub struct TestFileSet(BTreeSet<RepoPath>);

impl TestFileSet {
    pub fn try_new(files: impl IntoIterator<Item = RepoPath>) -> Result<Self, SelectionError> {
        let files: BTreeSet<RepoPath> = files.into_iter().collect();
        if files.is_empty() {
            return Err(SelectionError::EmptyFileSet);
        }
        Ok(Self(files))
    }

    pub fn iter(&self) -> impl Iterator<Item = &RepoPath> {
        self.0.iter()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Always `false`; provided because the set has a `len`.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn contains(&self, path: &RepoPath) -> bool {
        self.0.contains(path)
    }

    pub fn as_set(&self) -> &BTreeSet<RepoPath> {
        &self.0
    }

    pub fn union(&self, other: &Self) -> Self {
        Self(self.0.union(&other.0).cloned().collect())
    }

    pub fn is_superset(&self, other: &Self) -> bool {
        self.0.is_superset(&other.0)
    }
}

/// Deserialization is strict: stored selections never contain duplicates, so a
/// duplicate indicates a corrupted or hand-edited document.
impl TryFrom<Vec<RepoPath>> for TestFileSet {
    type Error = SelectionError;

    fn try_from(files: Vec<RepoPath>) -> Result<Self, Self::Error> {
        let mut set = BTreeSet::new();
        for file in files {
            if let Some(duplicate) = set.replace(file) {
                return Err(SelectionError::DuplicateFile(duplicate));
            }
        }
        if set.is_empty() {
            return Err(SelectionError::EmptyFileSet);
        }
        Ok(Self(set))
    }
}

impl From<TestFileSet> for Vec<RepoPath> {
    fn from(set: TestFileSet) -> Self {
        set.0.into_iter().collect()
    }
}

/// Why a suite is not executed. Omitting is never the same as passing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OmitReason {
    /// There is no change relative to the baseline.
    NoChanges,
    /// Every change matches the reviewed `non_test_inputs`.
    OnlyNonTestInputsChanged,
    /// Changes exist, all of them are covered by other tests, and none of
    /// them reaches a test of this suite.
    NoAffectedTests,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Selection {
    /// Run the whole suite.
    All,
    /// Run exactly these test files (every test inside them).
    Files { files: TestFileSet },
    /// Do not run the suite.
    Omit { reason: OmitReason },
}

impl Selection {
    pub fn files(files: impl IntoIterator<Item = RepoPath>) -> Result<Self, SelectionError> {
        Ok(Self::Files {
            files: TestFileSet::try_new(files)?,
        })
    }

    /// The least selection that covers both operands. Monotonic: the result
    /// covers `self` and `other`.
    pub fn widen(&self, other: &Self) -> Self {
        match (self, other) {
            (Self::All, _) | (_, Self::All) => Self::All,
            (Self::Files { files: left }, Self::Files { files: right }) => Self::Files {
                files: left.union(right),
            },
            (Self::Files { .. }, Self::Omit { .. }) | (Self::Omit { .. }, Self::Omit { .. }) => {
                self.clone()
            }
            (Self::Omit { .. }, Self::Files { .. }) => other.clone(),
        }
    }

    /// Whether everything `other` runs is also run by `self`.
    pub fn covers(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::All, _) | (_, Self::Omit { .. }) => true,
            (Self::Files { files: left }, Self::Files { files: right }) => left.is_superset(right),
            (Self::Files { .. } | Self::Omit { .. }, Self::All)
            | (Self::Omit { .. }, Self::Files { .. }) => false,
        }
    }

    pub fn runs_anything(&self) -> bool {
        !matches!(self, Self::Omit { .. })
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn path(raw: &str) -> RepoPath {
        RepoPath::try_new(raw).unwrap()
    }

    /// P01: an empty file set is rejected by the type.
    #[test]
    fn p01_empty_file_set_is_rejected() {
        assert_eq!(TestFileSet::try_new([]), Err(SelectionError::EmptyFileSet));
        assert_eq!(Selection::files([]), Err(SelectionError::EmptyFileSet));
    }

    /// P01: an empty or duplicated list is rejected when a document is read.
    #[test]
    fn p01_empty_file_list_is_rejected_on_deserialization() {
        let empty = r#"{"kind":"files","files":[]}"#;
        assert!(serde_json::from_str::<Selection>(empty).is_err());
        let duplicated = r#"{"kind":"files","files":["a.test.ts","./a.test.ts"]}"#;
        assert!(serde_json::from_str::<Selection>(duplicated).is_err());
        let missing = r#"{"kind":"files"}"#;
        assert!(serde_json::from_str::<Selection>(missing).is_err());
    }

    #[test]
    fn json_shape_matches_the_contract() {
        let files = Selection::files([path("b.test.ts"), path("a.test.ts")]).unwrap();
        assert_eq!(
            serde_json::to_string(&files).unwrap(),
            r#"{"kind":"files","files":["a.test.ts","b.test.ts"]}"#
        );
        assert_eq!(
            serde_json::to_string(&Selection::All).unwrap(),
            r#"{"kind":"all"}"#
        );
        let omit = Selection::Omit {
            reason: OmitReason::NoChanges,
        };
        assert_eq!(
            serde_json::to_string(&omit).unwrap(),
            r#"{"kind":"omit","reason":"no_changes"}"#
        );
        assert_eq!(
            serde_json::from_str::<Selection>(r#"{"kind":"all"}"#).unwrap(),
            Selection::All
        );
        assert!(serde_json::from_str::<Selection>(r#"{"kind":"some"}"#).is_err());
    }

    #[test]
    fn widen_follows_the_lattice() {
        let omit = Selection::Omit {
            reason: OmitReason::NoAffectedTests,
        };
        let a = Selection::files([path("a")]).unwrap();
        let b = Selection::files([path("b")]).unwrap();
        assert_eq!(omit.widen(&a), a);
        assert_eq!(a.widen(&omit), a);
        assert_eq!(
            a.widen(&b),
            Selection::files([path("a"), path("b")]).unwrap()
        );
        assert_eq!(a.widen(&Selection::All), Selection::All);
        assert_eq!(Selection::All.widen(&omit), Selection::All);
        assert_eq!(omit.widen(&omit), omit);
        assert!(!omit.runs_anything());
        assert!(a.runs_anything());
    }

    fn arbitrary_selection() -> impl Strategy<Value = Selection> {
        prop_oneof![
            Just(Selection::All),
            Just(Selection::Omit {
                reason: OmitReason::NoChanges
            }),
            Just(Selection::Omit {
                reason: OmitReason::NoAffectedTests
            }),
            proptest::collection::btree_set("[a-e]", 1..5).prop_map(|names| {
                Selection::files(names.iter().map(|name| path(name))).unwrap()
            }),
        ]
    }

    proptest! {
        /// Set monotonicity: widening never loses anything either side runs.
        #[test]
        fn widen_covers_both_operands(a in arbitrary_selection(), b in arbitrary_selection()) {
            let widened = a.widen(&b);
            prop_assert!(widened.covers(&a));
            prop_assert!(widened.covers(&b));
        }

        /// Order independence: the executed set does not depend on the order
        /// in which selections are combined.
        #[test]
        fn widen_is_commutative_and_associative_on_coverage(
            a in arbitrary_selection(),
            b in arbitrary_selection(),
            c in arbitrary_selection(),
        ) {
            let left = a.widen(&b).widen(&c);
            let right = a.widen(&b.widen(&c));
            prop_assert!(left.covers(&right) && right.covers(&left));
            let swapped = b.widen(&a);
            prop_assert!(a.widen(&b).covers(&swapped) && swapped.covers(&a.widen(&b)));
        }

        #[test]
        fn widen_is_idempotent(a in arbitrary_selection()) {
            prop_assert_eq!(a.widen(&a), a);
        }
    }
}
