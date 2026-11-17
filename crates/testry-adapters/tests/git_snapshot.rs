//! Git snapshot adapter against real repositories (spec section 6 and the
//! G08-G14, G17 acceptance items). The repositories live in temporary
//! directories inside the test container.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use testry_adapters::source::git::GitSourceRepository;
use testry_adapters::source::reader::{GitWorkspaceReader, SourceContent, SourceReader};
use testry_application::ports::planning::{SnapshotRequest, SourceRepository};
use testry_application::ports::{PortError, SourceUnavailableReason};
use testry_application::testing::{cancelled_context, context};
use testry_domain::fingerprint::Fingerprint;
use testry_domain::paths::RepoPath;
use testry_domain::pattern::PatternSet;
use testry_domain::plan::Mode;
use testry_domain::snapshot::{ChangeSource, FileKind, SnapshotDiagnosticKind, SourceSnapshot};

struct Repo {
    directory: tempfile::TempDir,
}

impl Repo {
    fn new() -> Self {
        let repo = Self {
            directory: tempfile::tempdir().unwrap(),
        };
        repo.git(&["init", "--quiet", "--initial-branch=main"]);
        repo.git(&["config", "user.name", "testry test"]);
        repo.git(&["config", "user.email", "test@example.invalid"]);
        repo.git(&["config", "commit.gpgsign", "false"]);
        repo
    }

    fn root(&self) -> PathBuf {
        self.directory.path().canonicalize().unwrap()
    }

    fn git(&self, arguments: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(self.directory.path())
            .args(arguments)
            .output()
            .expect("git must be installed in the test container");
        assert!(
            output.status.success(),
            "git {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn write(&self, path: &str, content: &str) {
        let full = self.directory.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, content).unwrap();
    }

    fn remove(&self, path: &str) {
        std::fs::remove_file(self.directory.path().join(path)).unwrap();
    }

    fn commit_all(&self, message: &str) -> String {
        self.git(&["add", "--all"]);
        self.git(&["commit", "--quiet", "--message", message]);
        self.git(&["rev-parse", "HEAD"])
    }

    fn request(&self, mode: Mode, base: Option<&str>) -> SnapshotRequest {
        SnapshotRequest {
            workspace_root: self.directory.path().to_path_buf(),
            mode,
            base_ref: base.map(str::to_owned),
            include_dirty: true,
            scope: None,
            exclude: PatternSet::try_new([".git/**", ".testry/**", "**/node_modules/**"]).unwrap(),
            declared_inputs: PatternSet::empty(),
        }
    }

    async fn try_snapshot(
        &self,
        mode: Mode,
        base: Option<&str>,
    ) -> Result<SourceSnapshot, PortError> {
        GitSourceRepository::new()
            .snapshot(&self.request(mode, base), &context())
            .await
    }

    async fn snapshot(&self, mode: Mode, base: Option<&str>) -> SourceSnapshot {
        self.try_snapshot(mode, base).await.unwrap()
    }

    async fn affected(&self) -> SourceSnapshot {
        self.snapshot(Mode::Affected, None).await
    }
}

fn path(raw: &str) -> RepoPath {
    RepoPath::try_new(raw).unwrap()
}

fn changed(snapshot: &SourceSnapshot) -> Vec<&str> {
    snapshot.changed_paths().map(RepoPath::as_str).collect()
}

fn current(snapshot: &SourceSnapshot) -> Vec<&str> {
    snapshot.current().keys().map(RepoPath::as_str).collect()
}

fn sources(snapshot: &SourceSnapshot, file: &str) -> Vec<ChangeSource> {
    snapshot
        .changes()
        .get(&path(file))
        .map(|sources| sources.iter().copied().collect())
        .unwrap_or_default()
}

fn reason(error: &PortError) -> SourceUnavailableReason {
    match error {
        PortError::SourceUnavailable { reason, .. } => *reason,
        other => panic!("expected a source error, got {other:?}"),
    }
}

#[tokio::test]
async fn full_mode_needs_no_commit_and_no_baseline() {
    let repo = Repo::new();
    repo.write("src/a.ts", "export {};");
    let snapshot = repo.snapshot(Mode::Full, Some("origin/main")).await;
    assert_eq!(snapshot.baseline(), None);
    assert!(snapshot.changes().is_empty());
    assert!(snapshot.baseline_files().is_empty());
    assert_eq!(current(&snapshot), ["src/a.ts"]);
}

