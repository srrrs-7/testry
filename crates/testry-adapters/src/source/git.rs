//! Git snapshot adapter (spec section 6).
//!
//! Everything here is read-only for the repository:
//! - no `reset`, `clean`, `stash`, `checkout` or `fetch` is ever issued;
//! - `GIT_OPTIONAL_LOCKS=0` keeps Git from refreshing the index on disk;
//! - diffs run with `--no-ext-diff --no-textconv`, so Git starts no external
//!   diff or conversion program;
//! - all listings use NUL separators, so spaces, newlines and non-ASCII names
//!   are never misparsed.
//!
//! A base that cannot be resolved is an error. It is never treated as an
//! empty diff.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use testry_application::ports::planning::{SnapshotRequest, SourceRepository};
use testry_application::ports::{OperationContext, PortError, PortFuture, SourceUnavailableReason};
use testry_domain::fingerprint::Fingerprint;
use testry_domain::ids::{BaselineId, WorkspaceId};
use testry_domain::paths::{RepoPath, RepoPathError};
use testry_domain::pattern::PatternSet;
use testry_domain::snapshot::{
    BaselineFile, ChangeSource, CurrentFile, FileKind, SnapshotDiagnostic, SnapshotDiagnosticKind,
    SourceSnapshot, SourceSnapshotParts,
};

/// Git-ignored files that match a declared input are fingerprinted. A larger
/// number almost certainly means a build output that should be listed in
/// `workspace.exclude`, so it is reported instead of hashed silently.
const MAX_IGNORED_INPUT_FILES: usize = 20_000;

/// Environment variables that would point Git at another repository or index.
const REDIRECTING_VARIABLES: [&str; 8] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
];

const MODE_SYMLINK: &str = "120000";
const MODE_SUBMODULE: &str = "160000";

#[derive(Debug, Clone)]
pub struct GitSourceRepository {
    program: String,
}

impl GitSourceRepository {
    pub fn new() -> Self {
        Self {
            program: "git".to_owned(),
        }
    }

    /// Uses another Git executable. Intended for tests.
    pub fn with_program(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
        }
    }
}

impl GitSourceRepository {
    /// The first line of `git --version`, for read-only diagnostics.
    pub async fn version(&self) -> Result<String, PortError> {
        let output = tokio::process::Command::new(&self.program)
            .arg("--version")
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|error| {
                unavailable(
                    SourceUnavailableReason::GitUnavailable,
                    format!("could not start `{}`: {}", self.program, error.kind()),
                )
            })?;
        if !output.status.success() {
            return Err(unavailable(
                SourceUnavailableReason::GitFailed,
                "`git --version` failed",
            ));
        }
        Ok(first_line(&output.stdout))
    }
}

impl Default for GitSourceRepository {
    fn default() -> Self {
        Self::new()
    }
}

impl SourceRepository for GitSourceRepository {
    fn snapshot<'a>(
        &'a self,
        request: &'a SnapshotRequest,
        ctx: &'a OperationContext,
    ) -> PortFuture<'a, SourceSnapshot> {
        Box::pin(self.take_snapshot(request, ctx))
    }
}

pub(crate) fn unavailable(
    reason: SourceUnavailableReason,
    message: impl Into<String>,
) -> PortError {
    PortError::SourceUnavailable {
        reason,
        message: message.into(),
    }
}

struct GitOutput {
    success: bool,
    stdout: Vec<u8>,
    stderr: String,
}

struct Git<'a> {
    program: &'a str,
    root: &'a Path,
    ctx: &'a OperationContext,
}

