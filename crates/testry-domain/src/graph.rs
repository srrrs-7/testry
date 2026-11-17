//! File-level dependency graph (spec section 7).
//!
//! Edges always point `consumer -> dependency`:
//!
//! ```text
//! user.test.ts -> user.ts
//! user.ts      -> validation.ts
//! ```
//!
//! A change to `validation.ts` is followed backwards to `user.test.ts`.
//! Traversals accept several graphs at once so the baseline and the current
//! graph can be searched as one union; that keeps the consumers of deleted and
//! renamed files.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::fingerprint::{Fingerprint, FingerprintBuilder};
use crate::paths::RepoPath;

/// Something the analysis could not establish. Every kind is an uncertainty:
/// it is never turned into "no dependency".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphDiagnosticKind {
    /// A specifier that could not be resolved (unknown alias, missing file,
    /// virtual module).
    UnresolvedImport,
    /// `import(expr)` / `require(expr)` with a non-literal argument.
    DynamicDependency,
    /// The file could not be parsed, so its imports are unknown.
    ParseFailure,
    /// A construct whose dependencies v0.1 does not model (for example
    /// `import.meta.glob`).
    UnsupportedMechanism,
    /// Resolution left the repository.
    OutsideRepository,
    /// Resolution touched a path that is not analyzed (symlink, submodule).
    UnanalyzedPath,
    /// An analysis resource limit was hit (for example file size).
    ResourceLimit,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct GraphDiagnostic {
    pub kind: GraphDiagnosticKind,
    /// The file in which the problem was found.
    pub file: RepoPath,
    /// The specifier or a short description. Never file contents.
    pub detail: String,
}

/// An immutable dependency graph. Cycles and self-edges are allowed; duplicate
/// edges collapse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyGraph {
    nodes: BTreeSet<RepoPath>,
    edges: BTreeMap<RepoPath, BTreeSet<RepoPath>>,
    diagnostics: BTreeSet<GraphDiagnostic>,
    profiles: BTreeSet<String>,
    fingerprint: Fingerprint,
}

impl DependencyGraph {
    pub fn builder() -> DependencyGraphBuilder {
        DependencyGraphBuilder::default()
    }

    pub fn nodes(&self) -> &BTreeSet<RepoPath> {
        &self.nodes
    }

    pub fn contains(&self, path: &RepoPath) -> bool {
        self.nodes.contains(path)
    }

    pub fn dependencies_of(&self, consumer: &RepoPath) -> impl Iterator<Item = &RepoPath> {
        self.edges.get(consumer).into_iter().flatten()
    }

    pub fn edges(&self) -> impl Iterator<Item = (&RepoPath, &RepoPath)> {
        self.edges
            .iter()
            .flat_map(|(consumer, dependencies)| dependencies.iter().map(move |d| (consumer, d)))
    }

    pub fn edge_count(&self) -> usize {
        self.edges.values().map(BTreeSet::len).sum()
    }

    pub fn diagnostics(&self) -> &BTreeSet<GraphDiagnostic> {
        &self.diagnostics
    }

    /// Names of the resolver profiles this graph was built with.
    pub fn profiles(&self) -> &BTreeSet<String> {
        &self.profiles
    }

    /// Fingerprint of nodes, edges, diagnostics and profiles. Independent of
    /// insertion order.
    pub fn fingerprint(&self) -> &Fingerprint {
        &self.fingerprint
    }

    /// The union of two graphs. Used to combine resolver profiles: a union is
    /// an over-approximation of each operand, so it can only widen selections.
    pub fn union(&self, other: &Self) -> Self {
        let mut builder = DependencyGraphBuilder::default();
        for graph in [self, other] {
            for node in &graph.nodes {
                builder.node(node.clone());
            }
            for (consumer, dependency) in graph.edges() {
                builder.edge(consumer.clone(), dependency.clone());
            }
            for diagnostic in &graph.diagnostics {
                builder.diagnostic(diagnostic.clone());
            }
            for profile in &graph.profiles {
                builder.profile(profile.clone());
            }
        }
        builder.build()
    }
}

#[derive(Debug, Default, Clone)]
pub struct DependencyGraphBuilder {
    nodes: BTreeSet<RepoPath>,
    edges: BTreeMap<RepoPath, BTreeSet<RepoPath>>,
    diagnostics: BTreeSet<GraphDiagnostic>,
    profiles: BTreeSet<String>,
}

