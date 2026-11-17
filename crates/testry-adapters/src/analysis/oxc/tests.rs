//! Analyzer acceptance tests on in-memory snapshots (spec section 18, G01-G08,
//! G15-G17). No Git and no disk access are involved.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use testry_application::ports::PortError;
use testry_application::ports::planning::{
    AnalysisLimits, AnalysisProfile, DependencyAnalyzer, ResolverProfile, SnapshotSide,
};
use testry_application::testing::{cancelled_context, context};
use testry_domain::fingerprint::Fingerprint;
use testry_domain::graph::{DependencyGraph, GraphDiagnosticKind};
use testry_domain::ids::{BaselineId, WorkspaceId};
use testry_domain::paths::RepoPath;
use testry_domain::snapshot::{
    BaselineFile, ChangeSource, CurrentFile, FileKind, SourceSnapshot, SourceSnapshotParts,
};

use super::OxcDependencyAnalyzer;
use crate::source::reader::memory::MemoryReader;

fn path(raw: &str) -> RepoPath {
    RepoPath::try_new(raw).unwrap()
}

/// A workspace described by its files. `@link` as content marks a symbolic
/// link, `@submodule` a submodule.
#[derive(Default)]
struct Workspace {
    current: Vec<(String, String)>,
    /// Baseline content of files that differ from the working tree. `None`
    /// means the file does not exist in the working tree any more.
    baseline_only: Vec<(String, String)>,
    changed: Vec<String>,
    with_baseline: bool,
}

impl Workspace {
    fn of(files: &[(&str, &str)]) -> Self {
        Self {
            current: files
                .iter()
                .map(|(name, content)| ((*name).to_owned(), (*content).to_owned()))
                .collect(),
            ..Self::default()
        }
    }

    fn kind(content: &str) -> FileKind {
        match content {
            "@link" => FileKind::Symlink,
            "@submodule" => FileKind::Submodule,
            _ => FileKind::File,
        }
    }

    fn build(&self) -> (SourceSnapshot, MemoryReader) {
        let mut reader = MemoryReader::default();
        let mut current = BTreeMap::new();
        let mut baseline_files = BTreeMap::new();
        let changed: BTreeSet<&str> = self.changed.iter().map(String::as_str).collect();
        let baseline_only: BTreeMap<&str, &str> = self
            .baseline_only
            .iter()
            .map(|(name, content)| (name.as_str(), content.as_str()))
            .collect();

        for (name, content) in &self.current {
            let digest = Fingerprint::of_bytes(content.as_bytes());
            current.insert(
                path(name),
                CurrentFile {
                    kind: Self::kind(content),
                    digest,
                },
            );
            reader
                .current
                .insert(path(name), content.as_bytes().to_vec());
            // Unchanged files exist with the same content in the baseline.
            if self.with_baseline && !changed.contains(name.as_str()) {
                baseline_files.insert(
                    path(name),
                    BaselineFile {
                        kind: Self::kind(content),
                        object_id: format!("{:0>40}", "0"),
                    },
                );
            }
        }
        for (index, (name, content)) in baseline_only.iter().enumerate() {
            let object_id = format!("{:0>40x}", index + 1);
            reader
                .baseline
                .insert(object_id.clone(), content.as_bytes().to_vec());
            baseline_files.insert(
                path(name),
                BaselineFile {
                    kind: Self::kind(content),
                    object_id,
                },
            );
        }

        let snapshot = SourceSnapshot::new(
            WorkspaceId::try_new("ws-test").unwrap(),
            SourceSnapshotParts {
                baseline: self
                    .with_baseline
                    .then(|| BaselineId::try_new("a".repeat(40)).unwrap()),
                changes: self
                    .changed
                    .iter()
                    .map(|name| (path(name), BTreeSet::from([ChangeSource::Unstaged])))
                    .collect(),
                current,
                baseline_files,
                ..SourceSnapshotParts::default()
            },
        );
        (snapshot, reader)
    }

