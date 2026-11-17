//! The `affected` selection rules (spec section 8).
//!
//! This is a pure function of the changed paths, the dependency graphs, the
//! test inventory and the configured patterns. It only narrows when the graph
//! positively connects every change to tests; in every other situation it
//! widens:
//!
//! 1. no change, or only reviewed `non_test_inputs` changed: nothing runs
//!    except `always_run` suites;
//! 2. a `global_inputs` path changed, the graph has any diagnostic, or a
//!    changed path is not reachable from any test: every suite in scope runs
//!    in full;
//! 3. otherwise `whole_suite` and `always_run` suites run in full and
//!    `file_graph` suites run the tests that depend on a change; a changed
//!    file inside a suite's declared `inputs` that the graph cannot attribute
//!    to that suite's tests widens the suite to full.
//!
//! Declared `inputs` are only ever used to widen. They never justify omitting
//! a suite.

use std::collections::{BTreeMap, BTreeSet};

use crate::graph::{DependencyGraph, dependencies_of, dependents_of};
use crate::ids::SuiteId;
use crate::inventory::TestInventory;
use crate::paths::RepoPath;
use crate::pattern::PatternSet;
use crate::plan::{FallbackKind, FallbackReason, SelectionReason};
use crate::selection::{OmitReason, Selection, TestFileSet};
use crate::suite::{SelectionStrategy, SuiteDefinition};

/// Extensions of files whose imports are analyzed.
pub const MODULE_EXTENSIONS: [&str; 8] = ["ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts"];

/// Capability name reported when a `file_graph` suite contains test files
/// that are not TypeScript or JavaScript modules.
pub const NON_MODULE_TEST_FILES: &str = "test_files_without_import_analysis";

/// Whether the dependency analysis parses this file for imports.
pub fn is_module_path(path: &RepoPath) -> bool {
    path.extension()
        .is_some_and(|extension| MODULE_EXTENSIONS.contains(&extension))
}

pub struct SelectionInput<'a> {
    /// Paths that differ from the baseline. Excluded runtime outputs are
    /// already removed.
    pub changed: &'a BTreeSet<RepoPath>,
    /// The baseline and the current graph. They are searched as one union.
    pub graphs: &'a [&'a DependencyGraph],
    /// Test files of every configured suite, including suites out of scope.
    pub inventory: &'a TestInventory,
    /// The suites a selection is computed for.
    pub suites: &'a [&'a SuiteDefinition],
    pub global_inputs: &'a PatternSet,
    pub non_test_inputs: &'a PatternSet,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuiteSelection {
    pub suite_id: SuiteId,
    pub selection: Selection,
    pub reasons: BTreeSet<SelectionReason>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AffectedSelection {
    /// One entry per suite of the input, in input order.
    pub suites: Vec<SuiteSelection>,
    /// Non-empty when every suite was widened to full.
    pub fallbacks: BTreeSet<FallbackReason>,
}

