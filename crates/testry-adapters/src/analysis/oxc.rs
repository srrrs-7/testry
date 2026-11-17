//! Dependency analysis with the Oxc parser and resolver (spec section 7).
//!
//! The graph is built for one side of a snapshot (baseline or current) under
//! one resolver profile. Nothing is executed and the real `node_modules` are
//! not consulted.
//!
//! Safety rules implemented here:
//! - every specifier that does not resolve to a repository file, a runtime
//!   builtin or a declared external package becomes a diagnostic;
//! - a bare specifier is resolved under every condition set of the profile
//!   and all results are kept (a union can only widen a selection);
//! - when several sibling files could satisfy a specifier (`foo.ts` next to
//!   `foo.js`), all of them become dependencies, so the result does not depend
//!   on the extension order of a particular runner;
//! - a resolution that touches a symbolic link or a submodule is a diagnostic.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;
use std::sync::{Arc, Mutex};

use oxc_resolver::{ResolveError, ResolveOptions, ResolverGeneric, TsconfigDiscovery};
use testry_application::ports::planning::{
    AnalysisProfile, DependencyAnalyzer, ResolverProfile, SnapshotSide,
};
use testry_application::ports::{Cancellation, OperationContext, PortError, PortFuture};
use testry_domain::fingerprint::Fingerprint;
use testry_domain::graph::{DependencyGraph, GraphDiagnostic, GraphDiagnosticKind};
use testry_domain::paths::RepoPath;
use testry_domain::planning::is_module_path;
use testry_domain::snapshot::{FileKind, SourceSnapshot};

use super::extract::{ModuleRequests, extract};
use super::vfs::{Loader, SnapshotFs, Tree, lock, virtual_path};
use crate::source::reader::{SourceContent, SourceReader};

/// Bumped whenever extraction or resolution rules change, so cached results
/// of an older rule set are never reused.
const ANALYSIS_VERSION: &str = "oxc-0.159.0/resolver-11.24.3/rules-1";

/// Extensions tried for an extensionless specifier. The order does not decide
/// which file wins: sibling candidates are all recorded.
const EXTENSIONS: [&str; 12] = [
    ".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs", ".json", ".d.ts", ".d.mts",
    ".d.cts",
];

/// TypeScript sources are imported with JavaScript extensions.
const EXTENSION_ALIASES: [(&str, &[&str]); 4] = [
    (".js", &[".js", ".ts", ".tsx", ".jsx", ".d.ts"]),
    (".jsx", &[".jsx", ".tsx"]),
    (".mjs", &[".mjs", ".mts", ".d.mts"]),
    (".cjs", &[".cjs", ".cts", ".d.cts"]),
];

/// Suffixes that are stripped to find sibling candidates, longest first.
const CANDIDATE_SUFFIXES: [&str; 12] = [
    ".d.mts", ".d.cts", ".d.ts", ".json", ".tsx", ".mts", ".cts", ".jsx", ".mjs", ".cjs", ".ts",
    ".js",
];

const MAX_DETAIL_CHARS: usize = 200;

struct Variant {
    conditions: &'static [&'static str],
    main_fields: &'static [&'static str],
}

/// The condition sets a runner family may resolve package `exports` with.
/// Which one applies depends on details testry cannot see statically (import
/// versus require, browser versus server), so all of them are resolved.
fn variants(profile: ResolverProfile) -> &'static [Variant] {
    match profile {
        ResolverProfile::Bun => &[
            Variant {
                conditions: &["bun", "node", "import", "default"],
                main_fields: &["module", "main"],
            },
            Variant {
                conditions: &["bun", "import", "default"],
                main_fields: &["module", "main"],
            },
            Variant {
                conditions: &["bun", "node", "require", "default"],
                main_fields: &["main", "module"],
            },
            Variant {
                conditions: &["bun", "require", "default"],
                main_fields: &["main", "module"],
            },
        ],
        ResolverProfile::Vite => &[
            Variant {
                conditions: &["module", "node", "development", "import", "default"],
                main_fields: &["module", "jsnext:main", "jsnext", "main"],
            },
            Variant {
                conditions: &["module", "browser", "development", "import", "default"],
                main_fields: &["browser", "module", "jsnext:main", "jsnext", "main"],
            },
            Variant {
                conditions: &["module", "node", "development", "require", "default"],
                main_fields: &["module", "jsnext:main", "jsnext", "main"],
            },
            Variant {
                conditions: &["module", "browser", "development", "require", "default"],
                main_fields: &["browser", "module", "jsnext:main", "jsnext", "main"],
            },
        ],
        ResolverProfile::Node => &[
            Variant {
                conditions: &["node", "import", "default"],
                main_fields: &["main"],
            },
            Variant {
                conditions: &["node", "require", "default"],
                main_fields: &["main"],
            },
        ],
    }
}