    async fn analyze(
        &self,
        side: SnapshotSide,
        resolver: ResolverProfile,
    ) -> Result<DependencyGraph, PortError> {
        let (snapshot, reader) = self.build();
        let analyzer = OxcDependencyAnalyzer::new(Arc::new(reader));
        let profile = AnalysisProfile {
            side,
            resolver,
            limits: AnalysisLimits::default(),
        };
        analyzer.analyze(&snapshot, &profile, &context()).await
    }

    async fn graph(&self) -> DependencyGraph {
        self.analyze(SnapshotSide::Current, ResolverProfile::Bun)
            .await
            .unwrap()
    }
}

async fn graph(files: &[(&str, &str)]) -> DependencyGraph {
    Workspace::of(files).graph().await
}

fn dependencies(graph: &DependencyGraph, consumer: &str) -> Vec<String> {
    graph
        .dependencies_of(&path(consumer))
        .map(|dependency| dependency.as_str().to_owned())
        .collect()
}

fn diagnostics(graph: &DependencyGraph) -> Vec<(GraphDiagnosticKind, String, String)> {
    graph
        .diagnostics()
        .iter()
        .map(|diagnostic| {
            (
                diagnostic.kind,
                diagnostic.file.as_str().to_owned(),
                diagnostic.detail.clone(),
            )
        })
        .collect()
}

fn assert_clean(graph: &DependencyGraph) {
    assert!(
        graph.diagnostics().is_empty(),
        "unexpected diagnostics: {:?}",
        diagnostics(graph)
    );
}

/// G01 / G02: relative imports resolve through extension probing and chain.
#[tokio::test]
async fn g01_g02_relative_imports_form_a_transitive_chain() {
    let graph = graph(&[
        ("src/user.test.ts", "import { user } from './user';"),
        (
            "src/user.ts",
            "import { validate } from './lib/validation';",
        ),
        (
            "src/lib/validation.ts",
            "export const validate = () => true;",
        ),
        ("src/other.ts", "export const other = 1;"),
    ])
    .await;
    assert_clean(&graph);
    assert_eq!(dependencies(&graph, "src/user.test.ts"), ["src/user.ts"]);
    assert_eq!(
        dependencies(&graph, "src/user.ts"),
        ["src/lib/validation.ts"]
    );
    assert!(
        graph.contains(&path("src/other.ts")),
        "every module is a node"
    );
    assert_eq!(graph.profiles().iter().collect::<Vec<_>>(), ["bun"]);
}

/// G03: a barrel re-export connects the test to the implementation.
#[tokio::test]
async fn g03_barrel_re_exports_are_followed() {
    let graph = graph(&[
        ("src/a.test.ts", "import { a } from './lib';"),
        (
            "src/lib/index.ts",
            "export * from './a'; export { b } from './b';",
        ),
        ("src/lib/a.ts", "export const a = 1;"),
        ("src/lib/b.ts", "export const b = 1;"),
    ])
    .await;
    assert_clean(&graph);
    assert_eq!(dependencies(&graph, "src/a.test.ts"), ["src/lib/index.ts"]);
    assert_eq!(
        dependencies(&graph, "src/lib/index.ts"),
        ["src/lib/a.ts", "src/lib/b.ts"]
    );
}

/// G04: type-only imports are dependencies, including declaration files.
#[tokio::test]
async fn g04_type_only_imports_resolve_to_declarations() {
    let graph = graph(&[
        (
            "src/a.test.ts",
            "import type { A } from './types'; import type { B } from './model';",
        ),
        ("src/types.d.ts", "export interface A {}"),
        ("src/model.ts", "export interface B {}"),
    ])
    .await;
    assert_clean(&graph);
    assert_eq!(
        dependencies(&graph, "src/a.test.ts"),
        ["src/model.ts", "src/types.d.ts"]
    );
}

/// G05: literal dynamic imports and requires are followed.
#[tokio::test]
async fn g05_literal_dynamic_import_and_require_are_followed() {
    let graph = graph(&[
        (
            "src/a.test.ts",
            "const a = await import('./a'); const b = require('./b.cjs');",
        ),
        ("src/a.ts", "export {};"),
        ("src/b.cjs", "module.exports = {};"),
    ])
    .await;
    assert_clean(&graph);
    assert_eq!(
        dependencies(&graph, "src/a.test.ts"),
        ["src/a.ts", "src/b.cjs"]
    );
}