#[tokio::test]
async fn affected_without_any_commit_is_an_error_not_an_empty_diff() {
    let repo = Repo::new();
    repo.write("src/a.ts", "export {};");
    for mode in [Mode::Affected, Mode::Shadow] {
        let error = repo.try_snapshot(mode, None).await.unwrap_err();
        assert_eq!(reason(&error), SourceUnavailableReason::UnbornHead);
        assert_eq!(error.exit_condition().exit_code(), 2);
    }
}

/// G11: staged and unstaged changes are both part of the default diff.
#[tokio::test]
async fn g11_default_base_is_head_and_covers_staged_unstaged_and_untracked() {
    let repo = Repo::new();
    for name in ["a.ts", "b.ts", "c.ts", "untouched.ts"] {
        repo.write(name, "export const v = 1;");
    }
    let head = repo.commit_all("initial");

    repo.write("a.ts", "export const v = 2;"); // unstaged
    repo.write("b.ts", "export const v = 2;");
    repo.git(&["add", "b.ts"]); // staged
    repo.write("c.ts", "export const v = 2;");
    repo.git(&["add", "c.ts"]);
    repo.write("c.ts", "export const v = 3;"); // staged and unstaged
    repo.write("new.test.ts", "test"); // untracked

    let snapshot = repo.affected().await;
    assert_eq!(snapshot.baseline().unwrap().as_str(), head);
    assert_eq!(changed(&snapshot), ["a.ts", "b.ts", "c.ts", "new.test.ts"]);
    assert_eq!(sources(&snapshot, "a.ts"), [ChangeSource::Unstaged]);
    assert_eq!(sources(&snapshot, "b.ts"), [ChangeSource::Staged]);
    assert_eq!(
        sources(&snapshot, "c.ts"),
        [ChangeSource::Staged, ChangeSource::Unstaged]
    );
    assert_eq!(sources(&snapshot, "new.test.ts"), [ChangeSource::Untracked]);
    assert!(sources(&snapshot, "untouched.ts").is_empty());
}

#[tokio::test]
async fn a_clean_tree_has_no_changes() {
    let repo = Repo::new();
    repo.write("a.ts", "export {};");
    repo.commit_all("initial");
    let snapshot = repo.affected().await;
    assert!(snapshot.changes().is_empty());
    assert_eq!(current(&snapshot), ["a.ts"]);
    assert_eq!(snapshot.baseline_files().len(), 1);
}

#[tokio::test]
async fn an_explicit_base_uses_the_merge_base_with_head() {
    let repo = Repo::new();
    repo.write("shared.ts", "1");
    let fork_point = repo.commit_all("fork point");

    repo.git(&["checkout", "--quiet", "-b", "feature"]);
    repo.write("feature.ts", "1");
    repo.commit_all("feature work");

    repo.git(&["checkout", "--quiet", "main"]);
    repo.write("main-only.ts", "1");
    let main_tip = repo.commit_all("main moved on");

    repo.git(&["checkout", "--quiet", "feature"]);
    repo.write("dirty.ts", "1");

    let snapshot = repo.snapshot(Mode::Affected, Some("main")).await;
    assert_eq!(snapshot.baseline().unwrap().as_str(), fork_point);
    assert_ne!(snapshot.baseline().unwrap().as_str(), main_tip);
    assert_eq!(changed(&snapshot), ["dirty.ts", "feature.ts"]);
    assert_eq!(sources(&snapshot, "feature.ts"), [ChangeSource::Committed]);
    assert!(
        !snapshot
            .baseline_files()
            .contains_key(&path("main-only.ts"))
    );

    // The stored baseline is the resolved commit, so moving `main` later
    // cannot change what an existing snapshot refers to.
    let by_id = repo.snapshot(Mode::Shadow, Some(&main_tip)).await;
    assert_eq!(by_id.baseline().unwrap().as_str(), fork_point);
}