impl DependencyGraphBuilder {
    pub fn profile(&mut self, name: impl Into<String>) -> &mut Self {
        self.profiles.insert(name.into());
        self
    }

    pub fn node(&mut self, path: RepoPath) -> &mut Self {
        self.nodes.insert(path);
        self
    }

    /// Adds `consumer -> dependency` and both endpoints.
    pub fn edge(&mut self, consumer: RepoPath, dependency: RepoPath) -> &mut Self {
        self.nodes.insert(consumer.clone());
        self.nodes.insert(dependency.clone());
        self.edges.entry(consumer).or_default().insert(dependency);
        self
    }

    pub fn diagnostic(&mut self, diagnostic: GraphDiagnostic) -> &mut Self {
        self.diagnostics.insert(diagnostic);
        self
    }

    pub fn build(self) -> DependencyGraph {
        let mut fingerprint = FingerprintBuilder::new("testry.dependency-graph.v1");
        for profile in &self.profiles {
            fingerprint.text("profile", profile);
        }
        for node in &self.nodes {
            fingerprint.text("node", node.as_str());
        }
        for (consumer, dependencies) in &self.edges {
            for dependency in dependencies {
                fingerprint.text("consumer", consumer.as_str());
                fingerprint.text("dependency", dependency.as_str());
            }
        }
        for diagnostic in &self.diagnostics {
            fingerprint.text("diagnostic-kind", diagnostic_kind_code(diagnostic.kind));
            fingerprint.text("diagnostic-file", diagnostic.file.as_str());
            fingerprint.text("diagnostic-detail", &diagnostic.detail);
        }
        DependencyGraph {
            nodes: self.nodes,
            edges: self.edges,
            diagnostics: self.diagnostics,
            profiles: self.profiles,
            fingerprint: fingerprint.finish(),
        }
    }
}

fn diagnostic_kind_code(kind: GraphDiagnosticKind) -> &'static str {
    match kind {
        GraphDiagnosticKind::UnresolvedImport => "unresolved_import",
        GraphDiagnosticKind::DynamicDependency => "dynamic_dependency",
        GraphDiagnosticKind::ParseFailure => "parse_failure",
        GraphDiagnosticKind::UnsupportedMechanism => "unsupported_mechanism",
        GraphDiagnosticKind::OutsideRepository => "outside_repository",
        GraphDiagnosticKind::UnanalyzedPath => "unanalyzed_path",
        GraphDiagnosticKind::ResourceLimit => "resource_limit",
    }
}

impl GraphDiagnosticKind {
    pub fn code(self) -> &'static str {
        diagnostic_kind_code(self)
    }
}

/// The result of following dependency edges backwards from changed files.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Dependents {
    /// For every reached file: the next hop towards the nearest origin, or
    /// `None` when the file is an origin itself.
    next_hop: BTreeMap<RepoPath, Option<RepoPath>>,
}

impl Dependents {
    pub fn contains(&self, path: &RepoPath) -> bool {
        self.next_hop.contains_key(path)
    }

    pub fn len(&self) -> usize {
        self.next_hop.len()
    }

    pub fn is_empty(&self) -> bool {
        self.next_hop.is_empty()
    }

    pub fn paths(&self) -> impl Iterator<Item = &RepoPath> {
        self.next_hop.keys()
    }

    /// The dependency path `[path, ..., origin]`, where every step is a
    /// `consumer -> dependency` edge. `None` when `path` was not reached.
    pub fn path_to_origin(&self, path: &RepoPath) -> Option<Vec<RepoPath>> {
        let mut current = path;
        let mut route = vec![current.clone()];
        loop {
            match self.next_hop.get(current)? {
                Some(next) => {
                    route.push(next.clone());
                    current = next;
                }
                None => return Some(route),
            }
        }
    }
}