/// G06: a non-literal dynamic import is a diagnostic.
#[tokio::test]
async fn g06_non_literal_dynamic_import_is_a_diagnostic() {
    let graph = graph(&[
        (
            "src/i18n.ts",
            "export const load = (lang: string) => import(`./locales/${lang}.ts`);",
        ),
        ("src/locales/en.ts", "export default {};"),
    ])
    .await;
    assert_eq!(
        diagnostics(&graph),
        [(
            GraphDiagnosticKind::DynamicDependency,
            "src/i18n.ts".to_owned(),
            "import(<expression>)".to_owned()
        )]
    );
}

/// G07: an alias that does not resolve and a file that does not parse are
/// diagnostics, never "no dependency".
#[tokio::test]
async fn g07_unresolved_alias_and_parse_failure_are_diagnostics() {
    let graph = graph(&[
        (
            "src/a.test.ts",
            "import { x } from '@/missing'; import './gone';",
        ),
        ("src/broken.ts", "import { from './a'"),
        ("src/a.ts", "export {};"),
    ])
    .await;
    assert_eq!(
        diagnostics(&graph),
        [
            (
                GraphDiagnosticKind::UnresolvedImport,
                "src/a.test.ts".to_owned(),
                "./gone".to_owned()
            ),
            (
                GraphDiagnosticKind::UnresolvedImport,
                "src/a.test.ts".to_owned(),
                "@/missing".to_owned()
            ),
            (
                GraphDiagnosticKind::ParseFailure,
                "src/broken.ts".to_owned(),
                "syntax error".to_owned()
            ),
        ]
    );
}

#[tokio::test]
async fn tsconfig_paths_and_base_url_are_resolved() {
    let graph = graph(&[
        (
            "tsconfig.json",
            r#"{
                // comments are allowed in tsconfig
                "compilerOptions": { "baseUrl": ".", "paths": { "@/*": ["src/*"], "~shared": ["packages/shared/index.ts"] } },
            }"#,
        ),
        ("src/a.test.ts", "import { a } from '@/lib/a'; import s from '~shared';"),
        ("src/lib/a.ts", "export const a = 1;"),
        ("packages/shared/index.ts", "export default 1;"),
    ])
    .await;
    assert_clean(&graph);
    assert_eq!(
        dependencies(&graph, "src/a.test.ts"),
        ["packages/shared/index.ts", "src/lib/a.ts"]
    );
}

#[tokio::test]
async fn the_nearest_tsconfig_applies_to_each_file() {
    let graph = graph(&[
        (
            "apps/api/tsconfig.json",
            r#"{ "compilerOptions": { "paths": { "@/*": ["./src/*"] } } }"#,
        ),
        (
            "apps/web/tsconfig.json",
            r#"{ "compilerOptions": { "paths": { "@/*": ["./app/*"] } } }"#,
        ),
        ("apps/api/a.test.ts", "import '@/x';"),
        ("apps/web/b.test.ts", "import '@/x';"),
        ("apps/api/src/x.ts", "export {};"),
        ("apps/web/app/x.ts", "export {};"),
    ])
    .await;
    assert_clean(&graph);
    assert_eq!(
        dependencies(&graph, "apps/api/a.test.ts"),
        ["apps/api/src/x.ts"]
    );
    assert_eq!(
        dependencies(&graph, "apps/web/b.test.ts"),
        ["apps/web/app/x.ts"]
    );
}

#[tokio::test]
async fn a_tsconfig_that_extends_outside_the_repository_is_one_diagnostic() {
    let graph = graph(&[
        (
            "tsconfig.json",
            r#"{ "extends": "@tsconfig/strictest/tsconfig.json" }"#,
        ),
        ("src/a.test.ts", "import './a'; import './b';"),
        ("src/a.ts", "export {};"),
        ("src/b.ts", "export {};"),
    ])
    .await;
    let found = diagnostics(&graph);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, GraphDiagnosticKind::UnsupportedMechanism);
    assert_eq!(found[0].1, "tsconfig.json");
}

