//! Architecture rules of spec section 3, checked against the resolved
//! dependency graph (`cargo metadata`) and the sources.
//!
//! ```text
//! testry-cli -> testry-application -> testry-domain
//!          \--> testry-adapters ----> testry-application, testry-domain
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
    resolve: Resolve,
}

#[derive(Deserialize)]
struct Package {
    id: String,
    name: String,
}

#[derive(Deserialize)]
struct Resolve {
    nodes: Vec<Node>,
}

#[derive(Deserialize)]
struct Node {
    id: String,
    deps: Vec<NodeDependency>,
}

#[derive(Deserialize)]
struct NodeDependency {
    pkg: String,
    dep_kinds: Vec<DependencyKind>,
}

#[derive(Deserialize)]
struct DependencyKind {
    kind: Option<String>,
}

const CRATES: [&str; 4] = [
    "testry-domain",
    "testry-application",
    "testry-adapters",
    "testry-cli",
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn host_triple() -> String {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_owned());
    let output = Command::new(rustc)
        .arg("-vV")
        .output()
        .expect("rustc must be runnable");
    let text = String::from_utf8(output.stdout).unwrap();
    text.lines()
        .find_map(|line| line.strip_prefix("host: "))
        .expect("rustc -vV prints the host triple")
        .to_owned()
}

struct Graph {
    names: BTreeMap<String, String>,
    normal_dependencies: BTreeMap<String, Vec<String>>,
}

impl Graph {
    fn load() -> Self {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
        // Restricting the graph to the host platform keeps the check offline:
        // packages for other platforms need not be downloaded.
        let host = host_triple();
        let output = Command::new(cargo)
            .args(["metadata", "--format-version", "1", "--locked", "--offline"])
            .args(["--filter-platform", &host])
            .current_dir(workspace_root())
            .output()
            .expect("cargo metadata must be runnable");
        assert!(
            output.status.success(),
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let metadata: Metadata = serde_json::from_slice(&output.stdout).unwrap();
        let names = metadata
            .packages
            .into_iter()
            .map(|package| (package.id, package.name))
            .collect();
        let normal_dependencies = metadata
            .resolve
            .nodes
            .into_iter()
            .map(|node| {
                // Dev and build dependencies never end up in the shipped code.
                let normal = node
                    .deps
                    .into_iter()
                    .filter(|dependency| {
                        dependency.dep_kinds.iter().any(|kind| kind.kind.is_none())
                    })
                    .map(|dependency| dependency.pkg)
                    .collect();
                (node.id, normal)
            })
            .collect();
        Self {
            names,
            normal_dependencies,
        }
    }

    fn id_of(&self, name: &str) -> &str {
        self.names
            .iter()
            .find(|(_, package)| package.as_str() == name)
            .map(|(id, _)| id.as_str())
            .unwrap_or_else(|| panic!("package {name} is not part of the workspace graph"))
    }

    fn direct(&self, name: &str) -> BTreeSet<&str> {
        self.normal_dependencies[self.id_of(name)]
            .iter()
            .map(|id| self.names[id].as_str())
            .collect()
    }

    /// Names of every crate reachable through normal dependencies.
    fn closure(&self, name: &str) -> BTreeSet<&str> {
        let mut reached = BTreeSet::new();
        let mut queue = VecDeque::from([self.id_of(name)]);
        while let Some(id) = queue.pop_front() {
            for dependency in &self.normal_dependencies[id] {
                if reached.insert(self.names[dependency].as_str()) {
                    queue.push_back(dependency);
                }
            }
        }
        reached
    }
}

fn starts_with_any(name: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|prefix| {
        name == *prefix
            || name.starts_with(&format!("{prefix}_"))
            || name.starts_with(&format!("{prefix}-"))
    })
}

#[test]
fn domain_depends_on_pure_computation_crates_only() {
    let graph = Graph::load();
    assert_eq!(
        graph.direct("testry-domain"),
        BTreeSet::from(["blake3", "globset", "serde", "serde_json", "thiserror"]),
        "testry-domain gained or lost a direct dependency; review it against spec section 3"
    );
    let forbidden = [
        "tokio",
        "clap",
        "oxc",
        "rusqlite",
        "libsqlite3",
        "bollard",
        "testry",
    ];
    let offenders: Vec<&str> = graph
        .closure("testry-domain")
        .into_iter()
        .filter(|name| starts_with_any(name, &forbidden))
        .collect();
    assert!(
        offenders.is_empty(),
        "testry-domain must not reach {offenders:?}"
    );
}