/// Every file that reaches at least one of `origins` through
/// `consumer -> dependency` edges in the union of `graphs`, including the
/// origins themselves. Breadth-first, so recorded paths are shortest paths and
/// the result is deterministic for a given input set.
pub fn dependents_of<'a>(
    graphs: &[&DependencyGraph],
    origins: impl IntoIterator<Item = &'a RepoPath>,
) -> Dependents {
    let mut consumers: BTreeMap<&RepoPath, BTreeSet<&RepoPath>> = BTreeMap::new();
    for graph in graphs {
        for (consumer, dependency) in graph.edges() {
            consumers.entry(dependency).or_default().insert(consumer);
        }
    }

    let origins: BTreeSet<&RepoPath> = origins.into_iter().collect();
    let mut next_hop: BTreeMap<RepoPath, Option<RepoPath>> = BTreeMap::new();
    let mut queue: VecDeque<&RepoPath> = VecDeque::new();
    for origin in origins {
        next_hop.insert(origin.clone(), None);
        queue.push_back(origin);
    }
    while let Some(dependency) = queue.pop_front() {
        let Some(direct_consumers) = consumers.get(dependency) else {
            continue;
        };
        for consumer in direct_consumers {
            if !next_hop.contains_key(*consumer) {
                next_hop.insert((*consumer).clone(), Some(dependency.clone()));
                queue.push_back(consumer);
            }
        }
    }
    Dependents { next_hop }
}