#[tokio::test]
async fn an_unknown_base_is_an_error_and_nothing_is_fetched() {
    let repo = Repo::new();
    repo.write("a.ts", "1");
    repo.commit_all("initial");
    repo.git(&[
        "remote",
        "add",
        "origin",
        "https://example.invalid/never-contacted.git",
    ]);

    for base in [
        "origin/main",
        "no-such-branch",
        "--upload-pack=touch /tmp/pwned",
        "",
    ] {
        let error = repo
            .try_snapshot(Mode::Affected, Some(base))
            .await
            .unwrap_err();
        assert_eq!(
            reason(&error),
            SourceUnavailableReason::BaseNotFound,
            "{base:?}"
        );
        assert_eq!(error.exit_condition().exit_code(), 2);
    }
    assert_eq!(
        repo.git(&["for-each-ref", "refs/remotes"]),
        "",
        "no remote ref may appear"
    );
    assert!(!Path::new("/tmp/pwned").exists());
}

#[tokio::test]
async fn unrelated_histories_have_no_merge_base() {
    let repo = Repo::new();
    repo.write("a.ts", "1");
    repo.commit_all("main");
    repo.git(&["checkout", "--quiet", "--orphan", "island"]);
    repo.write("b.ts", "1");
    repo.commit_all("island");

    let error = repo
        .try_snapshot(Mode::Shadow, Some("main"))
        .await
        .unwrap_err();
    assert_eq!(reason(&error), SourceUnavailableReason::NoMergeBase);
    assert_eq!(error.exit_condition().exit_code(), 2);
}

/// G09: a rename affects the old and the new path.
#[tokio::test]
async fn g09_rename_reports_old_and_new_path() {
    let repo = Repo::new();
    repo.write(
        "src/old-name.ts",
        "export const value = 1;\n".repeat(20).as_str(),
    );
    repo.write("src/user.test.ts", "import './old-name';");
    repo.commit_all("initial");
    repo.git(&["mv", "src/old-name.ts", "src/new-name.ts"]);

    let snapshot = repo.affected().await;
    assert_eq!(changed(&snapshot), ["src/new-name.ts", "src/old-name.ts"]);
    assert!(snapshot.current().contains_key(&path("src/new-name.ts")));
    assert!(!snapshot.current().contains_key(&path("src/old-name.ts")));
    assert!(
        snapshot
            .baseline_files()
            .contains_key(&path("src/old-name.ts"))
    );
}

/// G08: a deleted file is a change and stays readable from the baseline.
#[tokio::test]
async fn g08_deleted_files_stay_in_the_baseline() {
    let repo = Repo::new();
    repo.write("src/removed.ts", "export const removed = 1;");
    repo.write("src/kept.ts", "export {};");
    repo.commit_all("initial");
    repo.remove("src/removed.ts");

    let snapshot = repo.affected().await;
    assert_eq!(changed(&snapshot), ["src/removed.ts"]);
    assert_eq!(current(&snapshot), ["src/kept.ts"]);

    let entry = &snapshot.baseline_files()[&path("src/removed.ts")];
    let reader = GitWorkspaceReader::new(repo.root());
    let content = reader
        .read_baseline(&path("src/removed.ts"), &entry.object_id, 1024)
        .unwrap();
    assert_eq!(
        content,
        SourceContent::Bytes(b"export const removed = 1;".to_vec())
    );
}

/// G13: spaces, Japanese and newlines in paths survive NUL-separated parsing.
#[tokio::test]
async fn g13_unusual_paths_are_parsed_exactly() {
    let repo = Repo::new();
    let names = [
        "apps/with space/a b.test.ts",
        "apps/日本語/ユーザー.test.ts",
        "apps/new\nline/改行\nあり.ts",
        "apps/[id]/{x}.ts",
        "apps/tab\there.ts",
        "apps/-leading-dash.ts",
        "apps/quote\"and'quote.ts",
    ];
    for name in names {
        repo.write(name, "1");
    }
    repo.commit_all("initial");
    for name in names {
        repo.write(name, "2");
    }
    repo.write("apps/日本語/新規 ファイル.test.ts", "new");

    let snapshot = repo.affected().await;
    let mut expected: Vec<&str> = names.to_vec();
    expected.push("apps/日本語/新規 ファイル.test.ts");
    expected.sort_unstable();
    assert_eq!(changed(&snapshot), expected);
    assert_eq!(current(&snapshot), expected);
    assert_eq!(snapshot.baseline_files().len(), names.len());
}