pub fn select_affected(input: &SelectionInput<'_>) -> AffectedSelection {
    let mut fallbacks: BTreeSet<FallbackReason> = BTreeSet::new();
    let mut code_changes: BTreeSet<&RepoPath> = BTreeSet::new();
    for path in input.changed {
        // A path listed as both global and non-test input is global.
        if let Some(pattern) = input.global_inputs.first_match(path) {
            fallbacks.insert(FallbackReason::GlobalInputChanged {
                path: path.clone(),
                pattern: pattern.to_owned(),
            });
        } else if !input.non_test_inputs.is_match(path) {
            code_changes.insert(path);
        }
    }

    if fallbacks.is_empty() && code_changes.is_empty() {
        let reason = if input.changed.is_empty() {
            OmitReason::NoChanges
        } else {
            OmitReason::OnlyNonTestInputsChanged
        };
        return AffectedSelection {
            suites: input
                .suites
                .iter()
                .map(|suite| without_relevant_change(suite, reason))
                .collect(),
            fallbacks,
        };
    }

    let diagnostics: BTreeSet<_> = input
        .graphs
        .iter()
        .flat_map(|graph| graph.diagnostics().iter())
        .collect();
    if !diagnostics.is_empty() {
        fallbacks.insert(FallbackReason::UnknownDependency {
            diagnostics: u64::try_from(diagnostics.len()).unwrap_or(u64::MAX),
        });
    }

    // What each suite's tests depend on, directly or transitively.
    let closures: BTreeMap<&SuiteId, BTreeSet<RepoPath>> = input
        .inventory
        .suites()
        .iter()
        .map(|(suite, inventory)| (suite, dependencies_of(input.graphs, inventory.files.iter())))
        .collect();

    // A change nothing depends on is not assumed to be irrelevant.
    for path in &code_changes {
        if !closures.values().any(|closure| closure.contains(*path)) {
            fallbacks.insert(FallbackReason::NoRelatedTests {
                path: (*path).clone(),
            });
        }
    }

    if !fallbacks.is_empty() {
        let kinds: BTreeSet<FallbackKind> = fallbacks.iter().map(FallbackReason::kind).collect();
        let suites = input
            .suites
            .iter()
            .map(|suite| widened_by_fallback(suite, &kinds))
            .collect();
        return AffectedSelection { suites, fallbacks };
    }

    let dependents = dependents_of(input.graphs, code_changes.iter().copied());
    let suites = input
        .suites
        .iter()
        .map(|suite| {
            if suite.always_run() {
                return whole(suite, SelectionReason::AlwaysRun);
            }
            if suite.selection() == SelectionStrategy::WholeSuite {
                return whole(suite, SelectionReason::WholeSuitePolicy);
            }

            let mut reasons = BTreeSet::new();
            let mut files = BTreeSet::new();
            for test in input.inventory.files_of(suite.id()) {
                let Some(route) = dependents.path_to_origin(test) else {
                    continue;
                };
                files.insert(test.clone());
                reasons.insert(if route.len() == 1 {
                    SelectionReason::DirectChange { path: test.clone() }
                } else {
                    SelectionReason::TransitiveDependency {
                        dependency_path: route,
                    }
                });
            }

            let closure = closures.get(suite.id());
            let mut widen = false;

            // The graph only knows the imports of files it parses. A test
            // file of another kind has unknown dependencies, so the suite
            // cannot be narrowed.
            if input
                .inventory
                .files_of(suite.id())
                .any(|test| !is_module_path(test))
            {
                widen = true;
                reasons.insert(SelectionReason::UnsupportedCapability {
                    capability: NON_MODULE_TEST_FILES.to_owned(),
                });
            }

            for path in &code_changes {
                let Some(pattern) = suite.inputs().first_match(path) else {
                    continue;
                };
                let reached = closure.is_some_and(|closure| closure.contains(*path));
                if !reached || !is_module_path(path) {
                    widen = true;
                    reasons.insert(SelectionReason::DeclaredInputChanged {
                        path: (*path).clone(),
                        pattern: pattern.to_owned(),
                    });
                }
            }

            let selection = if widen {
                Selection::All
            } else {
                match TestFileSet::try_new(files) {
                    Ok(files) => Selection::Files { files },
                    Err(_) => {
                        reasons.insert(SelectionReason::NoAffectedTests);
                        Selection::Omit {
                            reason: OmitReason::NoAffectedTests,
                        }
                    }
                }
            };
            SuiteSelection {
                suite_id: suite.id().clone(),
                selection,
                reasons,
            }
        })
        .collect();
    AffectedSelection { suites, fallbacks }
}

fn whole(suite: &SuiteDefinition, reason: SelectionReason) -> SuiteSelection {
    SuiteSelection {
        suite_id: suite.id().clone(),
        selection: Selection::All,
        reasons: BTreeSet::from([reason]),
    }
}

fn without_relevant_change(suite: &SuiteDefinition, reason: OmitReason) -> SuiteSelection {
    if suite.always_run() {
        return whole(suite, SelectionReason::AlwaysRun);
    }
    let explanation = match reason {
        OmitReason::NoChanges => SelectionReason::NoChanges,
        OmitReason::OnlyNonTestInputsChanged => SelectionReason::OnlyNonTestInputsChanged,
        OmitReason::NoAffectedTests => SelectionReason::NoAffectedTests,
    };
    SuiteSelection {
        suite_id: suite.id().clone(),
        selection: Selection::Omit { reason },
        reasons: BTreeSet::from([explanation]),
    }
}