/// Every file reachable from `roots` along `consumer -> dependency` edges in
/// the union of `graphs`, including the roots.
pub fn dependencies_of<'a>(
    graphs: &[&DependencyGraph],
    roots: impl IntoIterator<Item = &'a RepoPath>,
) -> BTreeSet<RepoPath> {
    let mut reached: BTreeSet<RepoPath> = BTreeSet::new();
    let mut queue: VecDeque<RepoPath> = VecDeque::new();
    for root in roots {
        if reached.insert(root.clone()) {
            queue.push_back(root.clone());
        }
    }
    while let Some(consumer) = queue.pop_front() {
        for graph in graphs {
            for dependency in graph.dependencies_of(&consumer) {
                if reached.insert(dependency.clone()) {
                    queue.push_back(dependency.clone());
                }
            }
        }
    }
    reached
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn path(raw: &str) -> RepoPath {
        RepoPath::try_new(raw).unwrap()
    }

    fn graph(edges: &[(&str, &str)]) -> DependencyGraph {
        let mut builder = DependencyGraph::builder();
        for (consumer, dependency) in edges {
            builder.edge(path(consumer), path(dependency));
        }
        builder.build()
    }

    #[test]
    fn follows_edges_backwards_transitively() {
        let graph = graph(&[("user.test.ts", "user.ts"), ("user.ts", "validation.ts")]);
        let changed = path("validation.ts");
        let dependents = dependents_of(&[&graph], [&changed]);
        assert!(dependents.contains(&path("user.test.ts")));
        assert_eq!(
            dependents.path_to_origin(&path("user.test.ts")).unwrap(),
            vec![path("user.test.ts"), path("user.ts"), path("validation.ts")]
        );
        assert_eq!(
            dependents.path_to_origin(&changed).unwrap(),
            vec![changed.clone()]
        );
        assert_eq!(dependents.path_to_origin(&path("other.ts")), None);
    }

    #[test]
    fn unrelated_files_are_not_reached() {
        let graph = graph(&[("a.test.ts", "a.ts"), ("b.test.ts", "b.ts")]);
        let changed = path("a.ts");
        let dependents = dependents_of(&[&graph], [&changed]);
        assert!(dependents.contains(&path("a.test.ts")));
        assert!(!dependents.contains(&path("b.test.ts")));
    }

    #[test]
    fn cycles_terminate() {
        let graph = graph(&[
            ("a.ts", "b.ts"),
            ("b.ts", "a.ts"),
            ("t.test.ts", "a.ts"),
            ("a.ts", "a.ts"),
        ]);
        let changed = path("b.ts");
        let dependents = dependents_of(&[&graph], [&changed]);
        assert_eq!(dependents.len(), 3);
        let reachable = dependencies_of(&[&graph], [&path("t.test.ts")]);
        assert_eq!(reachable.len(), 3);
    }

    /// G08: the union with the baseline graph keeps consumers of a deleted file.
    #[test]
    fn union_of_baseline_and_current_keeps_deleted_dependencies() {
        let baseline = graph(&[("t.test.ts", "old.ts")]);
        let current = graph(&[("t.test.ts", "new.ts")]);
        let deleted = path("old.ts");
        assert!(
            dependents_of(&[&current], [&deleted])
                .paths()
                .all(|p| p == &deleted)
        );
        let dependents = dependents_of(&[&baseline, &current], [&deleted]);
        assert!(dependents.contains(&path("t.test.ts")));
    }

    #[test]
    fn duplicate_edges_and_insertion_order_do_not_matter() {
        let a = graph(&[("t", "a"), ("a", "b"), ("t", "a")]);
        let b = graph(&[("a", "b"), ("t", "a")]);
        assert_eq!(a, b);
        assert_eq!(a.fingerprint(), b.fingerprint());
        assert_eq!(a.edge_count(), 2);
    }

    #[test]
    fn diagnostics_and_profiles_change_the_fingerprint() {
        let plain = graph(&[("t", "a")]);
        let mut builder = DependencyGraph::builder();
        builder.edge(path("t"), path("a"));
        builder.diagnostic(GraphDiagnostic {
            kind: GraphDiagnosticKind::DynamicDependency,
            file: path("a"),
            detail: "import(expr)".to_owned(),
        });
        let with_diagnostic = builder.build();
        assert_ne!(plain.fingerprint(), with_diagnostic.fingerprint());

        let mut builder = DependencyGraph::builder();
        builder.edge(path("t"), path("a"));
        builder.profile("bun");
        assert_ne!(plain.fingerprint(), builder.build().fingerprint());
    }

    #[test]
    fn union_merges_everything() {
        let mut left = DependencyGraph::builder();
        left.edge(path("t"), path("a")).profile("bun");
        let mut right = DependencyGraph::builder();
        right
            .edge(path("t"), path("b"))
            .profile("vite")
            .node(path("lonely"));
        let union = left.build().union(&right.build());
        assert_eq!(union.edge_count(), 2);
        assert!(union.contains(&path("lonely")));
        assert_eq!(union.profiles().len(), 2);
    }

    fn arbitrary_edges() -> impl Strategy<Value = Vec<(u8, u8)>> {
        proptest::collection::vec((0u8..12, 0u8..12), 0..40)
    }

    fn build(edges: &[(u8, u8)]) -> DependencyGraph {
        let mut builder = DependencyGraph::builder();
        for (consumer, dependency) in edges {
            builder.edge(
                path(&format!("n{consumer}")),
                path(&format!("n{dependency}")),
            );
        }
        builder.build()
    }

    proptest! {
        /// Graph cycles: traversal always terminates and every reported path is
        /// a real chain of edges ending in an origin.
        #[test]
        fn reported_paths_are_real_edge_chains(edges in arbitrary_edges(), origin in 0u8..12) {
            let graph = build(&edges);
            let origin = path(&format!("n{origin}"));
            let dependents = dependents_of(&[&graph], [&origin]);
            for reached in dependents.paths() {
                let route = dependents.path_to_origin(reached).unwrap();
                prop_assert_eq!(route.first().unwrap(), reached);
                prop_assert_eq!(route.last().unwrap(), &origin);
                for step in route.windows(2) {
                    prop_assert!(graph.dependencies_of(&step[0]).any(|d| d == &step[1]));
                }
            }
        }

        /// Order independence and duplicate edges: shuffling or repeating edges
        /// yields the same graph and the same dependents.
        #[test]
        fn edge_order_and_duplicates_are_irrelevant(edges in arbitrary_edges(), origin in 0u8..12) {
            let mut reversed: Vec<(u8, u8)> = edges.iter().rev().copied().collect();
            reversed.extend(edges.iter().copied());
            let a = build(&edges);
            let b = build(&reversed);
            prop_assert_eq!(a.fingerprint(), b.fingerprint());
            let origin = path(&format!("n{origin}"));
            prop_assert_eq!(dependents_of(&[&a], [&origin]), dependents_of(&[&b], [&origin]));
        }

        /// P16 / monotonicity: adding edges never removes a dependent.
        #[test]
        fn adding_edges_never_shrinks_dependents(
            edges in arbitrary_edges(),
            extra in arbitrary_edges(),
            origin in 0u8..12,
        ) {
            let base = build(&edges);
            let mut all = edges.clone();
            all.extend(extra);
            let extended = build(&all);
            let origin = path(&format!("n{origin}"));
            let before = dependents_of(&[&base], [&origin]);
            let after = dependents_of(&[&extended], [&origin]);
            for reached in before.paths() {
                prop_assert!(after.contains(reached));
            }
        }

        /// Forward and backward reachability agree with each other.
        #[test]
        fn forward_and_backward_reachability_agree(edges in arbitrary_edges(), a in 0u8..12, b in 0u8..12) {
            let graph = build(&edges);
            let from = path(&format!("n{a}"));
            let to = path(&format!("n{b}"));
            let forward = dependencies_of(&[&graph], [&from]).contains(&to);
            let backward = dependents_of(&[&graph], [&to]).contains(&from);
            prop_assert_eq!(forward, backward);
        }
    }
}