/// G14: a non-UTF-8 path is rejected explicitly; it is never converted lossily.
#[tokio::test]
async fn g14_non_utf8_paths_are_rejected() {
    let repo = Repo::new();
    repo.write("a.ts", "1");
    repo.commit_all("initial");
    let name = std::ffi::OsStr::from_bytes(b"caf\xe9.ts");
    std::fs::write(repo.directory.path().join(name), "latin-1 name").unwrap();

    for mode in [Mode::Affected, Mode::Full] {
        let error = repo.try_snapshot(mode, None).await.unwrap_err();
        assert_eq!(reason(&error), SourceUnavailableReason::NonUtf8Path);
        assert_eq!(error.exit_condition().exit_code(), 2);
        assert!(error.to_string().contains("caf\\xe9.ts"), "{error}");
    }
}

/// G17: symbolic links are recorded as links and never followed.
#[tokio::test]
async fn g17_symlinks_are_never_followed() {
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(
        outside.path().join("secret.ts"),
        "export const secret = 'outside';",
    )
    .unwrap();

    let repo = Repo::new();
    repo.write("src/a.ts", "export {};");
    std::os::unix::fs::symlink(
        outside.path().join("secret.ts"),
        repo.directory.path().join("src/escape.ts"),
    )
    .unwrap();
    std::os::unix::fs::symlink(outside.path(), repo.directory.path().join("linked-dir")).unwrap();
    std::os::unix::fs::symlink("a.ts", repo.directory.path().join("src/inside.ts")).unwrap();
    repo.commit_all("initial");

    let snapshot = repo.affected().await;
    assert_eq!(
        current(&snapshot),
        ["linked-dir", "src/a.ts", "src/escape.ts", "src/inside.ts"]
    );
    for link in ["linked-dir", "src/escape.ts", "src/inside.ts"] {
        assert_eq!(
            snapshot.current()[&path(link)].kind,
            FileKind::Symlink,
            "{link}"
        );
        assert_eq!(
            snapshot.baseline_files()[&path(link)].kind,
            FileKind::Symlink,
            "{link}"
        );
    }
    // The digest covers the link target text, not the file it points to.
    assert_eq!(
        snapshot.current()[&path("src/inside.ts")].digest,
        Fingerprint::of_bytes(b"a.ts")
    );
    assert!(
        !current(&snapshot)
            .iter()
            .any(|name| name.contains("secret"))
    );

    // Reading a link as a regular file is refused.
    let reader = GitWorkspaceReader::new(repo.root());
    let digest = snapshot.current()[&path("src/escape.ts")].digest;
    let error = reader
        .read_current(&path("src/escape.ts"), &digest, 1024)
        .unwrap_err();
    assert_eq!(error.code(), "stale_inputs");

    // Retargeting a link is a change.
    std::fs::remove_file(repo.directory.path().join("src/inside.ts")).unwrap();
    std::os::unix::fs::symlink("b.ts", repo.directory.path().join("src/inside.ts")).unwrap();
    let retargeted = repo.affected().await;
    assert_eq!(changed(&retargeted), ["src/inside.ts"]);
    assert_ne!(retargeted.id(), snapshot.id());
}

/// G17: a submodule is listed as such and reported; its contents are not read.
#[tokio::test]
async fn g17_submodules_are_reported_and_not_analyzed() {
    let library = Repo::new();
    library.write("lib/index.ts", "export const lib = 1;");
    library.commit_all("library");

    let repo = Repo::new();
    repo.write("src/a.ts", "export {};");
    repo.commit_all("initial");
    let library_path = library.directory.path().to_str().unwrap().to_owned();
    repo.git(&[
        "-c",
        "protocol.file.allow=always",
        "submodule",
        "--quiet",
        "add",
        &library_path,
        "vendor/lib",
    ]);
    repo.commit_all("add submodule");

    let snapshot = repo.affected().await;
    assert_eq!(
        snapshot.current()[&path("vendor/lib")].kind,
        FileKind::Submodule
    );
    assert!(
        !current(&snapshot)
            .iter()
            .any(|name| name.starts_with("vendor/lib/"))
    );
    assert!(snapshot.diagnostics().iter().any(|diagnostic| {
        diagnostic.kind == SnapshotDiagnosticKind::SubmoduleNotAnalyzed
            && diagnostic.path == path("vendor/lib")
    }));
    assert!(snapshot.changes().is_empty());

    // A change inside the submodule surfaces as a change of the submodule path.
    std::fs::write(
        repo.directory.path().join("vendor/lib/lib/index.ts"),
        "changed",
    )
    .unwrap();
    let dirty = repo.affected().await;
    assert_eq!(changed(&dirty), ["vendor/lib"]);
}

