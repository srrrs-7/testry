//! Which test files belong to which suite (spec section 5, `TestInventory`).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::fingerprint::{Fingerprint, FingerprintBuilder};
use crate::ids::{RunnerProject, SuiteId, TestFileId};
use crate::paths::RepoPath;

/// How an inventory was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InventoryOrigin {
    /// Files of the snapshot matched against the suite's `include` patterns.
    /// No runner configuration or test code was executed.
    StaticInclude,
}

/// What a static inventory could not confirm. These are carried into the plan
/// so that the execution preflight knows what still has to be checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InventoryCaveat {
    /// The runner's own discovery may find a different set of files.
    RunnerDiscoveryNotVerified,
    /// The configured runner project was not checked against the runner.
    RunnerProjectNotVerified,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuiteInventory {
    pub project: Option<RunnerProject>,
    pub files: BTreeSet<RepoPath>,
    pub origin: InventoryOrigin,
    pub unverified: BTreeSet<InventoryCaveat>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestInventory {
    suites: BTreeMap<SuiteId, SuiteInventory>,
    fingerprint: Fingerprint,
}

impl TestInventory {
    pub fn new(suites: BTreeMap<SuiteId, SuiteInventory>) -> Self {
        let mut fingerprint = FingerprintBuilder::new("testry.test-inventory.v1");
        for (suite, inventory) in &suites {
            fingerprint.text("suite", suite.as_str());
            match &inventory.project {
                Some(project) => fingerprint.text("project", project.as_str()),
                None => fingerprint.text("no-project", ""),
            };
            for file in &inventory.files {
                fingerprint.text("file", file.as_str());
            }
        }
        Self {
            suites,
            fingerprint: fingerprint.finish(),
        }
    }

    pub fn suites(&self) -> &BTreeMap<SuiteId, SuiteInventory> {
        &self.suites
    }

    pub fn suite(&self, id: &SuiteId) -> Option<&SuiteInventory> {
        self.suites.get(id)
    }

    /// The test files of a suite; empty for an unknown suite.
    pub fn files_of(&self, id: &SuiteId) -> impl Iterator<Item = &RepoPath> {
        self.suites
            .get(id)
            .into_iter()
            .flat_map(|inventory| inventory.files.iter())
    }

    pub fn fingerprint(&self) -> &Fingerprint {
        &self.fingerprint
    }

    /// Every test file with its full identity. The same path in two suites or
    /// projects yields two entries.
    pub fn test_file_ids(&self) -> BTreeSet<TestFileId> {
        self.suites
            .iter()
            .flat_map(|(suite, inventory)| {
                inventory.files.iter().map(move |path| TestFileId {
                    suite: suite.clone(),
                    project: inventory.project.clone(),
                    path: path.clone(),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(raw: &str) -> RepoPath {
        RepoPath::try_new(raw).unwrap()
    }

    fn inventory(project: Option<&str>, files: &[&str]) -> SuiteInventory {
        SuiteInventory {
            project: project.map(|name| RunnerProject::try_new(name).unwrap()),
            files: files.iter().map(|file| path(file)).collect(),
            origin: InventoryOrigin::StaticInclude,
            unverified: BTreeSet::from([InventoryCaveat::RunnerDiscoveryNotVerified]),
        }
    }

    fn suite(name: &str) -> SuiteId {
        SuiteId::try_new(name).unwrap()
    }

    /// P11: the same file in two projects stays two test files.
    #[test]
    fn p11_same_path_in_two_projects_does_not_collide() {
        let inventory = TestInventory::new(BTreeMap::from([
            (suite("unit"), inventory(Some("unit"), &["a.test.ts"])),
            (
                suite("storybook"),
                inventory(Some("storybook"), &["a.test.ts"]),
            ),
        ]));
        assert_eq!(inventory.test_file_ids().len(), 2);
    }

    #[test]
    fn fingerprint_tracks_files_and_projects() {
        let base = TestInventory::new(BTreeMap::from([(suite("s"), inventory(None, &["a"]))]));
        let more = TestInventory::new(BTreeMap::from([(suite("s"), inventory(None, &["a", "b"]))]));
        let project =
            TestInventory::new(BTreeMap::from([(suite("s"), inventory(Some("p"), &["a"]))]));
        assert_ne!(base.fingerprint(), more.fingerprint());
        assert_ne!(base.fingerprint(), project.fingerprint());
        let again = TestInventory::new(BTreeMap::from([(suite("s"), inventory(None, &["a"]))]));
        assert_eq!(base.fingerprint(), again.fingerprint());
    }

    #[test]
    fn unknown_suite_has_no_files() {
        let inventory = TestInventory::new(BTreeMap::new());
        assert_eq!(inventory.files_of(&suite("missing")).count(), 0);
        assert!(inventory.suite(&suite("missing")).is_none());
    }
}