impl Git<'_> {
    async fn run(&self, arguments: &[&str]) -> Result<GitOutput, PortError> {
        self.ctx.ensure_active("git")?;
        let mut command = tokio::process::Command::new(self.program);
        command
            .arg("-C")
            .arg(self.root)
            .arg("--no-pager")
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .kill_on_drop(true);
        for variable in REDIRECTING_VARIABLES {
            command.env_remove(variable);
        }
        let output = tokio::select! {
            output = command.output() => output,
            () = self.ctx.cancellation.cancelled() => return Err(PortError::Cancelled),
        };
        let output = output.map_err(|error| {
            unavailable(
                SourceUnavailableReason::GitUnavailable,
                format!("could not start `{}`: {}", self.program, error.kind()),
            )
        })?;
        Ok(GitOutput {
            success: output.status.success(),
            stdout: output.stdout,
            stderr: first_line(&output.stderr),
        })
    }

    /// Runs a command that must succeed.
    async fn expect(&self, arguments: &[&str]) -> Result<Vec<u8>, PortError> {
        let output = self.run(arguments).await?;
        if output.success {
            Ok(output.stdout)
        } else {
            let command = arguments.first().copied().unwrap_or("git");
            Err(unavailable(
                SourceUnavailableReason::GitFailed,
                format!("`git {command}` failed: {}", output.stderr),
            ))
        }
    }

    /// Resolves a revision to a full commit ID, or `None` when it does not
    /// name a commit. `--end-of-options` keeps a ref that looks like an
    /// option from being interpreted as one.
    async fn commit(&self, revision: &str) -> Result<Option<BaselineId>, PortError> {
        let peeled = format!("{revision}^{{commit}}");
        let output = self
            .run(&[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &peeled,
            ])
            .await?;
        if !output.success {
            return Ok(None);
        }
        parse_commit(&output.stdout).map(Some)
    }
}

fn first_line(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    text.lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(300)
        .collect()
}

fn parse_commit(stdout: &[u8]) -> Result<BaselineId, PortError> {
    let text = String::from_utf8_lossy(stdout);
    BaselineId::try_new(text.trim()).map_err(|error| {
        unavailable(
            SourceUnavailableReason::GitFailed,
            format!("unexpected commit id: {error}"),
        )
    })
}

/// NUL-separated records without the trailing empty one.
fn records(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
}

/// Renders raw path bytes for an error message. This is display only; the
/// bytes are never converted and used as a path.
fn display_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .flat_map(|byte| std::ascii::escape_default(*byte))
        .map(char::from)
        .collect()
}

fn repo_path(bytes: &[u8]) -> Result<RepoPath, PortError> {
    RepoPath::try_from_bytes(bytes).map_err(|error| match error {
        RepoPathError::NotUtf8 => unavailable(
            SourceUnavailableReason::NonUtf8Path,
            format!(
                "path `{}` is not valid UTF-8; non-UTF-8 paths are not supported in v0.1",
                display_bytes(bytes)
            ),
        ),
        other => unavailable(
            SourceUnavailableReason::InvalidPath,
            format!(
                "Git reported an unusable path `{}`: {other}",
                display_bytes(bytes)
            ),
        ),
    })
}

/// A path as Git lists it. Untracked nested repositories and collapsed
/// ignored directories end with `/`.
struct Listed {
    path: RepoPath,
    is_directory: bool,
}