#[tokio::test]
async fn a_conflicted_index_is_an_error() {
    let repo = Repo::new();
    repo.write("a.ts", "base");
    repo.commit_all("base");
    repo.git(&["checkout", "--quiet", "-b", "other"]);
    repo.write("a.ts", "other");
    repo.commit_all("other");
    repo.git(&["checkout", "--quiet", "main"]);
    repo.write("a.ts", "main");
    repo.commit_all("main");
    let merge = Command::new("git")
        .arg("-C")
        .arg(repo.directory.path())
        .args(["merge", "--quiet", "other"])
        .output()
        .unwrap();
    assert!(!merge.status.success(), "the merge must conflict");

    for mode in [Mode::Affected, Mode::Full] {
        let error = repo.try_snapshot(mode, None).await.unwrap_err();
        assert_eq!(reason(&error), SourceUnavailableReason::UnmergedIndex);
        assert_eq!(error.exit_condition().exit_code(), 2);
    }
}

fn repository_state(repo: &Repo) -> (String, String, String, Vec<u8>, String) {
    (
        repo.git(&["status", "--porcelain=v1", "--untracked-files=all"]),
        repo.git(&["rev-parse", "HEAD"]),
        repo.git(&["stash", "list"]),
        std::fs::read(repo.directory.path().join(".git/index")).unwrap(),
        repo.git(&["for-each-ref"]),
    )
}

/// Spec section 6: the work tree and the index are never modified.
#[tokio::test]
async fn the_work_tree_index_and_refs_are_left_untouched() {
    let repo = Repo::new();
    repo.write("a.ts", "1");
    repo.write("b.ts", "1");
    repo.commit_all("initial");
    repo.git(&["checkout", "--quiet", "-b", "feature"]);
    repo.write("a.ts", "2");
    repo.commit_all("feature");
    repo.write("a.ts", "3");
    repo.write("b.ts", "2");
    repo.git(&["add", "b.ts"]);
    repo.write("untracked.ts", "1");
    // Make the index stat information stale, which invites Git to refresh it.
    std::thread::sleep(std::time::Duration::from_millis(20));
    repo.write("b.ts", "2");

    let before = repository_state(&repo);
    repo.snapshot(Mode::Affected, Some("main")).await;
    repo.snapshot(Mode::Shadow, None).await;
    repo.snapshot(Mode::Full, None).await;
    assert_eq!(repository_state(&repo), before);
}