fn resolve_options(variant: &Variant) -> ResolveOptions {
    let owned = |items: &[&str]| {
        items
            .iter()
            .map(|item| (*item).to_owned())
            .collect::<Vec<_>>()
    };
    ResolveOptions {
        tsconfig: Some(TsconfigDiscovery::Auto),
        condition_names: owned(variant.conditions),
        extensions: owned(&EXTENSIONS),
        extension_alias: EXTENSION_ALIASES
            .iter()
            .map(|(extension, aliases)| ((*extension).to_owned(), owned(aliases)))
            .collect(),
        main_fields: owned(variant.main_fields),
        // No symbolic link is exposed by the snapshot file system.
        symlinks: false,
        // NODE_PATH is machine state, not part of the snapshot.
        node_path: false,
        builtin_modules: true,
        ..ResolveOptions::default()
    }
}

/// How the bytes of a file of the analyzed side are obtained.
#[derive(Debug, Clone)]
enum Content {
    /// From the working tree, verified against this digest.
    Current(Fingerprint),
    /// From the Git object database.
    Baseline(String),
}

#[derive(Debug, Clone)]
struct SideFile {
    is_regular: bool,
    content: Content,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ParseKey {
    content: String,
    /// The source type follows from the extension.
    extension: String,
}

type ParseCache = Mutex<HashMap<ParseKey, Arc<ModuleRequests>>>;

/// Builds dependency graphs from snapshots. Parse results are reused across
/// sides and profiles by content digest, extension and analysis version.
pub struct OxcDependencyAnalyzer {
    reader: Arc<dyn SourceReader>,
    parsed: Arc<ParseCache>,
}

impl OxcDependencyAnalyzer {
    pub fn new(reader: Arc<dyn SourceReader>) -> Self {
        Self {
            reader,
            parsed: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl DependencyAnalyzer for OxcDependencyAnalyzer {
    fn analyze<'a>(
        &'a self,
        snapshot: &'a SourceSnapshot,
        profile: &'a AnalysisProfile,
        ctx: &'a OperationContext,
    ) -> PortFuture<'a, DependencyGraph> {
        Box::pin(async move {
            ctx.ensure_active("dependency analysis")?;
            let job = Job {
                reader: Arc::clone(&self.reader),
                parsed: Arc::clone(&self.parsed),
                cancellation: Arc::clone(&ctx.cancellation),
                files: Arc::new(side_files(snapshot, profile.side)?),
                profile: *profile,
            };
            // Parsing and resolving are CPU bound; keep them off the async executor.
            tokio::task::spawn_blocking(move || job.run())
                .await
                .map_err(|error| PortError::Storage {
                    message: format!("the analysis worker failed: {error}"),
                })?
        })
    }
}

fn side_files(
    snapshot: &SourceSnapshot,
    side: SnapshotSide,
) -> Result<BTreeMap<RepoPath, SideFile>, PortError> {
    match side {
        SnapshotSide::Current => Ok(snapshot
            .current()
            .iter()
            .map(|(path, file)| {
                let file = SideFile {
                    is_regular: file.kind == FileKind::File,
                    content: Content::Current(file.digest),
                };
                (path.clone(), file)
            })
            .collect()),
        SnapshotSide::Baseline => {
            if snapshot.baseline().is_none() {
                return Err(PortError::Configuration {
                    message: "the snapshot has no baseline to analyze".to_owned(),
                });
            }
            Ok(snapshot
                .baseline_files()
                .iter()
                .map(|(path, file)| {
                    // An unchanged file has the same content in the working
                    // tree, so it is read (and its parse result shared) there.
                    let unchanged = snapshot.current().get(path).filter(|current| {
                        !snapshot.is_changed(path) && current.kind == FileKind::File
                    });
                    let content = match unchanged {
                        Some(current) => Content::Current(current.digest),
                        None => Content::Baseline(file.object_id.clone()),
                    };
                    (
                        path.clone(),
                        SideFile {
                            is_regular: file.kind == FileKind::File,
                            content,
                        },
                    )
                })
                .collect())
        }
    }
}

struct Job {
    reader: Arc<dyn SourceReader>,
    parsed: Arc<ParseCache>,
    cancellation: Arc<dyn Cancellation>,
    files: Arc<BTreeMap<RepoPath, SideFile>>,
    profile: AnalysisProfile,
}

#[derive(Default)]
struct Packages {
    /// Package name -> directory, for names defined exactly once.
    directories: BTreeMap<String, String>,
    /// Names defined by more than one `package.json`. They are not linked.
    ambiguous: BTreeSet<String>,
    /// Every dependency name declared anywhere in the repository.
    declared: BTreeSet<String>,
}

impl Packages {
    fn is_workspace_package(&self, name: &str) -> bool {
        self.directories.contains_key(name) || self.ambiguous.contains(name)
    }