fn listed(bytes: &[u8]) -> Result<Listed, PortError> {
    let is_directory = bytes.ends_with(b"/");
    Ok(Listed {
        path: repo_path(bytes)?,
        is_directory,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tracked {
    Regular,
    Symlink,
    Submodule,
}

fn kind_of_mode(mode: &str) -> Tracked {
    match mode {
        MODE_SYMLINK => Tracked::Symlink,
        MODE_SUBMODULE => Tracked::Submodule,
        _ => Tracked::Regular,
    }
}

/// Splits `<meta>\t<path>` records of `ls-files --stage` and `ls-tree`.
fn split_meta(record: &[u8]) -> Result<(Vec<&str>, &[u8]), PortError> {
    let malformed = || {
        unavailable(
            SourceUnavailableReason::GitFailed,
            format!("unexpected listing record `{}`", display_bytes(record)),
        )
    };
    let tab = record
        .iter()
        .position(|byte| *byte == b'\t')
        .ok_or_else(malformed)?;
    let (meta, path) = record.split_at(tab);
    let meta = std::str::from_utf8(meta).map_err(|_| malformed())?;
    Ok((meta.split(' ').collect(), path.get(1..).unwrap_or_default()))
}

impl GitSourceRepository {
    async fn take_snapshot(
        &self,
        request: &SnapshotRequest,
        ctx: &OperationContext,
    ) -> Result<SourceSnapshot, PortError> {
        let root = canonical_root(&request.workspace_root)?;
        let git = Git {
            program: &self.program,
            root: &root,
            ctx,
        };

        ensure_top_level(&git, &root).await?;
        if !git
            .expect(&["ls-files", "--unmerged", "-z"])
            .await?
            .is_empty()
        {
            return Err(unavailable(
                SourceUnavailableReason::UnmergedIndex,
                "the index contains unmerged entries; resolve the conflict first",
            ));
        }

        let baseline = if request.mode.needs_baseline() {
            Some(resolve_baseline(&git, request.base_ref.as_deref()).await?)
        } else {
            None
        };

        let exclude = &request.exclude;
        let mut diagnostics = BTreeSet::new();

        // Index entries, keyed by path, with the mode that tells symlinks and
        // submodules apart.
        let mut candidates: BTreeMap<RepoPath, Option<(Tracked, String)>> = BTreeMap::new();
        for record in records(&git.expect(&["ls-files", "--stage", "-z"]).await?) {
            let (meta, path) = split_meta(record)?;
            let path = repo_path(path)?;
            if exclude.is_match(&path) {
                continue;
            }
            let mode = meta.first().copied().unwrap_or_default();
            let object_id = meta.get(1).copied().unwrap_or_default().to_owned();
            candidates.insert(path, Some((kind_of_mode(mode), object_id)));
        }

        let mut untracked = BTreeSet::new();
        if request.include_dirty {
            let listing = git
                .expect(&["ls-files", "--others", "--exclude-standard", "-z"])
                .await?;
            for record in records(&listing) {
                let entry = listed(record)?;
                if exclude.is_match(&entry.path) {
                    continue;
                }
                if entry.is_directory {
                    // An untracked nested repository. Its contents are not ours to read.
                    diagnostics.insert(SnapshotDiagnostic {
                        kind: SnapshotDiagnosticKind::SubmoduleNotAnalyzed,
                        path: entry.path.clone(),
                    });
                }
                untracked.insert(entry.path.clone());
                candidates.entry(entry.path).or_insert(None);
            }
        }

        // Git-ignored files are invisible to the listings above, but a declared
        // input stays an input when it is ignored (`.env` is the usual case).
        let ignored_listing = git
            .expect(&[
                "ls-files",
                "--others",
                "--ignored",
                "--exclude-standard",
                "--directory",
                "-z",
            ])
            .await?;
        let mut ignored_entries = Vec::new();
        for record in records(&ignored_listing) {
            ignored_entries.push(listed(record)?);
        }
        let ignored_inputs = {
            let root = root.clone();
            let exclude = exclude.clone();
            let declared = request.declared_inputs.clone();
            run_blocking(move || {
                collect_ignored_inputs(&root, ignored_entries, &exclude, &declared)
            })
            .await?
        };
        for path in ignored_inputs {
            if let std::collections::btree_map::Entry::Vacant(entry) = candidates.entry(path) {
                diagnostics.insert(SnapshotDiagnostic {
                    kind: SnapshotDiagnosticKind::IgnoredInputWithoutBaseline,
                    path: entry.key().clone(),
                });
                entry.insert(None);
            }
        }

        ctx.ensure_active("fingerprinting the working tree")?;
        let current = {
            let root = root.clone();
            run_blocking(move || fingerprint_working_tree(&root, candidates)).await?
        };
        for (path, file) in &current {
            if file.kind == FileKind::Submodule {
                diagnostics.insert(SnapshotDiagnostic {
                    kind: SnapshotDiagnosticKind::SubmoduleNotAnalyzed,
                    path: path.clone(),
                });
            }
        }

        let mut changes: BTreeMap<RepoPath, BTreeSet<ChangeSource>> = BTreeMap::new();
        let mut baseline_files = BTreeMap::new();
        if let Some(baseline) = &baseline {
            let head = git.commit("HEAD").await?.ok_or_else(unborn_head)?;
            const DIFF: [&str; 6] = [
                "diff",
                "--name-status",
                "-z",
                "--no-renames",
                "--no-ext-diff",
                "--no-textconv",
            ];
            let mut record_changes =
                |stdout: &[u8], source: ChangeSource| -> Result<(), PortError> {
                    for path in changed_paths(stdout)? {
                        if !exclude.is_match(&path) {
                            changes.entry(path).or_default().insert(source);
                        }
                    }
                    Ok(())
                };

            if baseline != &head {
                let arguments = [&DIFF[..], &[baseline.as_str(), head.as_str(), "--"][..]].concat();
                record_changes(&git.expect(&arguments).await?, ChangeSource::Committed)?;
            }
            if request.include_dirty {
                let staged = [&DIFF[..], &["--cached", head.as_str(), "--"][..]].concat();
                record_changes(&git.expect(&staged).await?, ChangeSource::Staged)?;
                let unstaged = [&DIFF[..], &["--"][..]].concat();
                record_changes(&git.expect(&unstaged).await?, ChangeSource::Unstaged)?;
                for path in untracked {
                    changes
                        .entry(path)
                        .or_default()
                        .insert(ChangeSource::Untracked);
                }
            }

            let tree = git
                .expect(&["ls-tree", "-r", "-z", "--full-tree", baseline.as_str()])
                .await?;
            for record in records(&tree) {
                let (meta, path) = split_meta(record)?;
                let path = repo_path(path)?;
                if exclude.is_match(&path) {
                    continue;
                }
                let kind = match kind_of_mode(meta.first().copied().unwrap_or_default()) {
                    Tracked::Regular => FileKind::File,
                    Tracked::Symlink => FileKind::Symlink,
                    Tracked::Submodule => FileKind::Submodule,
                };
                let object_id = meta.get(2).copied().unwrap_or_default().to_owned();
                baseline_files.insert(path, BaselineFile { kind, object_id });
            }
        }

        Ok(SourceSnapshot::new(
            workspace_id(&root)?,
            SourceSnapshotParts {
                baseline,
                changes,
                current,
                baseline_files,
                diagnostics,
            },
        ))
    }
}

fn canonical_root(root: &Path) -> Result<PathBuf, PortError> {
    std::fs::canonicalize(root).map_err(|error| {
        unavailable(
            SourceUnavailableReason::NotARepository,
            format!(
                "workspace root {} cannot be resolved: {}",
                root.display(),
                error.kind()
            ),
        )
    })
}

/// The workspace must be the top level of a work tree: repository-relative
/// paths are only meaningful from there.
async fn ensure_top_level(git: &Git<'_>, root: &Path) -> Result<(), PortError> {
    let output = git.run(&["rev-parse", "--show-toplevel"]).await?;
    if !output.success {
        return Err(unavailable(
            SourceUnavailableReason::NotARepository,
            format!(
                "{} is not inside a Git work tree: {}",
                root.display(),
                output.stderr
            ),
        ));
    }
    let reported = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
    let top_level = std::fs::canonicalize(Path::new(std::ffi::OsStr::from_bytes(reported)));
    match top_level {
        Ok(top_level) if top_level == root => Ok(()),
        Ok(top_level) => Err(unavailable(
            SourceUnavailableReason::NotARepository,
            format!(
                "workspace root {} is not the top level of its Git work tree ({})",
                root.display(),
                top_level.display()
            ),
        )),
        Err(error) => Err(unavailable(
            SourceUnavailableReason::GitFailed,
            format!("the Git top level cannot be resolved: {}", error.kind()),
        )),
    }
}

fn unborn_head() -> PortError {
    unavailable(
        SourceUnavailableReason::UnbornHead,
        "HEAD does not point to a commit yet, so there is no baseline to compare with; \
         commit first or run with --mode full",
    )
}

/// The commit a diff is computed against: `HEAD` itself, or the merge base of
/// the requested ref and `HEAD`. The resolved ID is what gets stored.
async fn resolve_baseline(git: &Git<'_>, base_ref: Option<&str>) -> Result<BaselineId, PortError> {
    let head = git.commit("HEAD").await?.ok_or_else(unborn_head)?;
    let Some(base_ref) = base_ref else {
        return Ok(head);
    };
    if base_ref.is_empty() || base_ref.chars().any(char::is_control) {
        return Err(unavailable(
            SourceUnavailableReason::BaseNotFound,
            "the base ref is empty or contains control characters",
        ));
    }
    let Some(base) = git.commit(base_ref).await? else {
        return Err(unavailable(
            SourceUnavailableReason::BaseNotFound,
            format!(
                "base `{base_ref}` does not name a commit in this clone. testry never fetches: \
                 fetch it yourself, or run with --mode full"
            ),
        ));
    };
    let output = git
        .run(&["merge-base", base.as_str(), head.as_str()])
        .await?;
    if !output.success {
        return Err(unavailable(
            SourceUnavailableReason::NoMergeBase,
            format!(
                "`{base_ref}` and HEAD have no common ancestor in this clone (a shallow clone is \
                 the usual cause). testry never fetches: deepen the clone, or run with --mode full"
            ),
        ));
    }
    parse_commit(&output.stdout)
}

/// Parses `git diff --name-status -z`. Renames and copies cannot occur with
/// `--no-renames`, but if they did, both paths would count as changed.
fn changed_paths(stdout: &[u8]) -> Result<Vec<RepoPath>, PortError> {
    let mut paths = Vec::new();
    let mut tokens = records(stdout);
    while let Some(status) = tokens.next() {
        let path_count = if matches!(status.first(), Some(b'R' | b'C')) {
            2
        } else {
            1
        };
        for _ in 0..path_count {
            let path = tokens.next().ok_or_else(|| {
                unavailable(
                    SourceUnavailableReason::GitFailed,
                    "truncated `git diff` output",
                )
            })?;
            paths.push(repo_path(path)?);
        }
    }
    Ok(paths)
}

fn workspace_id(root: &Path) -> Result<WorkspaceId, PortError> {
    let digest = Fingerprint::of_bytes(root.as_os_str().as_bytes()).to_hex();
    let short: String = digest.chars().take(32).collect();
    WorkspaceId::try_new(format!("ws-{short}")).map_err(|error| {
        unavailable(
            SourceUnavailableReason::GitFailed,
            format!("workspace id: {error}"),
        )
    })
}

async fn run_blocking<T, F>(work: F) -> Result<T, PortError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, PortError> + Send + 'static,
{
    tokio::task::spawn_blocking(work).await.map_err(|error| {
        unavailable(
            SourceUnavailableReason::ReadFailed,
            format!("a worker task failed: {error}"),
        )
    })?
}

fn read_failed(path: &Path, error: &std::io::Error) -> PortError {
    unavailable(
        SourceUnavailableReason::ReadFailed,
        format!("cannot read {}: {}", path.display(), error.kind()),
    )
}

/// Finds ignored files that match a declared input. Ignored directories are
/// walked without following symbolic links, and excluded directories are not
/// entered.
fn collect_ignored_inputs(
    root: &Path,
    entries: Vec<Listed>,
    exclude: &PatternSet,
    declared: &PatternSet,
) -> Result<Vec<RepoPath>, PortError> {
    let mut found = Vec::new();
    if declared.is_empty() {
        return Ok(found);
    }
    let mut pending: Vec<Listed> = entries;
    while let Some(entry) = pending.pop() {
        if exclude.is_match(&entry.path) {
            continue;
        }
        if !entry.is_directory {
            if declared.is_match(&entry.path) {
                found.push(entry.path);
                if found.len() > MAX_IGNORED_INPUT_FILES {
                    return Err(PortError::Configuration {
                        message: format!(
                            "more than {MAX_IGNORED_INPUT_FILES} Git-ignored files match the \
                             declared inputs (last: {}). List runtime outputs such as build or \
                             coverage directories in workspace.exclude",
                            found.last().map_or("", RepoPath::as_str)
                        ),
                    });
                }
            }
            continue;
        }
        // A probe below the directory tells whether an exclude pattern such
        // as `**/node_modules/**` covers everything in it.
        let probe = format!("{}/testry-exclude-probe", entry.path);
        if RepoPath::try_new(&probe).is_ok_and(|probe| exclude.is_match(&probe)) {
            continue;
        }
        let directory = root.join(entry.path.as_str());
        let children = match std::fs::read_dir(&directory) {
            Ok(children) => children,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(read_failed(&directory, &error)),
        };
        for child in children {
            let child = child.map_err(|error| read_failed(&directory, &error))?;
            let name = child.file_name();
            let name = name.to_str().ok_or_else(|| {
                unavailable(
                    SourceUnavailableReason::NonUtf8Path,
                    format!(
                        "a file below {} has a name that is not valid UTF-8 (`{}`)",
                        entry.path,
                        display_bytes(name.as_bytes())
                    ),
                )
            })?;
            let path = RepoPath::try_new(format!("{}/{name}", entry.path)).map_err(|error| {
                unavailable(SourceUnavailableReason::InvalidPath, error.to_string())
            })?;
            // `file_type` does not follow symbolic links.
            let file_type = child
                .file_type()
                .map_err(|error| read_failed(&directory, &error))?;
            pending.push(Listed {
                path,
                is_directory: file_type.is_dir(),
            });
        }
    }
    found.sort();
    Ok(found)
}

/// Fingerprints every candidate that exists in the working tree. Tracked
/// files that were deleted are simply absent from the result.
fn fingerprint_working_tree(
    root: &Path,
    candidates: BTreeMap<RepoPath, Option<(Tracked, String)>>,
) -> Result<BTreeMap<RepoPath, CurrentFile>, PortError> {
    let candidates: Vec<(RepoPath, Option<(Tracked, String)>)> = candidates.into_iter().collect();
    let workers = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .clamp(1, 8);
    let chunk_size = candidates.len().div_ceil(workers).max(1);

    let results: Vec<Result<Vec<(RepoPath, CurrentFile)>, PortError>> =
        std::thread::scope(|scope| {
            let handles: Vec<_> = candidates
                .chunks(chunk_size)
                .map(|chunk| {
                    scope.spawn(move || {
                        let mut files = Vec::with_capacity(chunk.len());
                        for (path, tracked) in chunk {
                            if let Some(file) = fingerprint_entry(root, path, tracked.as_ref())? {
                                files.push((path.clone(), file));
                            }
                        }
                        Ok(files)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| {
                    handle.join().unwrap_or_else(|_| {
                        Err(unavailable(
                            SourceUnavailableReason::ReadFailed,
                            "a fingerprint worker stopped unexpectedly",
                        ))
                    })
                })
                .collect()
        });

    let mut current = BTreeMap::new();
    for result in results {
        current.extend(result?);
    }
    Ok(current)
}

fn fingerprint_entry(
    root: &Path,
    path: &RepoPath,
    tracked: Option<&(Tracked, String)>,
) -> Result<Option<CurrentFile>, PortError> {
    let full = root.join(path.as_str());
    // `symlink_metadata` does not follow a link, so nothing outside the
    // repository is ever opened through one.
    let metadata = match std::fs::symlink_metadata(&full) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotADirectory => return Ok(None),
        Err(error) => return Err(read_failed(&full, &error)),
    };
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        let target = std::fs::read_link(&full).map_err(|error| read_failed(&full, &error))?;
        return Ok(Some(CurrentFile {
            kind: FileKind::Symlink,
            digest: Fingerprint::of_bytes(target.as_os_str().as_bytes()),
        }));
    }
    if file_type.is_file() {
        return Ok(Some(CurrentFile {
            kind: FileKind::File,
            digest: hash_file(&full)?,
        }));
    }
    if file_type.is_dir() {
        return Ok(match tracked {
            // The contents of a submodule are not fingerprinted in v0.1; only
            // the commit recorded in the index is.
            Some((Tracked::Submodule, object_id)) => Some(CurrentFile {
                kind: FileKind::Submodule,
                digest: Fingerprint::of_bytes(format!("submodule:{object_id}").as_bytes()),
            }),
            // An untracked nested repository.
            None => Some(CurrentFile {
                kind: FileKind::Submodule,
                digest: Fingerprint::of_bytes(b"untracked-repository"),
            }),
            // A tracked file that was replaced by a directory counts as deleted.
            Some(_) => None,
        });
    }
    Ok(None)
}

fn hash_file(path: &Path) -> Result<Fingerprint, PortError> {
    let mut file = std::fs::File::open(path).map_err(|error| read_failed(path, &error))?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| read_failed(path, &error))?;
        if read == 0 {
            break;
        }
        hasher.update(buffer.get(..read).unwrap_or_default());
    }
    Ok(Fingerprint::from_digest(*hasher.finalize().as_bytes()))
}