/// Spec section 6: Git starts no external diff or textconv program.
#[tokio::test]
async fn external_diff_and_textconv_programs_are_never_started() {
    let repo = Repo::new();
    let marker = repo
        .directory
        .path()
        .join("marker-created-by-external-program");
    let script = repo.directory.path().join("evil.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\ntouch '{}'\ncat \"$1\" 2>/dev/null\n",
            marker.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    repo.write(".gitattributes", "*.ts diff=evil\n");
    repo.write(".gitignore", "evil.sh\nmarker-*\n");
    repo.write("a.ts", "1");
    repo.commit_all("initial");
    let script = script.to_str().unwrap();
    repo.git(&["config", "diff.external", script]);
    repo.git(&["config", "diff.evil.textconv", script]);
    repo.git(&["config", "diff.evil.command", script]);
    repo.write("a.ts", "2");
    repo.git(&["add", "a.ts"]);
    repo.write("a.ts", "3");

    let snapshot = repo.affected().await;
    assert_eq!(changed(&snapshot), ["a.ts"]);
    assert!(
        !marker.exists(),
        "an external diff or textconv program was started"
    );
}

#[tokio::test]
async fn excluded_paths_are_neither_inputs_nor_changes() {
    let repo = Repo::new();
    repo.write("src/a.ts", "1");
    repo.write("node_modules/pkg/index.js", "tracked by mistake");
    repo.commit_all("initial");
    repo.write("node_modules/pkg/index.js", "changed");
    repo.write("apps/web/node_modules/x/y.js", "untracked");
    repo.write(".testry/runs/x/result.json", "{}");

    let snapshot = repo.affected().await;
    assert_eq!(current(&snapshot), ["src/a.ts"]);
    assert!(snapshot.changes().is_empty());
    assert_eq!(snapshot.baseline_files().len(), 1);
}

#[tokio::test]
async fn ignored_files_that_match_declared_inputs_are_fingerprinted() {
    let repo = Repo::new();
    repo.write(
        ".gitignore",
        ".env\n.env.*\ndist/\nnode_modules/\ncoverage/\n",
    );
    repo.write("apps/api/src/a.ts", "1");
    repo.commit_all("initial");
    repo.write(".env", "SECRET=1");
    repo.write("apps/api/.env.local", "SECRET=2");
    repo.write("apps/api/dist/out.js", "built");
    repo.write("apps/api/coverage/lcov.info", "coverage");
    repo.write("dist/root-out.js", "built");
    repo.write("node_modules/pkg/.env", "not ours");

    let mut request = repo.request(Mode::Affected, None);
    request.declared_inputs = PatternSet::try_new(["**/.env", "**/.env.*", "apps/api/**"]).unwrap();
    request.exclude = PatternSet::try_new(["**/node_modules/**", "**/coverage/**"]).unwrap();
    let source = GitSourceRepository::new();
    let snapshot = source.snapshot(&request, &context()).await.unwrap();

    assert_eq!(
        current(&snapshot),
        [
            ".env",
            ".gitignore",
            "apps/api/.env.local",
            "apps/api/dist/out.js",
            "apps/api/src/a.ts"
        ]
    );
    // There is no baseline for an ignored file, so it never counts as changed.
    assert!(snapshot.changes().is_empty());
    let reported: BTreeSet<&str> = snapshot
        .diagnostics()
        .iter()
        .filter(|diagnostic| diagnostic.kind == SnapshotDiagnosticKind::IgnoredInputWithoutBaseline)
        .map(|diagnostic| diagnostic.path.as_str())
        .collect();
    assert_eq!(
        reported,
        BTreeSet::from([".env", "apps/api/.env.local", "apps/api/dist/out.js"])
    );

    // But it is part of the input fingerprint (P13).
    repo.write(".env", "SECRET=changed");
    let after = source.snapshot(&request, &context()).await.unwrap();
    assert_ne!(after.id(), snapshot.id());
    assert!(after.changes().is_empty());
}

/// P13: the snapshot ID follows the inputs and nothing else.
#[tokio::test]
async fn p13_snapshot_id_is_stable_and_follows_content() {
    let repo = Repo::new();
    repo.write("a.ts", "1");
    repo.write("b.ts", "1");
    repo.commit_all("initial");
    repo.write("a.ts", "2");

    let first = repo.affected().await;
    let again = repo.affected().await;
    assert_eq!(first.id(), again.id());

    // Staging the same content does not change the inputs.
    repo.git(&["add", "a.ts"]);
    assert_eq!(repo.affected().await.id(), first.id());

    repo.write("a.ts", "3");
    let modified = repo.affected().await;
    assert_ne!(modified.id(), first.id());

    repo.write("c.ts", "new");
    let added = repo.affected().await;
    assert_ne!(added.id(), modified.id());

    repo.remove("b.ts");
    assert_ne!(repo.affected().await.id(), added.id());

    // Full mode has no baseline, so its ID differs from an affected snapshot.
    assert_ne!(
        repo.snapshot(Mode::Full, None).await.id(),
        repo.affected().await.id()
    );
}

#[tokio::test]
async fn the_workspace_must_be_the_top_level_of_a_work_tree() {
    let repo = Repo::new();
    repo.write("sub/a.ts", "1");
    repo.commit_all("initial");

    let mut nested = repo.request(Mode::Full, None);
    nested.workspace_root = repo.directory.path().join("sub");
    let error = GitSourceRepository::new()
        .snapshot(&nested, &context())
        .await
        .unwrap_err();
    assert_eq!(reason(&error), SourceUnavailableReason::NotARepository);

    let plain = tempfile::tempdir().unwrap();
    let mut outside = repo.request(Mode::Full, None);
    outside.workspace_root = plain.path().to_path_buf();
    let error = GitSourceRepository::new()
        .snapshot(&outside, &context())
        .await
        .unwrap_err();
    assert_eq!(reason(&error), SourceUnavailableReason::NotARepository);
    assert_eq!(error.exit_condition().exit_code(), 2);

    let mut missing = repo.request(Mode::Full, None);
    missing.workspace_root = repo.directory.path().join("does-not-exist");
    let error = GitSourceRepository::new()
        .snapshot(&missing, &context())
        .await
        .unwrap_err();
    assert_eq!(reason(&error), SourceUnavailableReason::NotARepository);
}

#[tokio::test]
async fn a_missing_git_executable_is_an_infrastructure_error() {
    let repo = Repo::new();
    let source = GitSourceRepository::with_program("/nonexistent/git");
    let error = source
        .snapshot(&repo.request(Mode::Full, None), &context())
        .await
        .unwrap_err();
    assert_eq!(reason(&error), SourceUnavailableReason::GitUnavailable);
    assert_eq!(error.exit_condition().exit_code(), 3);
}

#[tokio::test]
async fn a_cancelled_snapshot_stops() {
    let repo = Repo::new();
    let error = GitSourceRepository::new()
        .snapshot(&repo.request(Mode::Full, None), &cancelled_context())
        .await
        .unwrap_err();
    assert_eq!(error, PortError::Cancelled);
}

#[tokio::test]
async fn worktrees_are_distinct_workspaces() {
    let repo = Repo::new();
    repo.write("a.ts", "1");
    repo.commit_all("initial");
    let other = tempfile::tempdir().unwrap();
    let other_path = other.path().join("tree");
    repo.git(&[
        "worktree",
        "add",
        "--quiet",
        other_path.to_str().unwrap(),
        "-b",
        "second",
    ]);

    let first = repo.snapshot(Mode::Full, None).await;
    let mut request = repo.request(Mode::Full, None);
    request.workspace_root = other_path;
    let second = GitSourceRepository::new()
        .snapshot(&request, &context())
        .await
        .unwrap();
    assert_ne!(first.workspace_id(), second.workspace_id());
    assert_eq!(current(&second), ["a.ts"]);
}

#[tokio::test]
async fn the_reader_verifies_current_content_against_the_snapshot() {
    let repo = Repo::new();
    repo.write("a.ts", "export const a = 1;");
    repo.commit_all("initial");
    let snapshot = repo.affected().await;
    let digest = snapshot.current()[&path("a.ts")].digest;
    let reader = GitWorkspaceReader::new(repo.root());

    assert_eq!(
        reader.read_current(&path("a.ts"), &digest, 1024).unwrap(),
        SourceContent::Bytes(b"export const a = 1;".to_vec())
    );
    assert_eq!(
        reader.read_current(&path("a.ts"), &digest, 4).unwrap(),
        SourceContent::TooLarge { size: 19 }
    );

    repo.write("a.ts", "export const a = 2;");
    assert_eq!(
        reader
            .read_current(&path("a.ts"), &digest, 1024)
            .unwrap_err()
            .code(),
        "stale_inputs"
    );
    repo.remove("a.ts");
    assert_eq!(
        reader
            .read_current(&path("a.ts"), &digest, 1024)
            .unwrap_err()
            .code(),
        "stale_inputs"
    );

    let object_id = &snapshot.baseline_files()[&path("a.ts")].object_id;
    assert_eq!(
        reader.read_baseline(&path("a.ts"), object_id, 4).unwrap(),
        SourceContent::TooLarge { size: 19 }
    );
    assert!(reader.read_baseline(&path("a.ts"), "--help", 1024).is_err());
}