    /// Whether `name` is installed from outside the repository. Its content
    /// is covered by the lockfile, which is a global input.
    fn is_external(&self, name: &str) -> bool {
        if self.is_workspace_package(name) {
            return false;
        }
        let types = format!("@types/{}", name.trim_start_matches('@').replace('/', "__"));
        self.declared.contains(name) || self.declared.contains(&types)
    }
}

#[derive(Default)]
struct Findings {
    edges: Vec<(RepoPath, RepoPath)>,
    diagnostics: Vec<GraphDiagnostic>,
}

impl Findings {
    fn diagnostic(&mut self, kind: GraphDiagnosticKind, file: &RepoPath, detail: &str) {
        self.diagnostics.push(GraphDiagnostic {
            kind,
            file: file.clone(),
            detail: detail.chars().take(MAX_DETAIL_CHARS).collect(),
        });
    }

    fn merge(&mut self, other: Self) {
        self.edges.extend(other.edges);
        self.diagnostics.extend(other.diagnostics);
    }
}

struct Resolution<'a> {
    fs: &'a SnapshotFs,
    resolvers: &'a [ResolverGeneric<SnapshotFs>],
    packages: &'a Packages,
}

impl Job {
    fn run(self) -> Result<DependencyGraph, PortError> {
        let mut findings = Findings::default();
        let packages = self.read_packages(&mut findings)?;

        let modules: Vec<&RepoPath> = self
            .files
            .iter()
            .filter(|(path, file)| file.is_regular && is_module_path(path))
            .map(|(path, _)| path)
            .collect();
        let requests = self.in_parallel(&modules, |path: &&RepoPath| {
            self.requests_of(path)
                .map(|requests| ((*path).clone(), requests))
        })?;

        // A file that changes while it is read must fail the analysis; the
        // resolver would otherwise treat it as missing.
        let load_failure: Arc<Mutex<Option<PortError>>> = Arc::new(Mutex::new(None));
        let tree = Arc::new(Tree::new(
            self.files
                .iter()
                .map(|(path, file)| (path.as_str().to_owned(), file.is_regular)),
            packages.directories.clone(),
        ));
        let fs = SnapshotFs::over(Arc::clone(&tree), self.loader(Arc::clone(&load_failure)));
        let resolvers: Vec<ResolverGeneric<SnapshotFs>> = variants(self.profile.resolver)
            .iter()
            .map(|variant| {
                ResolverGeneric::new_with_file_system(fs.clone(), resolve_options(variant))
            })
            .collect();
        let resolution = Resolution {
            fs: &fs,
            resolvers: &resolvers,
            packages: &packages,
        };

        let resolved = self.in_parallel(&requests, |(path, requests)| {
            let mut findings = Findings::default();
            for (kind, detail) in &requests.problems {
                findings.diagnostic(*kind, path, detail);
            }
            for specifier in &requests.specifiers {
                resolve_specifier(&resolution, path, specifier, &mut findings);
            }
            Ok(findings)
        })?;
        for part in resolved {
            findings.merge(part);
        }
        if let Some(error) = lock(&load_failure).take() {
            return Err(error);
        }

        for probe in fs.probes() {
            if let Ok(path) = RepoPath::try_new(&probe) {
                findings.diagnostic(
                    GraphDiagnosticKind::UnanalyzedPath,
                    &path,
                    "module resolution reached a symbolic link or submodule, which is not followed",
                );
            }
        }

        let mut graph = DependencyGraph::builder();
        graph.profile(self.profile.resolver.name());
        for module in modules {
            graph.node(module.clone());
        }
        for (consumer, dependency) in findings.edges {
            graph.edge(consumer, dependency);
        }
        for diagnostic in findings.diagnostics {
            graph.diagnostic(diagnostic);
        }
        Ok(graph.build())
    }

