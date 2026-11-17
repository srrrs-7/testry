//! A read-only, in-memory file system over one side of a snapshot.
//!
//! The resolver only ever sees this view, never the real disk:
//! - resolution works for the baseline commit without checking it out;
//! - resolution uses exactly the files of the snapshot, so a file that
//!   appears later cannot leak into a plan;
//! - nothing outside the repository can be reached, whatever a specifier or a
//!   `tsconfig.json` says.
//!
//! Symbolic links and submodules are *opaque*: they look like missing paths,
//! and every attempt to touch one is recorded. The analyzer turns those
//! records into diagnostics, so a resolution that depends on a link widens
//! the selection instead of silently resolving somewhere else.
//!
//! Workspace packages are reachable as `node_modules/<name>` at the root,
//! mirroring what a package manager links. Real `node_modules` are not part
//! of a snapshot and are not needed: external packages are classified by the
//! analyzer from the declared dependencies.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use oxc_resolver::{FileMetadata, FileSystem, ResolveError};

/// The absolute path under which the snapshot is presented. It does not exist
/// on disk and is never opened.
pub(crate) const VIRTUAL_ROOT: &str = "/testry-snapshot-root";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Entry {
    File,
    Directory,
}

/// Loads the bytes of a regular file by its repository-relative path.
pub(crate) type Loader = dyn Fn(&str) -> io::Result<Vec<u8>> + Send + Sync;

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Debug, Default)]
pub(crate) struct Tree {
    /// Repository-relative path -> entry. The root directory is `""`.
    entries: BTreeMap<String, Entry>,
    /// Symbolic links and submodules.
    opaque: Vec<String>,
    /// Workspace package name -> package directory (`""` for the root).
    packages: BTreeMap<String, String>,
    /// `@scope` prefixes of the workspace package names.
    scopes: BTreeSet<String>,
}

enum Mirror {
    /// `node_modules` itself or a scope directory inside it.
    VirtualDirectory,
    /// The path inside the repository that the given path stands for.
    Real(String),
}

impl Tree {
    /// `files` yields `(path, is_regular_file)`; everything that is not a
    /// regular file is opaque.
    pub(crate) fn new(
        files: impl IntoIterator<Item = (String, bool)>,
        packages: BTreeMap<String, String>,
    ) -> Self {
        let mut entries = BTreeMap::from([(String::new(), Entry::Directory)]);
        let mut opaque = Vec::new();
        for (path, is_regular) in files {
            for (index, _) in path.match_indices('/') {
                if let Some(directory) = path.get(..index) {
                    entries
                        .entry(directory.to_owned())
                        .or_insert(Entry::Directory);
                }
            }
            if is_regular {
                entries.insert(path, Entry::File);
            } else {
                opaque.push(path);
            }
        }
        let scopes = packages
            .keys()
            .filter(|name| name.starts_with('@'))
            .filter_map(|name| name.split('/').next())
            .map(str::to_owned)
            .collect();
        Self {
            entries,
            opaque,
            packages,
            scopes,
        }
    }

    pub(crate) fn is_file(&self, path: &str) -> bool {
        self.entries.get(path) == Some(&Entry::File)
    }

    fn mirror(&self, path: &str) -> Mirror {
        let real = || Mirror::Real(path.to_owned());
        let Some(rest) = path.strip_prefix("node_modules") else {
            return real();
        };
        if rest.is_empty() {
            return if self.packages.is_empty() {
                real()
            } else {
                Mirror::VirtualDirectory
            };
        }
        let Some(rest) = rest.strip_prefix('/') else {
            return real();
        };
        let (name, remainder) = match rest.split_once('/') {
            Some((scope, scoped)) if scope.starts_with('@') => match scoped.split_once('/') {
                Some((package, remainder)) => (format!("{scope}/{package}"), Some(remainder)),
                None => (format!("{scope}/{scoped}"), None),
            },
            Some((package, remainder)) => (package.to_owned(), Some(remainder)),
            None if rest.starts_with('@') => {
                return if self.scopes.contains(rest) {
                    Mirror::VirtualDirectory
                } else {
                    real()
                };
            }
            None => (rest.to_owned(), None),
        };
        match (self.packages.get(&name), remainder) {
            (Some(directory), None) => Mirror::Real(directory.clone()),
            (Some(directory), Some(remainder)) if directory.is_empty() => {
                Mirror::Real(remainder.to_owned())
            }
            (Some(directory), Some(remainder)) => Mirror::Real(format!("{directory}/{remainder}")),
            (None, _) => real(),
        }
    }