#[test]
fn application_depends_on_the_domain_but_never_on_adapters_or_the_cli() {
    let graph = Graph::load();
    let closure = graph.closure("testry-application");
    assert!(closure.contains("testry-domain"));
    let forbidden = [
        "testry-adapters",
        "testry-cli",
        "clap",
        "oxc",
        "rusqlite",
        "libsqlite3",
        "bollard",
    ];
    let offenders: Vec<&str> = closure
        .into_iter()
        .filter(|name| starts_with_any(name, &forbidden))
        .collect();
    assert!(
        offenders.is_empty(),
        "testry-application must not reach {offenders:?}"
    );
}

#[test]
fn adapters_implement_application_ports_and_do_not_know_the_cli() {
    let graph = Graph::load();
    let direct = graph.direct("testry-adapters");
    assert!(direct.contains("testry-application"));
    assert!(direct.contains("testry-domain"));
    assert!(!graph.closure("testry-adapters").contains("testry-cli"));
}

#[test]
fn cli_is_the_only_crate_that_wires_everything() {
    let graph = Graph::load();
    let direct = graph.direct("testry-cli");
    for expected in ["testry-domain", "testry-application", "testry-adapters"] {
        assert!(
            direct.contains(expected),
            "testry-cli must depend on {expected}"
        );
    }
    for name in ["testry-domain", "testry-application", "testry-adapters"] {
        assert!(
            !graph.closure(name).contains("testry-cli"),
            "{name} must not depend on testry-cli"
        );
    }
}

fn manifest(name: &str) -> toml::Table {
    let path = workspace_root()
        .join("crates")
        .join(name)
        .join("Cargo.toml");
    toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn test_fakes_are_never_enabled_in_shipped_dependencies() {
    for name in ["testry-adapters", "testry-cli"] {
        let manifest = manifest(name);
        let dependencies = manifest["dependencies"].as_table().unwrap();
        for (dependency, specification) in dependencies {
            let features = specification
                .get("features")
                .and_then(toml::Value::as_array)
                .cloned()
                .unwrap_or_default();
            assert!(
                !features
                    .iter()
                    .any(|feature| feature.as_str() == Some("test-support")),
                "{name} enables test-support of {dependency} outside dev-dependencies"
            );
        }
    }
}

#[test]
fn every_crate_uses_the_workspace_lints() {
    for name in CRATES {
        let manifest = manifest(name);
        assert_eq!(
            manifest
                .get("lints")
                .and_then(|lints| lints.get("workspace")),
            Some(&toml::Value::Boolean(true)),
            "{name} must set [lints] workspace = true"
        );
    }
}

fn rust_sources(directory: &Path, sources: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_sources(&path, sources);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            sources.push(path);
        }
    }
}

fn sources_of(name: &str) -> Vec<(PathBuf, String)> {
    let mut sources = Vec::new();
    rust_sources(
        &workspace_root().join("crates").join(name).join("src"),
        &mut sources,
    );
    sources
        .into_iter()
        .map(|path| (path.clone(), std::fs::read_to_string(path).unwrap()))
        .collect()
}

#[test]
fn every_crate_forbids_unsafe_code() {
    for name in CRATES {
        let root = if name == "testry-cli" {
            "main.rs"
        } else {
            "lib.rs"
        };
        let path = workspace_root()
            .join("crates")
            .join(name)
            .join("src")
            .join(root);
        let source = std::fs::read_to_string(&path).unwrap();
        assert!(
            source.contains("#![forbid(unsafe_code)]"),
            "{} must forbid unsafe code",
            path.display()
        );
    }
}

#[test]
fn domain_and_application_perform_no_io() {
    let forbidden = [
        "std::fs",
        "std::process",
        "std::net",
        "std::env",
        "tokio::fs",
        "tokio::process",
    ];
    for name in ["testry-domain", "testry-application"] {
        for (path, source) in sources_of(name) {
            for token in forbidden {
                assert!(
                    !source.contains(token),
                    "{} uses {token}; IO belongs behind a port in testry-adapters",
                    path.display()
                );
            }
        }
    }
}

#[test]
fn domain_has_no_async_runtime_in_its_sources() {
    for (path, source) in sources_of("testry-domain") {
        assert!(
            !source.contains("tokio"),
            "{} mentions tokio",
            path.display()
        );
        assert!(
            !source.contains("async fn"),
            "{} declares an async fn",
            path.display()
        );
    }
}