#[tokio::test]
async fn workspace_packages_resolve_through_their_manifest() {
    let graph = graph(&[
        ("package.json", r#"{ "name": "root", "workspaces": ["packages/*", "apps/*"] }"#),
        (
            "packages/shared/package.json",
            r#"{ "name": "@app/shared", "exports": { ".": "./src/index.ts", "./date": "./src/date.ts" } }"#,
        ),
        ("packages/shared/src/index.ts", "export * from './date';"),
        ("packages/shared/src/date.ts", "export const now = () => 0;"),
        ("packages/legacy/package.json", r#"{ "name": "legacy", "main": "lib/main.js" }"#),
        ("packages/legacy/lib/main.js", "module.exports = {};"),
        ("apps/api/package.json", r#"{ "name": "api", "dependencies": { "@app/shared": "workspace:*" } }"#),
        (
            "apps/api/a.test.ts",
            "import { now } from '@app/shared'; import d from '@app/shared/date'; import l from 'legacy';",
        ),
    ])
    .await;
    assert_clean(&graph);
    assert_eq!(
        dependencies(&graph, "apps/api/a.test.ts"),
        [
            "packages/legacy/lib/main.js",
            "packages/shared/src/date.ts",
            "packages/shared/src/index.ts"
        ]
    );
}

#[tokio::test]
async fn a_workspace_package_that_points_at_missing_build_output_is_a_diagnostic() {
    let graph = graph(&[
        (
            "packages/shared/package.json",
            r#"{ "name": "@app/shared", "main": "dist/index.js" }"#,
        ),
        ("packages/shared/src/index.ts", "export {};"),
        (
            "apps/api/package.json",
            r#"{ "name": "api", "dependencies": { "@app/shared": "workspace:*" } }"#,
        ),
        ("apps/api/a.test.ts", "import '@app/shared';"),
    ])
    .await;
    assert_eq!(
        diagnostics(&graph),
        [(
            GraphDiagnosticKind::UnresolvedImport,
            "apps/api/a.test.ts".to_owned(),
            "@app/shared".to_owned()
        )],
        "a declared workspace package must never be classified as external"
    );
}

#[tokio::test]
async fn declared_external_packages_and_builtins_are_not_dependencies() {
    let graph = graph(&[
        (
            "package.json",
            r#"{ "name": "app", "dependencies": { "hono": "4" }, "devDependencies": { "vitest": "3", "@types/express": "5", "@scope/pkg": "1" } }"#,
        ),
        (
            "src/a.test.ts",
            r#"
            import { Hono } from "hono";
            import { cors } from "hono/cors";
            import { test } from "vitest";
            import type { Request } from "express";
            import x from "@scope/pkg/sub/path.js";
            import { expect } from "bun:test";
            import fs from "node:fs";
            import path from "path";
            import { $ } from "bun";
            "#,
        ),
    ])
    .await;
    assert_clean(&graph);
    assert!(dependencies(&graph, "src/a.test.ts").is_empty());
}

#[tokio::test]
async fn undeclared_packages_and_virtual_modules_are_diagnostics() {
    let graph = graph(&[
        (
            "package.json",
            r#"{ "name": "app", "dependencies": { "hono": "4" } }"#,
        ),
        (
            "src/a.ts",
            "import 'left-pad'; import 'virtual:pwa-register'; import '#internal';",
        ),
    ])
    .await;
    let found = diagnostics(&graph);
    assert_eq!(found.len(), 3, "{found:?}");
    assert!(
        found
            .iter()
            .all(|(kind, _, _)| *kind == GraphDiagnosticKind::UnresolvedImport)
    );
}

#[tokio::test]
async fn assets_and_query_suffixes_are_input_nodes() {
    let graph = graph(&[
        (
            "src/App.tsx",
            r#"
            import data from "./data.json";
            import "./App.css";
            import Logo from "./logo.svg?react";
            import raw from "./notes.txt?raw";
            export const App = () => <Logo />;
            "#,
        ),
        ("src/data.json", "{}"),
        ("src/App.css", "body {}"),
        ("src/logo.svg", "<svg/>"),
        ("src/notes.txt", "notes"),
    ])
    .await;
    assert_clean(&graph);
    assert_eq!(
        dependencies(&graph, "src/App.tsx"),
        [
            "src/App.css",
            "src/data.json",
            "src/logo.svg",
            "src/notes.txt"
        ]
    );
}

#[tokio::test]
async fn javascript_extensions_map_to_typescript_sources() {
    let graph = graph(&[
        (
            "src/a.test.ts",
            "import './a.js'; import './b.mjs'; import './c.jsx';",
        ),
        ("src/a.ts", "export {};"),
        ("src/b.mts", "export {};"),
        ("src/c.tsx", "export {};"),
    ])
    .await;
    assert_clean(&graph);
    assert_eq!(
        dependencies(&graph, "src/a.test.ts"),
        ["src/a.ts", "src/b.mts", "src/c.tsx"]
    );
}

/// The extension order of a runner must not decide the graph: every sibling
/// that could satisfy the specifier is a dependency.
#[tokio::test]
async fn ambiguous_siblings_are_all_dependencies() {
    let graph = graph(&[
        (
            "src/a.test.ts",
            "import './util'; import './exact.js'; import './dir';",
        ),
        ("src/util.ts", "export {};"),
        ("src/util.js", "export {};"),
        ("src/util/index.ts", "export {};"),
        ("src/exact.js", "export {};"),
        ("src/exact.ts", "export {};"),
        ("src/dir/index.ts", "export {};"),
        ("src/dir/index.js", "export {};"),
        ("src/unrelated.ts", "export {};"),
    ])
    .await;
    assert_clean(&graph);
    assert_eq!(
        dependencies(&graph, "src/a.test.ts"),
        [
            "src/dir/index.js",
            "src/dir/index.ts",
            "src/exact.js",
            "src/exact.ts",
            "src/util.js",
            "src/util.ts",
            "src/util/index.ts",
        ]
    );
}

#[tokio::test]
async fn a_specifier_that_names_a_file_exactly_has_one_dependency() {
    let graph = graph(&[
        ("src/a.test.ts", "import './util.ts'; import './data.json';"),
        ("src/util.ts", "export {};"),
        ("src/util.js", "export {};"),
        ("src/data.json", "{}"),
        ("src/data.ts", "export {};"),
    ])
    .await;
    assert_clean(&graph);
    assert_eq!(
        dependencies(&graph, "src/a.test.ts"),
        ["src/data.json", "src/util.ts"]
    );
}

/// G16: adding a file changes where an unchanged import resolves to.
#[tokio::test]
async fn g16_resolution_is_recomputed_when_a_file_is_added() {
    let before = graph(&[
        ("src/a.test.ts", "import './util';"),
        ("src/util.js", "export {};"),
    ])
    .await;
    assert_eq!(dependencies(&before, "src/a.test.ts"), ["src/util.js"]);

    let after = graph(&[
        ("src/a.test.ts", "import './util';"),
        ("src/util.js", "export {};"),
        ("src/util.ts", "export {};"),
    ])
    .await;
    assert_eq!(
        dependencies(&after, "src/a.test.ts"),
        ["src/util.js", "src/util.ts"]
    );
    assert_ne!(before.fingerprint(), after.fingerprint());
}

/// G15: a tsconfig change changes resolution; nothing stale is reused.
#[tokio::test]
async fn g15_resolution_follows_tsconfig_changes() {
    let files = |target: &str| {
        vec![
            (
                "tsconfig.json".to_owned(),
                format!(r#"{{ "compilerOptions": {{ "paths": {{ "@lib": ["{target}"] }} }} }}"#),
            ),
            ("src/a.test.ts".to_owned(), "import '@lib';".to_owned()),
            ("src/v1.ts".to_owned(), "export {};".to_owned()),
            ("src/v2.ts".to_owned(), "export {};".to_owned()),
        ]
    };
    let analyze = async |target: &str| {
        let workspace = Workspace {
            current: files(target),
            ..Workspace::default()
        };
        workspace.graph().await
    };
    let v1 = analyze("./src/v1.ts").await;
    let v2 = analyze("./src/v2.ts").await;
    assert_eq!(dependencies(&v1, "src/a.test.ts"), ["src/v1.ts"]);
    assert_eq!(dependencies(&v2, "src/a.test.ts"), ["src/v2.ts"]);
}

/// G17: a resolution that needs a symbolic link or a submodule is a
/// diagnostic. Links nobody imports do not matter.
#[tokio::test]
async fn g17_symlinks_and_submodules_are_not_followed() {
    let graph = graph(&[
        (
            "src/a.test.ts",
            "import './linked'; import '../vendor/lib/index';",
        ),
        ("src/linked.ts", "@link"),
        ("vendor/lib", "@submodule"),
        ("docs/readme-link.md", "@link"),
    ])
    .await;
    let found = diagnostics(&graph);
    let unanalyzed: Vec<&str> = found
        .iter()
        .filter(|(kind, _, _)| *kind == GraphDiagnosticKind::UnanalyzedPath)
        .map(|(_, file, _)| file.as_str())
        .collect();
    assert_eq!(unanalyzed, ["src/linked.ts", "vendor/lib"]);
    assert!(found.iter().any(
        |(kind, _, detail)| *kind == GraphDiagnosticKind::UnresolvedImport && detail == "./linked"
    ));
    assert!(
        !found
            .iter()
            .any(|(_, file, _)| file == "docs/readme-link.md")
    );
}

#[tokio::test]
async fn export_conditions_differ_per_profile_and_all_variants_are_kept() {
    let workspace = Workspace::of(&[
        (
            "packages/rt/package.json",
            r#"{ "name": "rt", "exports": { "bun": "./src/bun.ts", "browser": "./src/browser.ts", "import": "./src/esm.ts", "default": "./src/cjs.ts" } }"#,
        ),
        ("packages/rt/src/bun.ts", "export {};"),
        ("packages/rt/src/browser.ts", "export {};"),
        ("packages/rt/src/esm.ts", "export {};"),
        ("packages/rt/src/cjs.ts", "export {};"),
        ("src/a.test.ts", "import 'rt';"),
    ]);
    let bun = workspace
        .analyze(SnapshotSide::Current, ResolverProfile::Bun)
        .await
        .unwrap();
    assert_eq!(
        dependencies(&bun, "src/a.test.ts"),
        ["packages/rt/src/bun.ts"]
    );

    let node = workspace
        .analyze(SnapshotSide::Current, ResolverProfile::Node)
        .await
        .unwrap();
    assert_eq!(
        dependencies(&node, "src/a.test.ts"),
        ["packages/rt/src/cjs.ts", "packages/rt/src/esm.ts"],
        "import and require conditions are both possible"
    );

    let vite = workspace
        .analyze(SnapshotSide::Current, ResolverProfile::Vite)
        .await
        .unwrap();
    assert_eq!(
        dependencies(&vite, "src/a.test.ts"),
        [
            "packages/rt/src/browser.ts",
            "packages/rt/src/cjs.ts",
            "packages/rt/src/esm.ts"
        ]
    );
    assert_eq!(vite.profiles().iter().collect::<Vec<_>>(), ["vite"]);
}

/// G08: the baseline graph is built from Git objects and still knows files
/// that were deleted or changed in the working tree.
#[tokio::test]
async fn g08_baseline_graph_keeps_deleted_and_changed_dependencies() {
    let workspace = Workspace {
        current: vec![
            ("src/a.test.ts".to_owned(), "import './new';".to_owned()),
            ("src/new.ts".to_owned(), "export {};".to_owned()),
            ("src/keep.ts".to_owned(), "import './new';".to_owned()),
        ],
        baseline_only: vec![
            ("src/a.test.ts".to_owned(), "import './old';".to_owned()),
            ("src/old.ts".to_owned(), "import './deep';".to_owned()),
            ("src/deep.ts".to_owned(), "export {};".to_owned()),
        ],
        changed: vec![
            "src/a.test.ts".to_owned(),
            "src/old.ts".to_owned(),
            "src/deep.ts".to_owned(),
            "src/new.ts".to_owned(),
        ],
        with_baseline: true,
    };

    let baseline = workspace
        .analyze(SnapshotSide::Baseline, ResolverProfile::Bun)
        .await
        .unwrap();
    assert_eq!(dependencies(&baseline, "src/a.test.ts"), ["src/old.ts"]);
    assert_eq!(dependencies(&baseline, "src/old.ts"), ["src/deep.ts"]);
    // src/keep.ts is unchanged, but src/new.ts did not exist in the baseline.
    assert_eq!(
        diagnostics(&baseline),
        [(
            GraphDiagnosticKind::UnresolvedImport,
            "src/keep.ts".to_owned(),
            "./new".to_owned()
        )]
    );

    let current = workspace
        .analyze(SnapshotSide::Current, ResolverProfile::Bun)
        .await
        .unwrap();
    assert_clean(&current);
    assert_eq!(dependencies(&current, "src/a.test.ts"), ["src/new.ts"]);
    assert!(!current.contains(&path("src/old.ts")));
}

#[tokio::test]
async fn analyzing_the_baseline_without_a_baseline_is_an_error() {
    let workspace = Workspace::of(&[("a.ts", "export {};")]);
    let error = workspace
        .analyze(SnapshotSide::Baseline, ResolverProfile::Bun)
        .await
        .unwrap_err();
    assert_eq!(error.code(), "configuration");
}

/// P13: a file that changed after the snapshot fails the analysis.
#[tokio::test]
async fn p13_a_file_changed_after_the_snapshot_is_stale() {
    let workspace = Workspace::of(&[("src/a.ts", "import './b';"), ("src/b.ts", "export {};")]);
    let (snapshot, mut reader) = workspace.build();
    reader
        .current
        .insert(path("src/a.ts"), b"import './c';".to_vec());
    let analyzer = OxcDependencyAnalyzer::new(Arc::new(reader));
    let profile = AnalysisProfile {
        side: SnapshotSide::Current,
        resolver: ResolverProfile::Bun,
        limits: AnalysisLimits::default(),
    };
    let error = analyzer
        .analyze(&snapshot, &profile, &context())
        .await
        .unwrap_err();
    assert_eq!(error.code(), "stale_inputs");
}

#[tokio::test]
async fn a_changed_manifest_during_resolution_is_stale_too() {
    let workspace = Workspace::of(&[
        ("package.json", r#"{ "name": "app" }"#),
        (
            "tsconfig.json",
            r#"{ "compilerOptions": { "paths": { "@/*": ["src/*"] } } }"#,
        ),
        ("src/a.ts", "import '@/b';"),
        ("src/b.ts", "export {};"),
    ]);
    let (snapshot, mut reader) = workspace.build();
    reader.current.insert(path("tsconfig.json"), b"{}".to_vec());
    let analyzer = OxcDependencyAnalyzer::new(Arc::new(reader));
    let profile = AnalysisProfile {
        side: SnapshotSide::Current,
        resolver: ResolverProfile::Bun,
        limits: AnalysisLimits::default(),
    };
    let error = analyzer
        .analyze(&snapshot, &profile, &context())
        .await
        .unwrap_err();
    assert_eq!(error.code(), "stale_inputs");
}

#[tokio::test]
async fn a_file_over_the_analysis_limit_is_a_diagnostic() {
    let big = format!("import './b';\n// {}", "x".repeat(4096));
    let workspace = Workspace::of(&[("src/a.ts", big.as_str()), ("src/b.ts", "export {};")]);
    let (snapshot, reader) = workspace.build();
    let analyzer = OxcDependencyAnalyzer::new(Arc::new(reader));
    let profile = AnalysisProfile {
        side: SnapshotSide::Current,
        resolver: ResolverProfile::Bun,
        limits: AnalysisLimits {
            max_source_bytes: 1024,
        },
    };
    let graph = analyzer
        .analyze(&snapshot, &profile, &context())
        .await
        .unwrap();
    assert_eq!(diagnostics(&graph).len(), 1);
    assert_eq!(diagnostics(&graph)[0].0, GraphDiagnosticKind::ResourceLimit);
    assert!(dependencies(&graph, "src/a.ts").is_empty());
}

#[tokio::test]
async fn invalid_manifests_and_duplicate_package_names_are_handled() {
    let graph = graph(&[
        ("broken/package.json", "{ not json"),
        ("templates/a/package.json", r#"{ "name": "template" }"#),
        ("templates/b/package.json", r#"{ "name": "template" }"#),
        ("templates/a/index.ts", "export {};"),
        ("src/a.ts", "export {};"),
    ])
    .await;
    assert_eq!(
        diagnostics(&graph),
        [(
            GraphDiagnosticKind::ParseFailure,
            "broken/package.json".to_owned(),
            "package.json is not valid JSON".to_owned()
        )],
        "an unused duplicate name is not a problem by itself"
    );

    let importing = super::tests::graph(&[
        (
            "templates/a/package.json",
            r#"{ "name": "template", "main": "index.ts" }"#,
        ),
        (
            "templates/b/package.json",
            r#"{ "name": "template", "main": "index.ts" }"#,
        ),
        ("templates/a/index.ts", "export {};"),
        ("templates/b/index.ts", "export {};"),
        (
            "package.json",
            r#"{ "name": "root", "dependencies": { "template": "workspace:*" } }"#,
        ),
        ("src/a.ts", "import 'template';"),
    ])
    .await;
    assert_eq!(
        diagnostics(&importing),
        [(
            GraphDiagnosticKind::UnresolvedImport,
            "src/a.ts".to_owned(),
            "template".to_owned()
        )],
        "an ambiguous workspace package is neither linked nor treated as external"
    );
}

#[tokio::test]
async fn cycles_and_self_imports_are_fine() {
    let graph = graph(&[
        ("src/a.ts", "import './b'; import './a';"),
        ("src/b.ts", "import './a';"),
    ])
    .await;
    assert_clean(&graph);
    assert_eq!(dependencies(&graph, "src/a.ts"), ["src/a.ts", "src/b.ts"]);
    assert_eq!(dependencies(&graph, "src/b.ts"), ["src/a.ts"]);
}

#[tokio::test]
async fn paths_with_spaces_and_japanese_resolve() {
    let graph = graph(&[
        (
            "apps/日本語 の/ユーザー.test.ts",
            "import './ユーザー'; import './[id] page';",
        ),
        ("apps/日本語 の/ユーザー.ts", "export {};"),
        ("apps/日本語 の/[id] page.tsx", "export {};"),
    ])
    .await;
    assert_clean(&graph);
    assert_eq!(
        dependencies(&graph, "apps/日本語 の/ユーザー.test.ts"),
        ["apps/日本語 の/[id] page.tsx", "apps/日本語 の/ユーザー.ts"]
    );
}

#[tokio::test]
async fn the_graph_is_deterministic() {
    let files = [
        (
            "src/a.test.ts",
            "import './a'; import './b'; import('./c');",
        ),
        ("src/a.ts", "import './b';"),
        ("src/b.ts", "export {};"),
        ("src/c.ts", "import(name);"),
    ];
    let mut reversed = files;
    reversed.reverse();
    let first = graph(&files).await;
    let second = graph(&reversed).await;
    assert_eq!(first, second);
    assert_eq!(first.fingerprint(), second.fingerprint());
}

#[tokio::test]
async fn a_cancelled_analysis_stops() {
    let (snapshot, reader) = Workspace::of(&[("a.ts", "export {};")]).build();
    let analyzer = OxcDependencyAnalyzer::new(Arc::new(reader));
    let profile = AnalysisProfile {
        side: SnapshotSide::Current,
        resolver: ResolverProfile::Bun,
        limits: AnalysisLimits::default(),
    };
    let error = analyzer
        .analyze(&snapshot, &profile, &cancelled_context())
        .await
        .unwrap_err();
    assert_eq!(error, PortError::Cancelled);
}