fn widened_by_fallback(suite: &SuiteDefinition, kinds: &BTreeSet<FallbackKind>) -> SuiteSelection {
    let mut reasons: BTreeSet<SelectionReason> = kinds
        .iter()
        .map(|kind| SelectionReason::ExpandedByFallback { fallback: *kind })
        .collect();
    if suite.always_run() {
        reasons.insert(SelectionReason::AlwaysRun);
    }
    SuiteSelection {
        suite_id: suite.id().clone(),
        selection: Selection::All,
        reasons,
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::graph::{GraphDiagnostic, GraphDiagnosticKind};
    use crate::inventory::{InventoryOrigin, SuiteInventory};
    use crate::suite::RunnerKind;
    use crate::suite::test_support::parts;

    fn path(raw: &str) -> RepoPath {
        RepoPath::try_new(raw).unwrap()
    }

    fn paths(raw: &[&str]) -> BTreeSet<RepoPath> {
        raw.iter().map(|item| path(item)).collect()
    }

    fn graph(edges: &[(&str, &str)]) -> DependencyGraph {
        let mut builder = DependencyGraph::builder();
        for (consumer, dependency) in edges {
            builder.edge(path(consumer), path(dependency));
        }
        builder.build()
    }

    fn suite(id: &str, runner: RunnerKind, inputs: &[&str]) -> SuiteDefinition {
        let mut parts = parts(id, runner);
        parts.inputs = PatternSet::try_new(inputs).unwrap();
        SuiteDefinition::try_new(parts).unwrap()
    }

    fn inventory(suites: &[(&str, &[&str])]) -> TestInventory {
        TestInventory::new(
            suites
                .iter()
                .map(|(id, files)| {
                    (
                        SuiteId::try_new(*id).unwrap(),
                        SuiteInventory {
                            project: None,
                            files: paths(files),
                            origin: InventoryOrigin::StaticInclude,
                            unverified: BTreeSet::new(),
                        },
                    )
                })
                .collect(),
        )
    }

    struct Fixture {
        graphs: Vec<DependencyGraph>,
        inventory: TestInventory,
        suites: Vec<SuiteDefinition>,
        global_inputs: PatternSet,
        non_test_inputs: PatternSet,
    }

    impl Fixture {
        fn new(edges: &[(&str, &str)]) -> Self {
            Self {
                graphs: vec![graph(edges)],
                inventory: inventory(&[
                    ("api", &["api/a.test.ts", "api/b.test.ts"]),
                    ("web", &["web/w.test.ts"]),
                ]),
                suites: vec![
                    suite("api", RunnerKind::Bun, &[]),
                    suite("web", RunnerKind::Vitest, &[]),
                ],
                global_inputs: PatternSet::try_new(["**/bun.lock", "**/package.json"]).unwrap(),
                non_test_inputs: PatternSet::empty(),
            }
        }

        fn select(&self, changed: &[&str]) -> AffectedSelection {
            let changed = paths(changed);
            let graphs: Vec<&DependencyGraph> = self.graphs.iter().collect();
            let suites: Vec<&SuiteDefinition> = self.suites.iter().collect();
            select_affected(&SelectionInput {
                changed: &changed,
                graphs: &graphs,
                inventory: &self.inventory,
                suites: &suites,
                global_inputs: &self.global_inputs,
                non_test_inputs: &self.non_test_inputs,
            })
        }
    }

    fn selection_of<'a>(result: &'a AffectedSelection, suite: &str) -> &'a SuiteSelection {
        result
            .suites
            .iter()
            .find(|s| s.suite_id.as_str() == suite)
            .unwrap()
    }

    fn files(names: &[&str]) -> Selection {
        Selection::files(names.iter().map(|name| path(name))).unwrap()
    }

    const BASIC: &[(&str, &str)] = &[
        ("api/a.test.ts", "api/a.ts"),
        ("api/b.test.ts", "api/b.ts"),
        ("api/a.ts", "shared/util.ts"),
        ("web/w.test.ts", "shared/util.ts"),
    ];

    /// G01: a test importing A is selected when only A changes, with the path.
    #[test]
    fn g01_direct_dependency_is_selected_and_explained() {
        let result = Fixture::new(BASIC).select(&["api/b.ts"]);
        assert!(result.fallbacks.is_empty());
        let api = selection_of(&result, "api");
        assert_eq!(api.selection, files(&["api/b.test.ts"]));
        assert!(
            api.reasons
                .contains(&SelectionReason::TransitiveDependency {
                    dependency_path: vec![path("api/b.test.ts"), path("api/b.ts")],
                })
        );
        assert_eq!(
            selection_of(&result, "web").selection,
            Selection::Omit {
                reason: OmitReason::NoAffectedTests
            }
        );
    }

    /// G02: test -> A -> B, B changes.
    #[test]
    fn g02_transitive_dependency_is_selected() {
        let result = Fixture::new(BASIC).select(&["shared/util.ts"]);
        let api = selection_of(&result, "api");
        assert_eq!(api.selection, files(&["api/a.test.ts"]));
        assert!(
            api.reasons
                .contains(&SelectionReason::TransitiveDependency {
                    dependency_path: vec![
                        path("api/a.test.ts"),
                        path("api/a.ts"),
                        path("shared/util.ts")
                    ],
                })
        );
        assert_eq!(
            selection_of(&result, "web").selection,
            files(&["web/w.test.ts"])
        );
    }

    #[test]
    fn changed_test_file_is_a_direct_change() {
        let result = Fixture::new(BASIC).select(&["api/a.test.ts"]);
        let api = selection_of(&result, "api");
        assert_eq!(api.selection, files(&["api/a.test.ts"]));
        assert!(api.reasons.contains(&SelectionReason::DirectChange {
            path: path("api/a.test.ts")
        }));
    }

    /// G08: consumers of a deleted file are found through the baseline graph.
    #[test]
    fn g08_deleted_dependency_keeps_its_consumers() {
        let mut fixture =
            Fixture::new(&[("api/a.test.ts", "api/a.ts"), ("api/b.test.ts", "api/b.ts")]);
        fixture.graphs.insert(
            0,
            graph(&[
                ("api/a.test.ts", "api/a.ts"),
                ("api/a.test.ts", "api/removed.ts"),
                ("api/b.test.ts", "api/b.ts"),
            ]),
        );
        let result = fixture.select(&["api/removed.ts", "api/a.test.ts"]);
        assert!(result.fallbacks.is_empty());
        assert_eq!(
            selection_of(&result, "api").selection,
            files(&["api/a.test.ts"])
        );
    }

    /// G12 / P04: a global input widens everything, even when it is also
    /// listed as a non-test input.
    #[test]
    fn g12_p04_global_input_widens_all_suites_and_wins_over_non_test_inputs() {
        let mut fixture = Fixture::new(BASIC);
        fixture.non_test_inputs = PatternSet::try_new(["**/bun.lock", "docs/**"]).unwrap();
        let result = fixture.select(&["bun.lock", "docs/readme.md"]);
        assert_eq!(
            result.fallbacks,
            BTreeSet::from([FallbackReason::GlobalInputChanged {
                path: path("bun.lock"),
                pattern: "**/bun.lock".to_owned(),
            }])
        );
        for suite in &result.suites {
            assert_eq!(suite.selection, Selection::All);
            assert!(
                suite
                    .reasons
                    .contains(&SelectionReason::ExpandedByFallback {
                        fallback: FallbackKind::GlobalInputChanged
                    })
            );
        }
    }

    /// G06 / G07: any graph diagnostic widens every suite in scope.
    #[test]
    fn g06_g07_graph_uncertainty_widens_all_suites() {
        for kind in [
            GraphDiagnosticKind::DynamicDependency,
            GraphDiagnosticKind::UnresolvedImport,
            GraphDiagnosticKind::ParseFailure,
        ] {
            let mut fixture = Fixture::new(BASIC);
            let mut builder = DependencyGraph::builder();
            for (consumer, dependency) in BASIC {
                builder.edge(path(consumer), path(dependency));
            }
            builder.diagnostic(GraphDiagnostic {
                kind,
                file: path("web/lazy.ts"),
                detail: "x".to_owned(),
            });
            fixture.graphs = vec![builder.build()];
            let result = fixture.select(&["api/b.ts"]);
            assert_eq!(
                result.fallbacks,
                BTreeSet::from([FallbackReason::UnknownDependency { diagnostics: 1 }])
            );
            assert!(
                result
                    .suites
                    .iter()
                    .all(|suite| suite.selection == Selection::All),
                "{kind:?}"
            );
        }
    }

    /// P02: a code change no test depends on widens everything.
    #[test]
    fn p02_change_without_related_tests_widens_all_suites() {
        let result = Fixture::new(BASIC).select(&["api/orphan.ts", "api/b.ts"]);
        assert_eq!(
            result.fallbacks,
            BTreeSet::from([FallbackReason::NoRelatedTests {
                path: path("api/orphan.ts")
            }])
        );
        assert!(
            result
                .suites
                .iter()
                .all(|suite| suite.selection == Selection::All)
        );
    }

    /// P03: no change means no work, not a pass.
    #[test]
    fn p03_no_changes_selects_nothing() {
        let result = Fixture::new(BASIC).select(&[]);
        assert!(result.fallbacks.is_empty());
        for suite in &result.suites {
            assert_eq!(
                suite.selection,
                Selection::Omit {
                    reason: OmitReason::NoChanges
                }
            );
        }
    }

    #[test]
    fn only_reviewed_non_test_inputs_changed_selects_nothing_but_always_run() {
        let mut fixture = Fixture::new(BASIC);
        fixture.non_test_inputs = PatternSet::try_new(["docs/**"]).unwrap();
        let mut gate = parts("typecheck", RunnerKind::Command);
        gate.always_run = true;
        fixture.suites.push(SuiteDefinition::try_new(gate).unwrap());
        let result = fixture.select(&["docs/guide.md"]);
        assert!(result.fallbacks.is_empty());
        assert_eq!(
            selection_of(&result, "api").selection,
            Selection::Omit {
                reason: OmitReason::OnlyNonTestInputsChanged
            }
        );
        let gate = selection_of(&result, "typecheck");
        assert_eq!(gate.selection, Selection::All);
        assert!(gate.reasons.contains(&SelectionReason::AlwaysRun));
    }

    #[test]
    fn markdown_is_not_irrelevant_by_default() {
        let result = Fixture::new(BASIC).select(&["README.md"]);
        assert!(result.fallbacks.contains(&FallbackReason::NoRelatedTests {
            path: path("README.md")
        }));
    }

    #[test]
    fn whole_suite_and_always_run_suites_run_in_full_on_a_relevant_change() {
        let mut fixture = Fixture::new(BASIC);
        fixture
            .suites
            .push(suite("e2e", RunnerKind::Playwright, &[]));
        let mut gate = parts("typecheck", RunnerKind::Command);
        gate.always_run = true;
        fixture.suites.push(SuiteDefinition::try_new(gate).unwrap());
        let result = fixture.select(&["api/b.ts"]);
        assert!(result.fallbacks.is_empty());
        let e2e = selection_of(&result, "e2e");
        assert_eq!(e2e.selection, Selection::All);
        assert!(e2e.reasons.contains(&SelectionReason::WholeSuitePolicy));
        assert_eq!(selection_of(&result, "typecheck").selection, Selection::All);
        assert_eq!(
            selection_of(&result, "api").selection,
            files(&["api/b.test.ts"])
        );
    }

    #[test]
    fn a_change_covered_only_by_a_whole_suite_does_not_force_a_fallback() {
        let mut fixture = Fixture::new(&[
            ("api/a.test.ts", "api/a.ts"),
            ("e2e/login.spec.ts", "e2e/helpers.ts"),
        ]);
        fixture.inventory =
            inventory(&[("api", &["api/a.test.ts"]), ("e2e", &["e2e/login.spec.ts"])]);
        fixture.suites = vec![
            suite("api", RunnerKind::Bun, &[]),
            suite("e2e", RunnerKind::Playwright, &[]),
        ];
        let result = fixture.select(&["e2e/helpers.ts"]);
        assert!(result.fallbacks.is_empty());
        assert_eq!(selection_of(&result, "e2e").selection, Selection::All);
        assert_eq!(
            selection_of(&result, "api").selection,
            Selection::Omit {
                reason: OmitReason::NoAffectedTests
            }
        );
    }

    #[test]
    fn declared_input_the_graph_cannot_attribute_widens_that_suite() {
        let mut fixture = Fixture::new(BASIC);
        fixture.suites = vec![
            suite("api", RunnerKind::Bun, &["api/**", "shared/**"]),
            suite("web", RunnerKind::Vitest, &["web/**"]),
        ];
        // shared/web-only.ts is used by web tests only, but api declares shared/** as input.
        fixture.graphs = vec![graph(&[
            ("api/a.test.ts", "api/a.ts"),
            ("api/b.test.ts", "api/b.ts"),
            ("web/w.test.ts", "shared/web-only.ts"),
        ])];
        let result = fixture.select(&["shared/web-only.ts"]);
        assert!(result.fallbacks.is_empty());
        let api = selection_of(&result, "api");
        assert_eq!(api.selection, Selection::All);
        assert!(
            api.reasons
                .contains(&SelectionReason::DeclaredInputChanged {
                    path: path("shared/web-only.ts"),
                    pattern: "shared/**".to_owned(),
                })
        );
        assert_eq!(
            selection_of(&result, "web").selection,
            files(&["web/w.test.ts"])
        );
    }

    #[test]
    fn declared_non_module_input_widens_even_when_one_test_imports_it() {
        let mut fixture = Fixture::new(&[
            ("api/a.test.ts", "api/data.json"),
            ("api/b.test.ts", "api/b.ts"),
            ("web/w.test.ts", "web/w.ts"),
        ]);
        fixture.suites = vec![
            suite("api", RunnerKind::Bun, &["api/**"]),
            suite("web", RunnerKind::Vitest, &["web/**"]),
        ];
        let result = fixture.select(&["api/data.json"]);
        assert_eq!(selection_of(&result, "api").selection, Selection::All);

        // Without the declaration the graph edge alone decides.
        fixture.suites = vec![
            suite("api", RunnerKind::Bun, &[]),
            suite("web", RunnerKind::Vitest, &[]),
        ];
        let result = fixture.select(&["api/data.json"]);
        assert_eq!(
            selection_of(&result, "api").selection,
            files(&["api/a.test.ts"])
        );
    }

    #[test]
    fn module_inside_declared_inputs_reached_by_tests_stays_narrow() {
        let mut fixture = Fixture::new(BASIC);
        fixture.suites = vec![
            suite("api", RunnerKind::Bun, &["api/**"]),
            suite("web", RunnerKind::Vitest, &["web/**"]),
        ];
        let result = fixture.select(&["api/b.ts"]);
        assert_eq!(
            selection_of(&result, "api").selection,
            files(&["api/b.test.ts"])
        );
    }

    #[test]
    fn a_suite_with_unparsed_test_files_is_never_narrowed() {
        let mut fixture = Fixture::new(&[
            ("api/a.test.ts", "api/steps.ts"),
            ("web/w.test.ts", "web/w.ts"),
        ]);
        // `api/login.feature` may use api/steps.ts, but nothing can tell.
        fixture.inventory = inventory(&[
            ("api", &["api/a.test.ts", "api/login.feature"]),
            ("web", &["web/w.test.ts"]),
        ]);
        let result = fixture.select(&["api/steps.ts"]);
        assert!(result.fallbacks.is_empty());
        let api = selection_of(&result, "api");
        assert_eq!(api.selection, Selection::All);
        assert!(
            api.reasons
                .contains(&SelectionReason::UnsupportedCapability {
                    capability: NON_MODULE_TEST_FILES.to_owned(),
                })
        );
        assert_eq!(
            selection_of(&result, "web").selection,
            Selection::Omit {
                reason: OmitReason::NoAffectedTests
            }
        );
    }

    #[test]
    fn module_extensions() {
        for name in [
            "a.ts",
            "a.tsx",
            "a.js",
            "a.jsx",
            "a.mjs",
            "a.cjs",
            "a.mts",
            "a.cts",
            "x/a.test.ts",
        ] {
            assert!(is_module_path(&path(name)), "{name}");
        }
        for name in ["a.json", "a.css", "README.md", ".env", "a.d", "Makefile"] {
            assert!(!is_module_path(&path(name)), "{name}");
        }
    }

    fn node(index: u8) -> String {
        format!("n{index}.ts")
    }

    fn build_graph(edges: &[(u8, u8)], diagnostics: usize) -> DependencyGraph {
        let mut builder = DependencyGraph::builder();
        for (consumer, dependency) in edges {
            builder.edge(path(&node(*consumer)), path(&node(*dependency)));
        }
        for index in 0..diagnostics {
            builder.diagnostic(GraphDiagnostic {
                kind: GraphDiagnosticKind::UnresolvedImport,
                file: path("n0.ts"),
                detail: format!("spec{index}"),
            });
        }
        builder.build()
    }

    fn select_generated(
        graph: &DependencyGraph,
        changed: &BTreeSet<RepoPath>,
    ) -> AffectedSelection {
        let inventory = inventory(&[
            ("api", &["n0.ts", "n1.ts", "n2.ts"]),
            ("web", &["n3.ts", "n4.ts"]),
        ]);
        let suites = [
            suite("api", RunnerKind::Bun, &[]),
            suite("web", RunnerKind::Vitest, &[]),
        ];
        let suites: Vec<&SuiteDefinition> = suites.iter().collect();
        select_affected(&SelectionInput {
            changed,
            graphs: &[graph],
            inventory: &inventory,
            suites: &suites,
            global_inputs: &PatternSet::empty(),
            non_test_inputs: &PatternSet::empty(),
        })
    }

    fn covers(wider: &AffectedSelection, narrower: &AffectedSelection) -> bool {
        wider
            .suites
            .iter()
            .zip(&narrower.suites)
            .all(|(wide, narrow)| wide.selection.covers(&narrow.selection))
    }

    proptest! {
        /// P16: adding unknown dependencies never shrinks a selection.
        #[test]
        fn p16_adding_unknown_dependencies_never_shrinks(
            edges in proptest::collection::vec((0u8..10, 0u8..10), 0..30),
            changed in proptest::collection::btree_set(0u8..10, 0..4),
            extra_diagnostics in 1usize..4,
        ) {
            let changed: BTreeSet<RepoPath> = changed.iter().map(|index| path(&node(*index))).collect();
            let before = select_generated(&build_graph(&edges, 0), &changed);
            let after = select_generated(&build_graph(&edges, extra_diagnostics), &changed);
            prop_assert!(covers(&after, &before));
        }

        /// P16: adding edges never shrinks a selection that was derived from
        /// the graph. (A selection that is already a "no related tests"
        /// fallback is full and can only become explained, see the acceptance
        /// mapping.)
        #[test]
        fn p16_adding_edges_never_shrinks_a_graph_derived_selection(
            edges in proptest::collection::vec((0u8..10, 0u8..10), 0..30),
            extra in proptest::collection::vec((0u8..10, 0u8..10), 0..10),
            changed in proptest::collection::btree_set(0u8..10, 0..4),
        ) {
            let changed: BTreeSet<RepoPath> = changed.iter().map(|index| path(&node(*index))).collect();
            let before = select_generated(&build_graph(&edges, 0), &changed);
            prop_assume!(before.fallbacks.is_empty());
            let mut all = edges.clone();
            all.extend(extra);
            let after = select_generated(&build_graph(&all, 0), &changed);
            prop_assert!(covers(&after, &before));
        }

        /// Order independence: the selection is a function of the sets, not of
        /// the order in which edges were recorded.
        #[test]
        fn selection_is_independent_of_edge_order(
            edges in proptest::collection::vec((0u8..10, 0u8..10), 0..30),
            changed in proptest::collection::btree_set(0u8..10, 0..4),
        ) {
            let changed: BTreeSet<RepoPath> = changed.iter().map(|index| path(&node(*index))).collect();
            let reversed: Vec<(u8, u8)> = edges.iter().rev().copied().collect();
            prop_assert_eq!(
                select_generated(&build_graph(&edges, 0), &changed),
                select_generated(&build_graph(&reversed, 0), &changed)
            );
        }

        /// Every selected test really depends on a changed file, and every
        /// test that depends on a changed file is selected.
        #[test]
        fn selection_is_exactly_the_dependents_when_no_fallback_applies(
            edges in proptest::collection::vec((0u8..10, 0u8..10), 0..30),
            changed in proptest::collection::btree_set(0u8..10, 1..4),
        ) {
            let changed: BTreeSet<RepoPath> = changed.iter().map(|index| path(&node(*index))).collect();
            let graph = build_graph(&edges, 0);
            let result = select_generated(&graph, &changed);
            prop_assume!(result.fallbacks.is_empty());
            let tests = [("api", vec![0u8, 1, 2]), ("web", vec![3u8, 4])];
            for (suite, members) in tests {
                let expected: BTreeSet<RepoPath> = members
                    .iter()
                    .map(|index| path(&node(*index)))
                    .filter(|test| !dependencies_of(&[&graph], [test]).is_disjoint(&changed))
                    .collect();
                let actual: BTreeSet<RepoPath> = match &selection_of(&result, suite).selection {
                    Selection::Files { files } => files.as_set().clone(),
                    Selection::Omit { .. } => BTreeSet::new(),
                    Selection::All => unreachable!("no fallback and no declared inputs"),
                };
                prop_assert_eq!(actual, expected);
            }
        }
    }
}
