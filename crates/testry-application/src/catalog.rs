//! Static test inventory (spec section 5, `TestCatalog`).

use std::collections::{BTreeMap, BTreeSet};

use testry_domain::inventory::{InventoryCaveat, InventoryOrigin, SuiteInventory, TestInventory};
use testry_domain::snapshot::{FileKind, SourceSnapshot};
use testry_domain::suite::SuiteDefinition;

/// Matches the current files of the snapshot against each suite's `include`
/// patterns. Nothing is executed. New untracked test files are part of the
/// snapshot and therefore part of the inventory.
///
/// Symbolic links are not listed: they are never followed, so their content is
/// unknown. What the runner itself would discover is recorded as unverified.
pub fn collect_static_inventory(
    snapshot: &SourceSnapshot,
    suites: &[SuiteDefinition],
) -> TestInventory {
    let inventories = suites
        .iter()
        .map(|suite| {
            let files: BTreeSet<_> = snapshot
                .current()
                .iter()
                .filter(|(path, file)| {
                    file.kind == FileKind::File && suite.include().is_match(path)
                })
                .map(|(path, _)| path.clone())
                .collect();
            let mut unverified = BTreeSet::from([InventoryCaveat::RunnerDiscoveryNotVerified]);
            if suite.project().is_some() {
                unverified.insert(InventoryCaveat::RunnerProjectNotVerified);
            }
            let inventory = SuiteInventory {
                project: suite.project().cloned(),
                files,
                origin: InventoryOrigin::StaticInclude,
                unverified,
            };
            (suite.id().clone(), inventory)
        })
        .collect::<BTreeMap<_, _>>();
    TestInventory::new(inventories)
}

#[cfg(test)]
mod tests {
    use testry_domain::fingerprint::Fingerprint;
    use testry_domain::ids::{SuiteId, WorkspaceId};
    use testry_domain::paths::RepoPath;
    use testry_domain::snapshot::{CurrentFile, SourceSnapshotParts};
    use testry_domain::suite::RunnerKind;

    use super::*;
    use crate::settings::test_support::suite;

    fn snapshot(files: &[(&str, FileKind)]) -> SourceSnapshot {
        let current = files
            .iter()
            .map(|(name, kind)| {
                let file = CurrentFile {
                    kind: *kind,
                    digest: Fingerprint::of_bytes(name.as_bytes()),
                };
                (RepoPath::try_new(name).unwrap(), file)
            })
            .collect();
        SourceSnapshot::new(
            WorkspaceId::try_new("ws").unwrap(),
            SourceSnapshotParts {
                current,
                ..SourceSnapshotParts::default()
            },
        )
    }

    fn files_of(inventory: &TestInventory, suite: &str) -> Vec<String> {
        inventory
            .files_of(&SuiteId::try_new(suite).unwrap())
            .map(|path| path.as_str().to_owned())
            .collect()
    }

    #[test]
    fn matches_include_patterns_per_suite() {
        let snapshot = snapshot(&[
            ("apps/api/user.test.ts", FileKind::File),
            ("apps/api/user.ts", FileKind::File),
            ("apps/web/App.test.tsx", FileKind::File),
            ("apps/web/App.test.ts", FileKind::File),
        ]);
        let suites = [
            suite("api", RunnerKind::Bun, &["apps/api/**/*.test.ts"]),
            suite("web", RunnerKind::Vitest, &["apps/web/**/*.test.tsx"]),
            suite("typecheck", RunnerKind::Command, &[]),
        ];
        let inventory = collect_static_inventory(&snapshot, &suites);
        assert_eq!(files_of(&inventory, "api"), ["apps/api/user.test.ts"]);
        assert_eq!(files_of(&inventory, "web"), ["apps/web/App.test.tsx"]);
        assert!(files_of(&inventory, "typecheck").is_empty());
    }

    #[test]
    fn symlinks_and_submodules_are_not_test_files() {
        let snapshot = snapshot(&[
            ("a.test.ts", FileKind::File),
            ("link.test.ts", FileKind::Symlink),
            ("vendor.test.ts", FileKind::Submodule),
        ]);
        let inventory =
            collect_static_inventory(&snapshot, &[suite("api", RunnerKind::Bun, &["*.test.ts"])]);
        assert_eq!(files_of(&inventory, "api"), ["a.test.ts"]);
    }

    #[test]
    fn records_what_static_collection_cannot_verify() {
        let snapshot = snapshot(&[("a.test.ts", FileKind::File)]);
        let suites = [
            suite("api", RunnerKind::Bun, &["*.test.ts"]),
            suite("web", RunnerKind::Vitest, &["*.test.ts"]),
        ];
        let inventory = collect_static_inventory(&snapshot, &suites);
        let api = inventory.suite(&SuiteId::try_new("api").unwrap()).unwrap();
        assert_eq!(
            api.unverified,
            BTreeSet::from([InventoryCaveat::RunnerDiscoveryNotVerified])
        );
        let web = inventory.suite(&SuiteId::try_new("web").unwrap()).unwrap();
        assert!(
            web.unverified
                .contains(&InventoryCaveat::RunnerProjectNotVerified)
        );
        assert!(web.project.is_some());
    }
}