    /// The opaque entry that `path` is, or lies below.
    fn opaque_ancestor(&self, path: &str) -> Option<&str> {
        self.opaque.iter().map(String::as_str).find(|opaque| {
            path.strip_prefix(opaque)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        })
    }
}

/// Converts an absolute virtual path into a repository-relative one. `None`
/// when the path is outside the virtual root or not valid UTF-8.
fn relative_path(path: &Path) -> Option<String> {
    let relative = path.strip_prefix(VIRTUAL_ROOT).ok()?;
    let mut segments: Vec<&str> = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(segment) => segments.push(segment.to_str()?),
            Component::CurDir => {}
            Component::ParentDir => {
                segments.pop()?;
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(segments.join("/"))
}

/// The virtual absolute path of a repository-relative path.
pub(crate) fn virtual_path(path: &str) -> PathBuf {
    Path::new(VIRTUAL_ROOT).join(path)
}

#[derive(Clone)]
pub(crate) struct SnapshotFs {
    tree: Arc<Tree>,
    loader: Option<Arc<Loader>>,
    /// Opaque paths that resolution tried to use.
    probes: Arc<Mutex<BTreeSet<String>>>,
}

impl SnapshotFs {
    pub(crate) fn over(tree: Arc<Tree>, loader: Arc<Loader>) -> Self {
        Self {
            tree,
            loader: Some(loader),
            probes: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }

    pub(crate) fn tree(&self) -> &Tree {
        &self.tree
    }

    /// Opaque paths touched so far.
    pub(crate) fn probes(&self) -> BTreeSet<String> {
        lock(&self.probes).clone()
    }

    /// The repository path a resolved virtual path stands for.
    pub(crate) fn repository_path(&self, path: &Path) -> Option<String> {
        match self.tree.mirror(&relative_path(path)?) {
            Mirror::Real(real) => Some(real),
            Mirror::VirtualDirectory => None,
        }
    }

    fn locate(&self, path: &Path) -> Option<(String, Entry)> {
        let relative = relative_path(path)?;
        let real = match self.tree.mirror(&relative) {
            Mirror::VirtualDirectory => return Some((relative, Entry::Directory)),
            Mirror::Real(real) => real,
        };
        if let Some(opaque) = self.tree.opaque_ancestor(&real) {
            lock(&self.probes).insert(opaque.to_owned());
            return None;
        }
        let entry = *self.tree.entries.get(&real)?;
        Some((real, entry))
    }

    fn not_found() -> io::Error {
        io::Error::from(io::ErrorKind::NotFound)
    }
}

impl FileSystem for SnapshotFs {
    fn new() -> Self {
        Self {
            tree: Arc::new(Tree::default()),
            loader: None,
            probes: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        match (self.locate(path), &self.loader) {
            (Some((real, Entry::File)), Some(loader)) => loader(&real),
            _ => Err(Self::not_found()),
        }
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        String::from_utf8(self.read(path)?).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))
    }

    fn metadata(&self, path: &Path) -> io::Result<FileMetadata> {
        match self.locate(path) {
            Some((_, Entry::File)) => Ok(FileMetadata::new(true, false, false)),
            Some((_, Entry::Directory)) => Ok(FileMetadata::new(false, true, false)),
            None => Err(Self::not_found()),
        }
    }

    /// No symbolic link is ever exposed, so this equals [`Self::metadata`].
    fn symlink_metadata(&self, path: &Path) -> io::Result<FileMetadata> {
        self.metadata(path)
    }

    fn read_link(&self, _path: &Path) -> Result<PathBuf, ResolveError> {
        Err(io::Error::from(io::ErrorKind::InvalidInput).into())
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        self.metadata(path)?;
        Ok(path.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(files: &[&str], opaque: &[&str], packages: &[(&str, &str)]) -> Arc<Tree> {
        let entries = files
            .iter()
            .map(|path| ((*path).to_owned(), true))
            .chain(opaque.iter().map(|path| ((*path).to_owned(), false)));
        let packages = packages
            .iter()
            .map(|(name, dir)| ((*name).to_owned(), (*dir).to_owned()))
            .collect();
        Arc::new(Tree::new(entries, packages))
    }

    fn fs(tree: Arc<Tree>) -> SnapshotFs {
        SnapshotFs::over(tree, Arc::new(|path: &str| Ok(path.as_bytes().to_vec())))
    }

    fn is_file(fs: &SnapshotFs, path: &str) -> bool {
        fs.metadata(&virtual_path(path))
            .is_ok_and(FileMetadata::is_file)
    }

    fn is_dir(fs: &SnapshotFs, path: &str) -> bool {
        fs.metadata(&virtual_path(path))
            .is_ok_and(FileMetadata::is_dir)
    }

    #[test]
    fn exposes_files_and_their_directories() {
        let fs = fs(tree(&["apps/api/src/user.ts", "package.json"], &[], &[]));
        assert!(is_file(&fs, "apps/api/src/user.ts"));
        assert!(is_file(&fs, "package.json"));
        assert!(is_dir(&fs, "apps"));
        assert!(is_dir(&fs, "apps/api/src"));
        assert!(is_dir(&fs, ""));
        assert!(!is_file(&fs, "apps/api/src/missing.ts"));
        assert!(!is_dir(&fs, "apps/api/src/user.ts"));
        assert_eq!(
            fs.read(&virtual_path("package.json")).unwrap(),
            b"package.json"
        );
        assert!(fs.read(&virtual_path("apps")).is_err());
    }

    #[test]
    fn nothing_outside_the_virtual_root_exists() {
        let fs = fs(tree(&["a.ts"], &[], &[]));
        for path in [
            "/",
            "/etc/passwd",
            "/testry-snapshot-root/../etc/passwd",
            "/tmp",
            "relative.ts",
        ] {
            assert!(fs.metadata(Path::new(path)).is_err(), "{path}");
            assert!(fs.read(Path::new(path)).is_err(), "{path}");
        }
        assert!(is_file(&fs, "x/../a.ts"));
        assert!(is_file(&fs, "./a.ts"));
    }

    /// G17: symbolic links and submodules are never followed; touching one is recorded.
    #[test]
    fn g17_opaque_paths_look_missing_and_are_recorded() {
        let fs = fs(tree(&["src/a.ts"], &["src/link.ts", "vendor/sub"], &[]));
        assert!(fs.probes().is_empty());
        assert!(fs.metadata(&virtual_path("src/link.ts")).is_err());
        assert!(
            fs.metadata(&virtual_path("vendor/sub/lib/index.ts"))
                .is_err()
        );
        assert!(fs.read(&virtual_path("vendor/sub/package.json")).is_err());
        assert_eq!(
            fs.probes(),
            BTreeSet::from(["src/link.ts".to_owned(), "vendor/sub".to_owned()])
        );
        // A path that merely shares a prefix is not below the opaque entry.
        assert!(fs.metadata(&virtual_path("vendor/submarine.ts")).is_err());
        assert_eq!(fs.probes().len(), 2);
    }

    #[test]
    fn workspace_packages_are_mirrored_under_node_modules() {
        let fs = fs(tree(
            &[
                "packages/shared/package.json",
                "packages/shared/src/index.ts",
                "packages/ui/index.ts",
                "package.json",
                "src/main.ts",
            ],
            &[],
            &[
                ("@app/shared", "packages/shared"),
                ("ui", "packages/ui"),
                ("root-app", ""),
            ],
        ));
        assert!(is_dir(&fs, "node_modules"));
        assert!(is_dir(&fs, "node_modules/@app"));
        assert!(is_dir(&fs, "node_modules/@app/shared"));
        assert!(is_file(&fs, "node_modules/@app/shared/package.json"));
        assert!(is_file(&fs, "node_modules/@app/shared/src/index.ts"));
        assert!(is_file(&fs, "node_modules/ui/index.ts"));
        assert!(is_file(&fs, "node_modules/root-app/src/main.ts"));
        assert!(!is_dir(&fs, "node_modules/@other"));
        assert!(!is_dir(&fs, "node_modules/react"));
        assert!(!is_file(&fs, "node_modules/@app/shared/src/missing.ts"));
        assert_eq!(
            fs.repository_path(&virtual_path("node_modules/@app/shared/src/index.ts"))
                .as_deref(),
            Some("packages/shared/src/index.ts")
        );
        assert_eq!(
            fs.repository_path(&virtual_path("src/main.ts")).as_deref(),
            Some("src/main.ts")
        );
        assert_eq!(
            fs.read(&virtual_path("node_modules/@app/shared/package.json"))
                .unwrap(),
            b"packages/shared/package.json"
        );
    }

    #[test]
    fn without_workspace_packages_there_is_no_node_modules() {
        let fs = fs(tree(&["a.ts"], &[], &[]));
        assert!(!is_dir(&fs, "node_modules"));
    }

    #[test]
    fn the_default_file_system_is_empty() {
        let fs = <SnapshotFs as FileSystem>::new();
        assert!(fs.metadata(&virtual_path("a.ts")).is_err());
        assert!(fs.read(&virtual_path("a.ts")).is_err());
    }
}