    fn ensure_active(&self) -> Result<(), PortError> {
        if self.cancellation.is_cancelled() {
            Err(PortError::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Applies `work` to every item on a bounded number of threads and keeps
    /// the input order.
    fn in_parallel<T, R, F>(&self, items: &[T], work: F) -> Result<Vec<R>, PortError>
    where
        T: Sync,
        R: Send,
        F: Fn(&T) -> Result<R, PortError> + Sync,
    {
        let workers = std::thread::available_parallelism()
            .map_or(1, usize::from)
            .clamp(1, 8);
        let chunk_size = items.len().div_ceil(workers).max(1);
        let work = &work;
        let chunks: Vec<Result<Vec<R>, PortError>> = std::thread::scope(|scope| {
            let handles: Vec<_> = items
                .chunks(chunk_size)
                .map(|chunk| {
                    scope.spawn(move || {
                        let mut results = Vec::with_capacity(chunk.len());
                        for item in chunk {
                            self.ensure_active()?;
                            results.push(work(item)?);
                        }
                        Ok(results)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| {
                    handle.join().unwrap_or_else(|_| {
                        Err(PortError::Storage {
                            message: "an analysis worker stopped unexpectedly".to_owned(),
                        })
                    })
                })
                .collect()
        });
        let mut results = Vec::with_capacity(items.len());
        for chunk in chunks {
            results.extend(chunk?);
        }
        Ok(results)
    }

    fn read(&self, path: &RepoPath, file: &SideFile) -> Result<SourceContent, PortError> {
        let limit = self.profile.limits.max_source_bytes;
        match &file.content {
            Content::Current(digest) => self.reader.read_current(path, digest, limit),
            Content::Baseline(object_id) => self.reader.read_baseline(path, object_id, limit),
        }
    }

    fn requests_of(&self, path: &RepoPath) -> Result<Arc<ModuleRequests>, PortError> {
        let Some(file) = self.files.get(path) else {
            return Ok(Arc::new(ModuleRequests::default()));
        };
        let content = match &file.content {
            Content::Current(digest) => digest.to_string(),
            Content::Baseline(object_id) => format!("git:{object_id}"),
        };
        let key = ParseKey {
            content: format!("{ANALYSIS_VERSION}:{content}"),
            extension: path.extension().unwrap_or_default().to_owned(),
        };
        if let Some(hit) = lock(&self.parsed).get(&key) {
            return Ok(Arc::clone(hit));
        }
        let requests = Arc::new(match self.read(path, file)? {
            SourceContent::Bytes(bytes) => extract(path, &bytes),
            SourceContent::TooLarge { .. } => ModuleRequests::problem(
                GraphDiagnosticKind::ResourceLimit,
                "the file is larger than the analysis limit and was not parsed",
            ),
        });
        lock(&self.parsed).insert(key, Arc::clone(&requests));
        Ok(requests)
    }

    /// Reads every `package.json`: workspace package names and the declared
    /// dependency names.
    fn read_packages(&self, findings: &mut Findings) -> Result<Packages, PortError> {
        let mut packages = Packages::default();
        for (path, file) in self.files.iter() {
            if !file.is_regular || path.file_name() != "package.json" {
                continue;
            }
            self.ensure_active()?;
            let bytes = match self.read(path, file)? {
                SourceContent::Bytes(bytes) => bytes,
                SourceContent::TooLarge { .. } => {
                    findings.diagnostic(
                        GraphDiagnosticKind::ResourceLimit,
                        path,
                        "package.json is larger than the analysis limit",
                    );
                    continue;
                }
            };
            let Ok(manifest) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                findings.diagnostic(
                    GraphDiagnosticKind::ParseFailure,
                    path,
                    "package.json is not valid JSON",
                );
                continue;
            };
            for field in [
                "dependencies",
                "devDependencies",
                "peerDependencies",
                "optionalDependencies",
            ] {
                if let Some(dependencies) =
                    manifest.get(field).and_then(serde_json::Value::as_object)
                {
                    packages.declared.extend(dependencies.keys().cloned());
                }
            }
            if let Some(name) = manifest.get("name").and_then(serde_json::Value::as_str) {
                let directory = path
                    .parent()
                    .as_path()
                    .map_or("", RepoPath::as_str)
                    .to_owned();
                if packages.ambiguous.contains(name) {
                    continue;
                }
                if packages.directories.remove(name).is_some() {
                    packages.ambiguous.insert(name.to_owned());
                } else {
                    packages.directories.insert(name.to_owned(), directory);
                }
            }
        }
        Ok(packages)
    }

    /// Serves `package.json` and `tsconfig` contents to the resolver.
    fn loader(&self, failure: Arc<Mutex<Option<PortError>>>) -> Arc<Loader> {
        let files = Arc::clone(&self.files);
        let reader = Arc::clone(&self.reader);
        let limit = self.profile.limits.max_source_bytes;
        Arc::new(move |path: &str| {
            let missing = || io::Error::from(io::ErrorKind::NotFound);
            let path = RepoPath::try_new(path).map_err(|_| missing())?;
            let file = files.get(&path).ok_or_else(missing)?;
            let content = match &file.content {
                Content::Current(digest) => reader.read_current(&path, digest, limit),
                Content::Baseline(object_id) => reader.read_baseline(&path, object_id, limit),
            };
            match content {
                Ok(SourceContent::Bytes(bytes)) => Ok(bytes),
                Ok(SourceContent::TooLarge { .. }) => {
                    lock(&failure).get_or_insert(PortError::Configuration {
                        message: format!(
                            "{path} is needed for module resolution but is larger than the analysis limit"
                        ),
                    });
                    Err(io::Error::from(io::ErrorKind::InvalidData))
                }
                Err(error) => {
                    lock(&failure).get_or_insert(error);
                    Err(io::Error::from(io::ErrorKind::Other))
                }
            }
        })
    }
}

/// Runtime modules that never map to a repository file.
fn is_runtime_builtin(specifier: &str) -> bool {
    specifier == "bun" || specifier.starts_with("bun:") || specifier.starts_with("node:")
}

fn is_path_specifier(specifier: &str) -> bool {
    specifier == "."
        || specifier == ".."
        || specifier.starts_with("./")
        || specifier.starts_with("../")
        || specifier.starts_with('/')
}

/// The package a bare specifier refers to, e.g. `@scope/name` of
/// `@scope/name/sub/path`. `None` for paths, `#imports` and URL-like
/// specifiers such as `virtual:x`.
fn package_name(specifier: &str) -> Option<&str> {
    if is_path_specifier(specifier) || specifier.starts_with('#') || specifier.contains(':') {
        return None;
    }
    let mut segments = specifier.split('/');
    let first = segments.next().filter(|segment| !segment.is_empty())?;
    if !first.starts_with('@') {
        return Some(first);
    }
    let second = segments.next().filter(|segment| !segment.is_empty())?;
    specifier.get(..first.len() + 1 + second.len())
}

fn resolve_specifier(
    resolution: &Resolution<'_>,
    consumer: &RepoPath,
    specifier: &str,
    findings: &mut Findings,
) {
    if is_runtime_builtin(specifier) {
        return;
    }
    let file = virtual_path(consumer.as_str());
    // Export conditions only matter for package specifiers.
    let resolvers = if is_path_specifier(specifier) {
        resolution.resolvers.get(..1).unwrap_or_default()
    } else {
        resolution.resolvers
    };
    let mut targets = BTreeSet::new();
    let mut problems = BTreeSet::new();
    for resolver in resolvers {
        match resolver.resolve_file(&file, specifier) {
            Ok(resolved) => match resolution.fs.repository_path(resolved.path()) {
                Some(path) if resolution.fs.tree().is_file(&path) => {
                    targets.insert(path);
                }
                _ => {
                    problems.insert((
                        GraphDiagnosticKind::OutsideRepository,
                        consumer.clone(),
                        specifier.to_owned(),
                    ));
                }
            },
            Err(ResolveError::Builtin { .. }) => {}
            Err(error) => {
                if let Some(problem) = classify_failure(resolution, consumer, specifier, &error) {
                    problems.insert(problem);
                }
            }
        }
    }
    for (kind, file, detail) in problems {
        findings.diagnostic(kind, &file, &detail);
    }
    for target in targets {
        for candidate in sibling_candidates(resolution.fs.tree(), specifier, &target) {
            if let Ok(dependency) = RepoPath::try_new(&candidate) {
                findings.edges.push((consumer.clone(), dependency));
            }
        }
    }
}

/// Turns a failed resolution into a diagnostic, or `None` for a declared
/// external package.
fn classify_failure(
    resolution: &Resolution<'_>,
    consumer: &RepoPath,
    specifier: &str,
    error: &ResolveError,
) -> Option<(GraphDiagnosticKind, RepoPath, String)> {
    let tsconfig = match error {
        ResolveError::TsconfigNotFound(path)
        | ResolveError::TsconfigSelfReference(path)
        | ResolveError::TsconfigLoadFailed { path, .. } => Some(Some(path.as_path())),
        ResolveError::TsconfigCircularExtend(_) => Some(None),
        _ => None,
    };
    if let Some(tsconfig) = tsconfig {
        // One diagnostic per tsconfig, not one per import that uses it. The
        // error may name a file outside the repository (the extended
        // config); the tsconfig that governs the consumer is the one to fix.
        let tree = resolution.fs.tree();
        let file = tsconfig
            .and_then(|path| resolution.fs.repository_path(path))
            .filter(|path| tree.is_file(path))
            .or_else(|| nearest_tsconfig(tree, consumer))
            .and_then(|path| RepoPath::try_new(path).ok())
            .unwrap_or_else(|| consumer.clone());
        return Some((
            GraphDiagnosticKind::UnsupportedMechanism,
            file,
            "a tsconfig could not be loaded (for example it extends a file outside the repository), \
             so path aliases are unknown"
                .to_owned(),
        ));
    }
    if package_name(specifier).is_some_and(|name| resolution.packages.is_external(name)) {
        return None;
    }
    Some((
        GraphDiagnosticKind::UnresolvedImport,
        consumer.clone(),
        specifier.to_owned(),
    ))
}

/// The `tsconfig.json` closest to `file`, walking up to the repository root.
fn nearest_tsconfig(tree: &Tree, file: &RepoPath) -> Option<String> {
    let mut directory = file.parent();
    loop {
        let candidate = match directory.as_path() {
            Some(path) => format!("{path}/tsconfig.json"),
            None => "tsconfig.json".to_owned(),
        };
        if tree.is_file(&candidate) {
            return Some(candidate);
        }
        directory = directory.as_path()?.parent();
    }
}

/// The resolved file plus every sibling that the same specifier could resolve
/// to under another extension order: `foo.ts` / `foo.js` / `foo.d.ts`,
/// `foo/index.*`, and `foo.*` next to `foo/index.*`.
fn sibling_candidates(tree: &Tree, specifier: &str, resolved: &str) -> BTreeSet<String> {
    let mut candidates = BTreeSet::from([resolved.to_owned()]);
    let file_name = resolved.rsplit('/').next().unwrap_or(resolved);
    let request = specifier.split(['?', '#']).next().unwrap_or(specifier);
    if request.rsplit('/').next() == Some(file_name) {
        // The specifier names the file exactly. Only a JavaScript extension
        // is still ambiguous: `./x.js` may mean `x.ts` to a TypeScript-aware
        // runner even when `x.js` exists.
        for (extension, aliases) in EXTENSION_ALIASES {
            let Some(base) = resolved.strip_suffix(extension) else {
                continue;
            };
            for alias in aliases {
                let candidate = format!("{base}{alias}");
                if tree.is_file(&candidate) {
                    candidates.insert(candidate);
                }
            }
        }
        return candidates;
    }
    let Some(base) = CANDIDATE_SUFFIXES
        .iter()
        .find_map(|suffix| resolved.strip_suffix(suffix))
    else {
        return candidates;
    };
    let mut bases = vec![base.to_owned(), format!("{base}/index")];
    if let Some(directory) = base.strip_suffix("/index") {
        bases.push(directory.to_owned());
    }
    for base in bases {
        for suffix in CANDIDATE_SUFFIXES {
            let candidate = format!("{base}{suffix}");
            if tree.is_file(&candidate) {
                candidates.insert(candidate);
            }
        }
    }
    candidates
}

#[cfg(test)]
mod tests;
