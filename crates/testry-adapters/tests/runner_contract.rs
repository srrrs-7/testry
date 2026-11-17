//! Contract tests against the real runners (spec section 9 and 19).
//!
//! These tests start `bun test`, Vitest, Storybook stories (through the
//! Storybook Vitest addon, in a real browser) and Playwright, and compare
//! what they really do with what the adapters assume: the meaning of file
//! arguments, the report shape, exit codes, focused tests, setup projects,
//! leftover processes and module resolution.
//!
//! They need the dev container: Bun, Node and Chromium, and the fixture
//! dependencies at `TESTRY_RUNNER_FIXTURE_MODULES`. Outside of it they fail;
//! they are never skipped silently. They need no network.
//!
//! Verified versions: Bun 1.4.4, Vitest 5.1.0, Storybook 10.6.7 and
//! Playwright 1.65.1 on Node 26.13.1.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use testry_adapters::analysis::oxc::OxcDependencyAnalyzer;
use testry_adapters::runners::bun::BunAdapter;
use testry_adapters::runners::command::CommandAdapter;
use testry_adapters::runners::playwright::PlaywrightAdapter;
use testry_adapters::runners::storybook::StorybookAdapter;
use testry_adapters::runners::vitest::VitestAdapter;
use testry_adapters::runtime::host::HostRuntime;
use testry_adapters::source::git::GitSourceRepository;
use testry_adapters::source::reader::GitWorkspaceReader;
use testry_adapters::storage::run_files::RunDirectory;
use testry_application::ports::execution::{
    ArtifactStore, CommandSpec, EnvironmentLease, EnvironmentRequest, FileAttribution,
    LeaseOwnership, OutputPolicy, RunnerAdapter, RuntimeProvider, SuiteResult,
};
use testry_application::ports::planning::{
    AnalysisLimits, AnalysisProfile, DependencyAnalyzer, ResolverProfile, SnapshotRequest,
    SnapshotSide, SourceRepository,
};
use testry_application::results::anomaly;
use testry_application::settings::test_support::suite_parts;
use testry_application::testing::context;
use testry_domain::environment::ProviderKind;
use testry_domain::fingerprint::Fingerprint;
use testry_domain::ids::{EnvironmentLeaseId, RunId, RunnerProject, TaskId, WorkspaceId};
use testry_domain::paths::{RepoDir, RepoPath};
use testry_domain::pattern::PatternSet;
use testry_domain::plan::Mode;
use testry_domain::results::{CaseStatus, TaskOutcome, TestCounts};
use testry_domain::selection::Selection;
use testry_domain::suite::{RunnerKind, SuiteDefinition};

static RUN_COUNTER: AtomicU32 = AtomicU32::new(1);

fn path(raw: &str) -> RepoPath {
    RepoPath::try_new(raw).unwrap()
}

fn files(names: &[&str]) -> Selection {
    Selection::files(names.iter().map(|name| path(name))).unwrap()
}

struct Project {
    directory: tempfile::TempDir,
}

impl Project {
    fn new() -> Self {
        let modules = std::env::var("TESTRY_RUNNER_FIXTURE_MODULES").expect(
            "TESTRY_RUNNER_FIXTURE_MODULES is not set: the runner contract tests must run in the \
             dev container (make test)",
        );
        assert!(
            Path::new(&modules).join("vitest").is_dir(),
            "{modules} has no vitest"
        );
        let project = Self {
            directory: tempfile::tempdir().unwrap(),
        };
        project.write("package.json", r#"{ "name": "contract", "private": true, "type": "module", "devDependencies": { "vitest": "5.1.0" } }"#);
        std::os::unix::fs::symlink(&modules, project.root().join("node_modules")).unwrap();
        project
    }

    fn root(&self) -> &Path {
        self.directory.path()
    }

    fn write(&self, path: &str, content: &str) {
        let full = self.root().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, content).unwrap();
    }

    fn bun_test(&self, path: &str, body: &str) {
        self.write(
            path,
            &format!("import {{ describe, expect, test }} from \"bun:test\";\n{body}\n"),
        );
    }

    fn vitest_test(&self, path: &str, body: &str) {
        self.write(
            path,
            &format!("import {{ describe, expect, test }} from \"vitest\";\n{body}\n"),
        );
    }

    fn suite(&self, runner: RunnerKind, cwd: &str, project: Option<&str>) -> SuiteDefinition {
        let mut parts = suite_parts("suite", runner, &["**/*.test.ts", "**/*.test.tsx"]);
        parts.cwd = RepoDir::try_new(cwd).unwrap();
        parts.project = project.map(|name| RunnerProject::try_new(name).unwrap());
        parts.command = match runner {
            RunnerKind::Bun => vec!["bun".to_owned(), "test".to_owned()],
            RunnerKind::Vitest | RunnerKind::StorybookVitest => {
                let script = self.root().join("node_modules/vitest/vitest.mjs");
                vec![
                    "node".to_owned(),
                    script.to_str().unwrap().to_owned(),
                    "run".to_owned(),
                ]
            }
            RunnerKind::Playwright => {
                let script = self.root().join("node_modules/@playwright/test/cli.js");
                vec![
                    "node".to_owned(),
                    script.to_str().unwrap().to_owned(),
                    "test".to_owned(),
                ]
            }
            _ => vec!["sh".to_owned(), "-c".to_owned(), "exit 0".to_owned()],
        };
        parts.timeout = Duration::from_secs(120);
        SuiteDefinition::try_new(parts).unwrap()
    }

    /// A Playwright project with a setup project that the browser project
    /// depends on. `settings` are extra top-level settings of the config.
    fn playwright(&self, settings: &str) {
        self.write(
            "playwright.config.ts",
            &format!(
                "import {{ defineConfig }} from \"@playwright/test\";\n\
                 export default defineConfig({{\n  testDir: \"./tests/e2e\",\n  retries: 1,\n  {settings}\n  projects: [\n\
                 {{ name: \"setup\", testMatch: /global\\.setup\\.ts/ }},\n\
                 {{ name: \"chromium\", testMatch: /.*\\.spec\\.ts/, dependencies: [\"setup\"], use: {{ browserName: \"chromium\" }} }},\n  ],\n}});\n"
            ),
        );
        self.playwright_setup("writeFileSync(new URL(\"./.state\", import.meta.url), \"ready\");");
    }

    fn playwright_setup(&self, body: &str) {
        self.write(
            "tests/e2e/global.setup.ts",
            &format!(
                "import {{ test as setup }} from \"@playwright/test\";\nimport {{ writeFileSync }} from \"node:fs\";\n\
                 setup(\"prepare\", async () => {{ {body} }});\n"
            ),
        );
    }

    fn playwright_spec(&self, path: &str, body: &str) {
        self.write(
            path,
            &format!(
                "import {{ expect, test }} from \"@playwright/test\";\n\
                 import {{ existsSync, readFileSync, writeFileSync }} from \"node:fs\";\n{body}\n"
            ),
        );
    }

    /// A Storybook project whose stories run as the Vitest project
    /// `storybook`, next to a plain `unit` project in the same configuration.
    fn storybook(&self) {
        self.write(
            ".storybook/main.ts",
            "export default {\n  stories: [\"../src/**/*.stories.ts\"],\n  addons: [\"@storybook/addon-vitest\"],\n\
             framework: { name: \"@storybook/html-vite\", options: {} },\n  core: { disableTelemetry: true },\n};\n",
        );
        self.write(".storybook/preview.ts", "export default {};\n");
        self.write(
            "vitest.config.ts",
            "import fs from \"node:fs\";\nimport path from \"node:path\";\nimport { fileURLToPath } from \"node:url\";\n\
             import { defineConfig } from \"vitest/config\";\n\
             import { storybookTest } from \"@storybook/addon-vitest/vitest-plugin\";\n\
             import { playwright } from \"@vitest/browser-playwright\";\n\
             const dirname = path.dirname(fileURLToPath(import.meta.url));\n\
             export default defineConfig({\n\
               cacheDir: path.join(dirname, \".cache/vite\"),\n\
               // The fixture links node_modules to a directory outside the project.\n\
               server: { fs: { allow: [dirname, fs.realpathSync(path.join(dirname, \"node_modules\"))] } },\n\
               test: { projects: [\n\
                 { extends: true, plugins: [storybookTest({ configDir: path.join(dirname, \".storybook\") })],\n\
                   test: { name: \"storybook\", browser: { enabled: true, headless: true, provider: playwright({}), instances: [{ browser: \"chromium\" }] } } },\n\
                 { extends: true, test: { name: \"unit\", include: [\"src/**/*.test.ts\"] } },\n\
               ] },\n});\n",
        );
        self.write(
            "src/components/Button.stories.ts",
            "import type { Meta, StoryObj } from \"@storybook/html-vite\";\nimport { expect, within } from \"storybook/test\";\n\
             const meta: Meta = { title: \"Button\", args: { label: \"Hello\" },\n\
               render: (args) => { const button = document.createElement(\"button\"); button.textContent = String(args.label); return button; } };\n\
             export default meta;\nexport const Primary: StoryObj = {};\n\
             export const WithPlay: StoryObj = { play: async ({ canvasElement }) => {\n\
               await expect(within(canvasElement).getByRole(\"button\")).toHaveTextContent(\"Hello\"); } };\n",
        );
        self.vitest_test("src/math.test.ts", PASSING);
    }

    fn story(&self, path: &str, title: &str, stories: &str) {
        self.write(
            path,
            &format!(
                "import type {{ Meta, StoryObj }} from \"@storybook/html-vite\";\n\
                 const meta: Meta = {{ title: \"{title}\", render: () => {{ const div = document.createElement(\"div\"); div.textContent = \"{title}\"; return div; }} }};\n\
                 export default meta;\nexport const Default: StoryObj = {{}};\n{stories}\n"
            ),
        );
    }

    /// One run in a fresh run directory, exactly as the use case drives it.
    async fn run(
        &self,
        adapter: &dyn RunnerAdapter,
        suite: &SuiteDefinition,
        selection: &Selection,
    ) -> SuiteResult {
        self.run_in(adapter, suite, selection, self.execution().await)
            .await
    }

    async fn execution(&self) -> Execution {
        let number = RUN_COUNTER.fetch_add(1, Ordering::SeqCst);
        let run_id = RunId::try_new(format!("00000000-0000-4000-8000-{number:012x}")).unwrap();
        let run = RunDirectory::create(self.root(), &path(".testry"), &run_id, false).unwrap();
        let environment = HostRuntime::inherited_environment(std::env::vars());
        let runtime = HostRuntime::new(
            run.root().to_path_buf(),
            Duration::from_millis(500),
            environment,
            true,
            false,
        );
        let lease = runtime
            .prepare(
                &EnvironmentRequest {
                    provider: ProviderKind::Host,
                    workspace_root: self.root().to_path_buf(),
                    config_fingerprint: Fingerprint::of_bytes(b"contract"),
                    ownership: LeaseOwnership {
                        workspace_id: WorkspaceId::try_new("ws-contract").unwrap(),
                        run_id,
                        lease_id: EnvironmentLeaseId::try_new(
                            "11111111-1111-4111-8111-111111111111",
                        )
                        .unwrap(),
                    },
                    readiness_commands: Vec::new(),
                    trust: None,
                },
                &context(),
            )
            .await
            .unwrap();
        Execution {
            mark: format!("contract-{number}-{}", std::process::id()),
            run,
            runtime,
            lease,
        }
    }

    async fn run_in(
        &self,
        adapter: &dyn RunnerAdapter,
        suite: &SuiteDefinition,
        selection: &Selection,
        execution: Execution,
    ) -> SuiteResult {
        let task = TaskId::for_suite(suite.id());
        let artifacts = execution
            .run
            .task_artifacts(&task, "report.out")
            .await
            .unwrap();
        let invocation = adapter
            .prepare(
                suite,
                selection,
                &execution.lease.descriptor,
                &artifacts.locations,
            )
            .unwrap();
        let (program, args) = invocation.argv.split_first().unwrap();
        let mut env = invocation.env.clone();
        env.insert(CONTRACT_MARK.to_owned(), execution.mark.clone());
        let command = CommandSpec {
            program: program.clone(),
            args: args.to_vec(),
            cwd: invocation.cwd.clone(),
            env,
            timeout: suite.timeout(),
            output: OutputPolicy {
                stdout: artifacts.stdout.clone(),
                stderr: artifacts.stderr.clone(),
                max_bytes_per_stream: 1024 * 1024,
            },
        };
        let process = execution
            .runtime
            .execute(&execution.lease, &command, &execution.run, &context())
            .await
            .unwrap();
        let reports = execution.run.read_report(&invocation).await.unwrap();
        let result = adapter.normalize(&invocation, &process, &reports).unwrap();
        if std::env::var("TESTRY_CONTRACT_DEBUG").is_ok() {
            eprintln!(
                "argv: {:?}\nexit: {:?}\nstdout:\n{}\nstderr:\n{}",
                invocation.argv,
                process.exit,
                std::fs::read_to_string(execution.run.root().join(&artifacts.stdout.relative_path))
                    .unwrap_or_default(),
                std::fs::read_to_string(execution.run.root().join(&artifacts.stderr.relative_path))
                    .unwrap_or_default(),
            );
        }
        result
    }

    /// The version the adapter reads from the installed runner.
    async fn probe(&self, adapter: &dyn RunnerAdapter, suite: &SuiteDefinition) -> Option<String> {
        let execution = self.execution().await;
        let task = TaskId::for_suite(suite.id());
        let artifacts = execution
            .run
            .task_artifacts(&task, "report.out")
            .await
            .unwrap();
        let argv = adapter.version_command(suite).unwrap();
        let (program, args) = argv.split_first().unwrap();
        let command = CommandSpec {
            program: program.clone(),
            args: args.to_vec(),
            cwd: suite.cwd().clone(),
            env: BTreeMap::new(),
            timeout: Duration::from_secs(60),
            output: OutputPolicy {
                stdout: artifacts.probe_stdout.clone(),
                stderr: artifacts.probe_stderr.clone(),
                max_bytes_per_stream: 64 * 1024,
            },
        };
        let process = execution
            .runtime
            .execute(&execution.lease, &command, &execution.run, &context())
            .await
            .unwrap();
        assert!(
            process.exit.is_success(),
            "the version probe failed: {:?}",
            process.exit
        );
        let output = execution
            .run
            .read_text(&artifacts.probe_stdout, 64 * 1024)
            .await
            .unwrap();
        adapter
            .parse_version(&output)
            .map(|version| version.version)
    }
}

struct Execution {
    /// The value of `CONTRACT_MARK` in the environment of this run's command.
    mark: String,
    run: RunDirectory,
    runtime: HostRuntime,
    lease: EnvironmentLease,
}

/// A variable the tests add to a command, independent of the marker testry
/// itself uses, to look for processes the command left behind.
const CONTRACT_MARK: &str = "TESTRY_CONTRACT_MARK";

/// Names of the processes that still carry the mark of a run.
fn processes_marked(mark: &str) -> Vec<String> {
    let marker = format!("{CONTRACT_MARK}={mark}");
    std::fs::read_dir("/proc")
        .expect("/proc must be readable in the test container")
        .flatten()
        .filter_map(|entry| {
            let environment = std::fs::read(entry.path().join("environ")).ok()?;
            environment
                .split(|byte| *byte == 0)
                .any(|variable| variable == marker.as_bytes())
                .then(|| {
                    std::fs::read_to_string(entry.path().join("comm"))
                        .unwrap_or_default()
                        .trim()
                        .to_owned()
                })
        })
        .collect()
}

fn wait_until_unmarked(mark: &str) -> Vec<String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let left = processes_marked(mark);
        if left.is_empty() || std::time::Instant::now() >= deadline {
            return left;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn case<'a>(result: &'a SuiteResult, title: &[&str]) -> &'a testry_domain::results::CaseResult {
    result
        .cases
        .iter()
        .find(|case| case.title_path == title)
        .unwrap_or_else(|| panic!("no case {title:?} in {:?}", result.cases))
}

fn executed(result: &SuiteResult) -> Vec<&str> {
    match &result.executed_files {
        FileAttribution::Known(files) => files.iter().map(RepoPath::as_str).collect(),
        FileAttribution::Unknown => panic!("the runner did not attribute files: {result:?}"),
    }
}

fn codes(result: &SuiteResult) -> Vec<&'static str> {
    result
        .anomalies
        .iter()
        .map(|anomaly| anomaly.code)
        .collect()
}

fn counts(result: &SuiteResult) -> TestCounts {
    result
        .counts
        .unwrap_or_else(|| panic!("no counts: {result:?}"))
}

const PASSING: &str = "test(\"passes\", () => { expect(1 + 1).toBe(2); });";
const FAILING: &str = "test(\"fails\", () => { expect(1).toBe(2); });";

// ------------------------------------------------------------------ Bun --

#[tokio::test]
async fn bun_installed_version_is_the_verified_one() {
    let project = Project::new();
    let suite = project.suite(RunnerKind::Bun, ".", None);
    let version = project
        .probe(&BunAdapter, &suite)
        .await
        .expect("bun --version is readable");
    assert!(
        testry_adapters::runners::bun::VERIFIED_VERSIONS.contains(&version.as_str()),
        "the image has Bun {version}, which is not in VERIFIED_VERSIONS; re-run the contract tests \
         and update compatibility.toml"
    );
}

/// The `./` prefix makes Bun run exactly the named files, also for names
/// that a filter or an option parser would treat specially.
#[tokio::test]
async fn bun_runs_exactly_the_selected_files() {
    let project = Project::new();
    let names = [
        "src/a.test.ts",
        "src/a.test.tsx",
        "src/aa.test.ts",
        "src/nested/a.test.ts",
        "src/with space/日本語 [x].test.ts",
        "src/-dash.test.ts",
    ];
    for name in names {
        project.bun_test(name, PASSING);
    }
    let suite = project.suite(RunnerKind::Bun, ".", None);

    let single = project
        .run(&BunAdapter, &suite, &files(&["src/a.test.ts"]))
        .await;
    assert_eq!(single.outcome, TaskOutcome::Passed, "{single:?}");
    assert_eq!(
        executed(&single),
        ["src/a.test.ts"],
        "a.test.tsx and aa.test.ts must not run"
    );
    assert!(single.anomalies.is_empty());

    let unusual = [
        "src/-dash.test.ts",
        "src/a.test.tsx",
        "src/with space/日本語 [x].test.ts",
    ];
    let several = project.run(&BunAdapter, &suite, &files(&unusual)).await;
    assert_eq!(several.outcome, TaskOutcome::Passed, "{several:?}");
    assert_eq!(executed(&several), unusual);

    let all = project.run(&BunAdapter, &suite, &Selection::All).await;
    assert_eq!(all.outcome, TaskOutcome::Passed);
    let mut expected = names.to_vec();
    expected.sort_unstable();
    assert_eq!(executed(&all), expected);
    assert_eq!(counts(&all).passed, 6);
}

#[tokio::test]
async fn bun_paths_are_relative_to_the_suite_directory() {
    let project = Project::new();
    project.bun_test("apps/api/src/a.test.ts", PASSING);
    project.bun_test("apps/api/src/b.test.ts", PASSING);
    project.bun_test("apps/web/src/c.test.ts", PASSING);
    let suite = project.suite(RunnerKind::Bun, "apps/api", None);

    let selected = project
        .run(&BunAdapter, &suite, &files(&["apps/api/src/b.test.ts"]))
        .await;
    assert_eq!(selected.outcome, TaskOutcome::Passed, "{selected:?}");
    assert_eq!(executed(&selected), ["apps/api/src/b.test.ts"]);

    // The whole suite is what Bun discovers below the suite directory.
    let all = project.run(&BunAdapter, &suite, &Selection::All).await;
    assert_eq!(
        executed(&all),
        ["apps/api/src/a.test.ts", "apps/api/src/b.test.ts"]
    );
}

#[tokio::test]
async fn bun_failures_skips_and_todos_are_reported_as_such() {
    let project = Project::new();
    project.bun_test(
        "src/pass.test.ts",
        "describe(\"group\", () => { test(\"inner\", () => {}); });",
    );
    project.bun_test("src/fail.test.ts", FAILING);
    project.bun_test(
        "src/skip.test.ts",
        "test.skip(\"later\", () => {});\ntest.todo(\"someday\");\ntest(\"ok\", () => {});",
    );
    let suite = project.suite(RunnerKind::Bun, ".", None);

    let result = project.run(&BunAdapter, &suite, &Selection::All).await;
    assert_eq!(result.outcome, TaskOutcome::Failed);
    assert_eq!(
        counts(&result),
        TestCounts {
            passed: 2,
            failed: 1,
            skipped: 2,
            flaky: 0
        }
    );
    let titles: BTreeSet<Vec<String>> = result
        .cases
        .iter()
        .map(|case| case.title_path.clone())
        .collect();
    assert!(titles.contains(&vec!["group".to_owned(), "inner".to_owned()]));
    let failed = result
        .cases
        .iter()
        .find(|case| case.title_path == ["fails"])
        .unwrap();
    assert_eq!(failed.file.as_ref().unwrap().as_str(), "src/fail.test.ts");
    assert!(failed.failure.as_ref().unwrap().contains("Expected"));
}

/// P09: Bun leaves a file without tests, and a file that fails to load, out
/// of its report. A requested file that is missing is never a pass.
#[tokio::test]
async fn p09_bun_file_missing_from_the_report_is_not_a_pass() {
    let project = Project::new();
    project.bun_test("src/ok.test.ts", PASSING);
    project.write("src/empty.test.ts", "export {};\n");
    project.write(
        "src/throws.test.ts",
        "throw new Error(\"boom at import\");\n",
    );
    project.write("src/syntax.test.ts", "import {\n");
    let suite = project.suite(RunnerKind::Bun, ".", None);

    // A file with zero tests: Bun exits with 0.
    let empty = project
        .run(
            &BunAdapter,
            &suite,
            &files(&["src/empty.test.ts", "src/ok.test.ts"]),
        )
        .await;
    assert_eq!(empty.outcome, TaskOutcome::InfrastructureError, "{empty:?}");
    assert_eq!(codes(&empty), [anomaly::EXPECTED_FILES_MISSING]);
    assert_eq!(executed(&empty), ["src/ok.test.ts"]);

    for broken in ["src/throws.test.ts", "src/syntax.test.ts"] {
        let result = project
            .run(&BunAdapter, &suite, &files(&[broken, "src/ok.test.ts"]))
            .await;
        assert_eq!(result.outcome, TaskOutcome::Failed, "{broken}: {result:?}");
        assert!(
            codes(&result).contains(&anomaly::EXPECTED_FILES_MISSING),
            "{broken}"
        );
    }
}

/// P08: no test at all, or a filter without a match: Bun writes no report.
#[tokio::test]
async fn p08_bun_without_any_test_is_an_infrastructure_error() {
    let project = Project::new();
    project.write("src/not-a-test.ts", "export {};\n");
    let suite = project.suite(RunnerKind::Bun, ".", None);
    let nothing = project.run(&BunAdapter, &suite, &Selection::All).await;
    assert_eq!(
        nothing.outcome,
        TaskOutcome::InfrastructureError,
        "{nothing:?}"
    );
    assert_eq!(codes(&nothing), [anomaly::REPORT_MISSING]);

    let missing = project
        .run(&BunAdapter, &suite, &files(&["src/gone.test.ts"]))
        .await;
    assert_eq!(missing.outcome, TaskOutcome::InfrastructureError);
}

/// A focused test would silently drop its siblings from the run and from the
/// report. The adapter makes Bun refuse it.
#[tokio::test]
async fn bun_focused_tests_fail_the_suite() {
    let project = Project::new();
    project.bun_test(
        "src/focus.test.ts",
        "test.only(\"focused\", () => {});\ntest(\"dropped\", () => {});",
    );
    let suite = project.suite(RunnerKind::Bun, ".", None);
    let result = project.run(&BunAdapter, &suite, &Selection::All).await;
    assert_ne!(result.outcome, TaskOutcome::Passed, "{result:?}");
    assert!(
        matches!(
            result.outcome,
            TaskOutcome::Failed | TaskOutcome::InfrastructureError
        ),
        "{result:?}"
    );
}

/// P15: a report left behind by an earlier run is never read as the result
/// of a new one, because every run has its own artifact directory.
#[tokio::test]
async fn p15_a_stale_report_is_not_reused() {
    let project = Project::new();
    project.bun_test("src/a.test.ts", PASSING);
    let suite = project.suite(RunnerKind::Bun, ".", None);
    let first = project.execution().await;
    let first_root = first.run.root().to_path_buf();
    let passed = project
        .run_in(&BunAdapter, &suite, &Selection::All, first)
        .await;
    assert_eq!(passed.outcome, TaskOutcome::Passed);
    assert!(first_root.join("tasks/suite/report.out").is_file());

    // Now the suite produces no report at all.
    std::fs::remove_file(project.root().join("src/a.test.ts")).unwrap();
    let second = project.execution().await;
    assert_ne!(second.run.root(), first_root);
    let result = project
        .run_in(&BunAdapter, &suite, &Selection::All, second)
        .await;
    assert_eq!(
        result.outcome,
        TaskOutcome::InfrastructureError,
        "{result:?}"
    );
    assert_eq!(codes(&result), [anomaly::REPORT_MISSING]);
}

// --------------------------------------------------------------- Vitest --

#[tokio::test]
async fn vitest_installed_version_is_the_verified_one() {
    let project = Project::new();
    let suite = project.suite(RunnerKind::Vitest, ".", None);
    let version = project
        .probe(&VitestAdapter, &suite)
        .await
        .expect("the vitest version is readable");
    assert!(
        testry_adapters::runners::vitest::VERIFIED_VERSIONS.contains(&version.as_str()),
        "the image has Vitest {version}, which is not in VERIFIED_VERSIONS"
    );
}

/// Vitest filters are substring matches: the executed set is a superset of
/// the request, and every requested file is in it.
#[tokio::test]
async fn vitest_runs_a_superset_that_contains_every_selected_file() {
    let project = Project::new();
    let names = [
        "src/a.test.ts",
        "src/a.test.tsx",
        "src/aa.test.ts",
        "src/b.test.ts",
        "src/with space/日本語 [x].test.ts",
    ];
    for name in names {
        project.vitest_test(name, PASSING);
    }
    let suite = project.suite(RunnerKind::Vitest, ".", None);

    let single = project
        .run(&VitestAdapter, &suite, &files(&["src/a.test.ts"]))
        .await;
    assert_eq!(single.outcome, TaskOutcome::Passed, "{single:?}");
    let ran = executed(&single);
    assert!(ran.contains(&"src/a.test.ts"));
    assert!(
        ran.contains(&"src/a.test.tsx"),
        "the filter is a substring match in Vitest 5.1.0: {ran:?}"
    );
    assert!(!ran.contains(&"src/b.test.ts"));
    assert!(
        single.anomalies.is_empty(),
        "a superset is expected, not an anomaly"
    );

    let unusual = project
        .run(
            &VitestAdapter,
            &suite,
            &files(&["src/with space/日本語 [x].test.ts", "src/b.test.ts"]),
        )
        .await;
    assert_eq!(unusual.outcome, TaskOutcome::Passed, "{unusual:?}");
    assert_eq!(
        executed(&unusual),
        ["src/b.test.ts", "src/with space/日本語 [x].test.ts"]
    );

    let all = project.run(&VitestAdapter, &suite, &Selection::All).await;
    let mut expected = names.to_vec();
    expected.sort_unstable();
    assert_eq!(executed(&all), expected);
}

/// P10: a test that passes on a retry is flaky, with the runner's own retries.
#[tokio::test]
async fn p10_vitest_retried_tests_are_flaky_and_statuses_are_kept() {
    let project = Project::new();
    project.vitest_test(
        "src/mixed.test.ts",
        "let attempts = 0;\n\
         test(\"flaky\", { retry: 2 }, () => { attempts += 1; if (attempts < 2) throw new Error(\"first\"); });\n\
         test.skip(\"later\", () => {});\n\
         test.todo(\"someday\");\n\
         describe(\"group\", () => { test(\"inner\", () => {}); });",
    );
    let suite = project.suite(RunnerKind::Vitest, ".", None);
    let result = project.run(&VitestAdapter, &suite, &Selection::All).await;
    assert_eq!(result.outcome, TaskOutcome::Flaky, "{result:?}");
    assert_eq!(
        counts(&result),
        TestCounts {
            passed: 1,
            failed: 0,
            skipped: 2,
            flaky: 1
        }
    );
    assert!(
        result
            .cases
            .iter()
            .any(|case| case.title_path == ["group", "inner"])
    );

    project.vitest_test("src/fail.test.ts", FAILING);
    let failed = project.run(&VitestAdapter, &suite, &Selection::All).await;
    assert_eq!(failed.outcome, TaskOutcome::Failed);
    assert_eq!(counts(&failed).failed, 1);
}

#[tokio::test]
async fn vitest_files_that_cannot_run_are_failures() {
    let project = Project::new();
    project.vitest_test("src/ok.test.ts", PASSING);
    project.write("src/empty.test.ts", "export {};\n");
    project.write(
        "src/throws.test.ts",
        "throw new Error(\"boom at import\");\n",
    );
    let suite = project.suite(RunnerKind::Vitest, ".", None);

    for broken in ["src/empty.test.ts", "src/throws.test.ts"] {
        let result = project
            .run(&VitestAdapter, &suite, &files(&[broken, "src/ok.test.ts"]))
            .await;
        assert_eq!(result.outcome, TaskOutcome::Failed, "{broken}: {result:?}");
        assert!(
            executed(&result).contains(&broken),
            "Vitest lists the failing file"
        );
        let file_failure = result
            .cases
            .iter()
            .find(|case| case.title_path.is_empty())
            .unwrap();
        assert_eq!(file_failure.file.as_ref().unwrap().as_str(), broken);
    }

    // P09: a requested file that does not exist is not a pass either.
    let missing = project
        .run(
            &VitestAdapter,
            &suite,
            &files(&["src/gone.test.ts", "src/ok.test.ts"]),
        )
        .await;
    assert_ne!(missing.outcome, TaskOutcome::Passed, "{missing:?}");
    assert!(codes(&missing).contains(&anomaly::EXPECTED_FILES_MISSING));
}

#[tokio::test]
async fn vitest_focused_tests_fail_the_suite() {
    let project = Project::new();
    project.vitest_test(
        "src/focus.test.ts",
        "test.only(\"focused\", () => {});\ntest(\"dropped\", () => {});",
    );
    let suite = project.suite(RunnerKind::Vitest, ".", None);
    let result = project.run(&VitestAdapter, &suite, &Selection::All).await;
    assert_eq!(result.outcome, TaskOutcome::Failed, "{result:?}");
}

/// P11: one project per invocation; the same path in two projects is two runs.
#[tokio::test]
async fn p11_vitest_projects_are_run_separately() {
    let project = Project::new();
    project.write(
        "vitest.config.ts",
        "import { defineConfig } from \"vitest/config\";\n\
         export default defineConfig({ test: { projects: [\n\
           { test: { name: \"unit\", include: [\"src/**/*.test.ts\"] } },\n\
           { test: { name: \"stories\", include: [\"stories/**/*.test.ts\"] } },\n\
         ] } });\n",
    );
    project.vitest_test("src/a.test.ts", PASSING);
    project.vitest_test("stories/a.test.ts", FAILING);

    let unit = project.suite(RunnerKind::Vitest, ".", Some("unit"));
    let result = project.run(&VitestAdapter, &unit, &Selection::All).await;
    assert_eq!(result.outcome, TaskOutcome::Passed, "{result:?}");
    assert_eq!(executed(&result), ["src/a.test.ts"]);

    let stories = project.suite(RunnerKind::Vitest, ".", Some("stories"));
    let result = project.run(&VitestAdapter, &stories, &Selection::All).await;
    assert_eq!(result.outcome, TaskOutcome::Failed);
    assert_eq!(executed(&result), ["stories/a.test.ts"]);

    // A project that does not exist produces no report: never a pass.
    let unknown = project.suite(RunnerKind::Vitest, ".", Some("nope"));
    let result = project.run(&VitestAdapter, &unknown, &Selection::All).await;
    assert_eq!(
        result.outcome,
        TaskOutcome::InfrastructureError,
        "{result:?}"
    );
}

// ----------------------------------------------------------- Playwright --

const E2E_PASSING: &str = "test.describe(\"login page\", () => {\n\
  test(\"renders\", async ({ page }) => {\n\
    await page.setContent(\"<h1>hello</h1>\");\n\
    await expect(page.locator(\"h1\")).toHaveText(\"hello\");\n\
  });\n\
  test(\"setup ran first\", () => { expect(readFileSync(new URL(\"../.state\", import.meta.url), \"utf8\")).toBe(\"ready\"); });\n\
});";

const E2E_MIXED: &str = "test(\"fails\", () => { expect(1).toBe(2); });\n\
test.skip(\"skipped\", () => {});\n\
test.fixme(\"fixme\", () => {});\n\
test(\"flaky\", () => {\n\
  const marker = new URL(\"./.flaky\", import.meta.url);\n\
  if (!existsSync(marker)) { writeFileSync(marker, \"x\"); throw new Error(\"first attempt\"); }\n\
});\n\
test(\"expected failure\", () => { test.fail(); expect(1).toBe(2); });";

#[tokio::test]
async fn playwright_installed_version_is_the_verified_one() {
    let project = Project::new();
    let suite = project.suite(RunnerKind::Playwright, ".", None);
    let version = project
        .probe(&PlaywrightAdapter, &suite)
        .await
        .expect("the Playwright version is readable");
    assert!(
        testry_adapters::runners::playwright::VERIFIED_VERSIONS.contains(&version.as_str()),
        "the image has Playwright {version}, which is not in VERIFIED_VERSIONS"
    );
}

/// The whole suite runs in a real browser, with the setup project first, and
/// every status of the report stays what it is.
#[tokio::test]
async fn playwright_runs_the_whole_suite_and_keeps_statuses_apart() {
    let project = Project::new();
    project.playwright("");
    project.playwright_spec("tests/e2e/auth/login.spec.ts", E2E_PASSING);
    project.playwright_spec("tests/e2e/mixed.spec.ts", E2E_MIXED);
    let suite = project.suite(RunnerKind::Playwright, ".", None);

    let result = project
        .run(&PlaywrightAdapter, &suite, &Selection::All)
        .await;
    assert_eq!(result.outcome, TaskOutcome::Failed, "{result:?}");
    assert_eq!(
        executed(&result),
        [
            "tests/e2e/auth/login.spec.ts",
            "tests/e2e/global.setup.ts",
            "tests/e2e/mixed.spec.ts"
        ]
    );
    let counts = counts(&result);
    assert_eq!(
        (counts.passed, counts.failed, counts.skipped, counts.flaky),
        (3, 1, 3, 1),
        "skip, fixme and the expected failure are never counted as passed"
    );

    // The page really rendered in the browser, after the setup project.
    assert_eq!(
        case(&result, &["login page", "renders"]).status,
        CaseStatus::Passed
    );
    assert_eq!(
        case(&result, &["login page", "setup ran first"]).status,
        CaseStatus::Passed
    );
    assert_eq!(
        case(&result, &["login page", "renders"]).project.as_deref(),
        Some("chromium")
    );
    assert_eq!(
        case(&result, &["prepare"]).project.as_deref(),
        Some("setup")
    );

    assert_eq!(case(&result, &["fails"]).status, CaseStatus::Failed);
    assert_eq!(case(&result, &["skipped"]).status, CaseStatus::Skipped);
    assert_eq!(case(&result, &["fixme"]).status, CaseStatus::Todo);
    // P10: a pass on the retry stays flaky.
    assert_eq!(case(&result, &["flaky"]).status, CaseStatus::Flaky);
    assert_eq!(
        case(&result, &["expected failure"]).status,
        CaseStatus::ExpectedFailure
    );
    assert!(
        !codes(&result).contains(&anomaly::SCOPE_LIMITED_BY_RUNNER_CONFIG),
        "{:?}",
        result.anomalies
    );

    // Traces and other output belong to the run, not to the work tree.
    assert!(
        !project.root().join("test-results").exists(),
        "the runner wrote into the work tree"
    );
}

#[tokio::test]
async fn playwright_passing_suite_passes() {
    let project = Project::new();
    project.playwright("");
    project.playwright_spec("tests/e2e/auth/login.spec.ts", E2E_PASSING);
    let suite = project.suite(RunnerKind::Playwright, ".", None);
    let result = project
        .run(&PlaywrightAdapter, &suite, &Selection::All)
        .await;
    assert_eq!(result.outcome, TaskOutcome::Passed, "{result:?}");
    assert_eq!(counts(&result).passed, 3);
    assert!(result.anomalies.is_empty(), "{:?}", result.anomalies);
}

/// R18: the setup project is never skipped, and when it fails nothing that
/// depends on it counts as run.
#[tokio::test]
async fn r18_playwright_setup_failure_is_not_a_pass() {
    let project = Project::new();
    project.playwright("");
    project.playwright_setup("throw new Error(\"setup broke\");");
    project.playwright_spec("tests/e2e/auth/login.spec.ts", E2E_PASSING);
    let suite = project.suite(RunnerKind::Playwright, ".", None);

    let result = project
        .run(&PlaywrightAdapter, &suite, &Selection::All)
        .await;
    assert_eq!(result.outcome, TaskOutcome::Failed, "{result:?}");
    assert_eq!(case(&result, &["prepare"]).status, CaseStatus::Failed);
    let dependent = case(&result, &["login page", "renders"]);
    assert_eq!(dependent.status, CaseStatus::Skipped);
    assert!(
        dependent
            .failure
            .as_deref()
            .unwrap()
            .contains("did not run")
    );
    assert_eq!(counts(&result).passed, 0);
}

/// R18: selecting one project still runs the project it depends on.
#[tokio::test]
async fn r18_playwright_project_selection_keeps_dependencies() {
    let project = Project::new();
    project.playwright("");
    project.playwright_spec("tests/e2e/auth/login.spec.ts", E2E_PASSING);
    let suite = project.suite(RunnerKind::Playwright, ".", Some("chromium"));

    let result = project
        .run(&PlaywrightAdapter, &suite, &Selection::All)
        .await;
    assert_eq!(result.outcome, TaskOutcome::Passed, "{result:?}");
    assert_eq!(
        case(&result, &["prepare"]).status,
        CaseStatus::Passed,
        "the setup project ran although only `chromium` was selected"
    );
    assert_eq!(counts(&result).passed, 3);
}

#[tokio::test]
async fn playwright_focused_tests_fail_the_suite() {
    let project = Project::new();
    project.playwright("");
    project.playwright_spec(
        "tests/e2e/focused.spec.ts",
        "test.only(\"focused\", () => {});\ntest(\"dropped by the focus\", () => {});",
    );
    let suite = project.suite(RunnerKind::Playwright, ".", None);
    let result = project
        .run(&PlaywrightAdapter, &suite, &Selection::All)
        .await;
    assert_eq!(result.outcome, TaskOutcome::Failed, "{result:?}");
    assert!(
        result.cases.iter().any(|case| case
            .failure
            .as_deref()
            .is_some_and(|message| message.contains(".only"))),
        "{:?}",
        result.cases
    );
}

/// P08: a suite without any test is not a pass.
#[tokio::test]
async fn p08_playwright_without_any_test_is_not_a_pass() {
    let project = Project::new();
    project.playwright("");
    std::fs::remove_file(project.root().join("tests/e2e/global.setup.ts")).unwrap();
    let suite = project.suite(RunnerKind::Playwright, ".", None);
    let result = project
        .run(&PlaywrightAdapter, &suite, &Selection::All)
        .await;
    assert_eq!(result.outcome, TaskOutcome::Failed, "{result:?}");
    assert_eq!(counts(&result).passed, 0);
}

#[tokio::test]
async fn playwright_file_that_cannot_be_loaded_is_a_failure() {
    let project = Project::new();
    project.playwright("");
    project.playwright_spec("tests/e2e/auth/login.spec.ts", E2E_PASSING);
    project.write("tests/e2e/broken.spec.ts", "import {\n");
    let suite = project.suite(RunnerKind::Playwright, ".", None);
    let result = project
        .run(&PlaywrightAdapter, &suite, &Selection::All)
        .await;
    assert_eq!(result.outcome, TaskOutcome::Failed, "{result:?}");
    assert!(
        executed(&result).contains(&"tests/e2e/broken.spec.ts"),
        "{result:?}"
    );
    assert_eq!(
        counts(&result).passed,
        0,
        "nothing runs when one file cannot be loaded"
    );
}

/// A shard in the runner's own configuration narrows the suite. The result
/// stands, but the narrowing is stated with it.
#[tokio::test]
async fn playwright_narrowing_configuration_is_reported() {
    let project = Project::new();
    project.playwright("shard: { total: 2, current: 1 },");
    project.playwright_spec("tests/e2e/auth/login.spec.ts", E2E_PASSING);
    project.playwright_spec("tests/e2e/other.spec.ts", "test(\"other\", () => {});");
    let suite = project.suite(RunnerKind::Playwright, ".", None);
    let result = project
        .run(&PlaywrightAdapter, &suite, &Selection::All)
        .await;
    assert_eq!(result.outcome, TaskOutcome::Passed, "{result:?}");
    let notes: Vec<&str> = result
        .anomalies
        .iter()
        .filter(|anomaly| anomaly.code == anomaly::SCOPE_LIMITED_BY_RUNNER_CONFIG)
        .map(|anomaly| anomaly.detail.as_str())
        .collect();
    assert_eq!(notes.len(), 1, "{:?}", result.anomalies);
    assert!(notes[0].contains("shard"));
}

/// R05: a timeout stops the runner, its browser and the server it started,
/// although the browser and the server run outside its process group.
#[tokio::test]
async fn r05_playwright_timeout_leaves_no_browser_or_server_behind() {
    let project = Project::new();
    project.write(
        "server.mjs",
        "import { createServer } from \"node:http\";\n\
         createServer((request, response) => { response.end(\"<h1>served</h1>\"); }).listen(0, \"127.0.0.1\", function () {\n\
           console.log(`listening ${this.address().port}`);\n});\n",
    );
    project.write(
        "playwright.config.ts",
        "import { defineConfig } from \"@playwright/test\";\n\
         export default defineConfig({\n  testDir: \"./tests/e2e\",\n\
           webServer: { command: \"node server.mjs\", wait: { stdout: /listening (?<port>\\d+)/ }, gracefulShutdown: { signal: \"SIGTERM\", timeout: 500 } },\n\
           projects: [{ name: \"chromium\", use: { browserName: \"chromium\" } }],\n});\n",
    );
    project.playwright_spec(
        "tests/e2e/slow.spec.ts",
        "test(\"never ends\", async ({ page }) => {\n  test.setTimeout(600000);\n\
           await page.setContent(\"<h1>open</h1>\");\n  await page.waitForTimeout(500000);\n});",
    );
    let mut parts = suite_parts("suite", RunnerKind::Playwright, &["**/*.spec.ts"]);
    parts.command = project
        .suite(RunnerKind::Playwright, ".", None)
        .command()
        .to_vec();
    parts.timeout = Duration::from_secs(8);
    let suite = SuiteDefinition::try_new(parts).unwrap();

    let execution = project.execution().await;
    let mark = execution.mark.clone();
    let observed_running = {
        let mark = mark.clone();
        tokio::task::spawn_blocking(move || {
            // While the test hangs, a browser of this run must be alive;
            // otherwise the check below would prove nothing.
            let deadline = std::time::Instant::now() + Duration::from_secs(7);
            while std::time::Instant::now() < deadline {
                let names = processes_marked(&mark);
                if names.iter().any(|name| name.contains("chrome")) {
                    return names;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Vec::new()
        })
    };
    let result = project
        .run_in(&PlaywrightAdapter, &suite, &Selection::All, execution)
        .await;
    let during = observed_running.await.unwrap();
    assert!(
        during.iter().any(|name| name.contains("chrome")),
        "no browser of this run was seen while the test was hanging: {during:?}"
    );
    assert!(
        during.len() >= 3,
        "runner, server and browser were expected: {during:?}"
    );

    assert_eq!(result.outcome, TaskOutcome::TimedOut, "{result:?}");
    assert!(
        !codes(&result).contains(&anomaly::RESIDUAL_PROCESSES),
        "{:?}",
        result.anomalies
    );
    let left = wait_until_unmarked(&mark);
    assert!(left.is_empty(), "processes survived the timeout: {left:?}");
}

// ------------------------------------------------------------ Storybook --

/// Every story of the stories project runs as a test in a real browser, and
/// nothing of another project of the same Vitest configuration runs.
#[tokio::test]
async fn storybook_runs_the_stories_project_as_a_whole() {
    let project = Project::new();
    project.storybook();
    project.story(
        "src/Card.stories.ts",
        "Card",
        "export const Broken: StoryObj = { play: async () => { throw new Error(\"boom\"); } };",
    );
    let suite = project.suite(RunnerKind::StorybookVitest, ".", Some("storybook"));

    // Asking for one story file still runs the whole project.
    let result = project
        .run(
            &StorybookAdapter,
            &suite,
            &files(&["src/components/Button.stories.ts"]),
        )
        .await;
    assert_eq!(result.outcome, TaskOutcome::Failed, "{result:?}");
    assert_eq!(
        executed(&result),
        ["src/Card.stories.ts", "src/components/Button.stories.ts"],
        "all stories ran, and the unit test of the other project did not"
    );
    assert!(codes(&result).contains(&anomaly::SELECTION_WIDENED));
    let counts = counts(&result);
    assert_eq!((counts.passed, counts.failed), (3, 1));

    // A story with a play function is a test like any other.
    assert_eq!(case(&result, &["With Play"]).status, CaseStatus::Passed);
    let broken = case(&result, &["Broken"]);
    assert_eq!(broken.status, CaseStatus::Failed);
    assert!(broken.failure.as_deref().unwrap().contains("boom"));
    assert_eq!(
        broken.file.as_ref().unwrap().as_str(),
        "src/Card.stories.ts"
    );
}

#[tokio::test]
async fn storybook_passing_stories_pass() {
    let project = Project::new();
    project.storybook();
    project.story("src/Card.stories.ts", "Card", "");
    let suite = project.suite(RunnerKind::StorybookVitest, ".", Some("storybook"));
    let result = project
        .run(&StorybookAdapter, &suite, &Selection::All)
        .await;
    assert_eq!(result.outcome, TaskOutcome::Passed, "{result:?}");
    assert_eq!(counts(&result).passed, 3);
    assert!(result.anomalies.is_empty(), "{:?}", result.anomalies);
}

/// A story file that cannot be loaded stops the runner before it writes its
/// report. Without a report nothing is known: that is not a result.
#[tokio::test]
async fn storybook_story_file_that_cannot_be_loaded_is_not_a_pass() {
    let project = Project::new();
    project.storybook();
    project.write("src/Card.stories.ts", "export const Broken = {\n");
    let suite = project.suite(RunnerKind::StorybookVitest, ".", Some("storybook"));
    let result = project
        .run(&StorybookAdapter, &suite, &Selection::All)
        .await;
    assert_eq!(
        result.outcome,
        TaskOutcome::InfrastructureError,
        "{result:?}"
    );
    assert!(codes(&result).contains(&anomaly::REPORT_MISSING));
}

#[tokio::test]
async fn storybook_installed_vitest_is_the_verified_one() {
    let project = Project::new();
    let suite = project.suite(RunnerKind::StorybookVitest, ".", Some("storybook"));
    let version = project
        .probe(&StorybookAdapter, &suite)
        .await
        .expect("the Vitest version is readable");
    assert!(testry_adapters::runners::vitest::VERIFIED_VERSIONS.contains(&version.as_str()));
    // The Storybook packages the stories ran with are the recorded ones.
    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(project.root().join("node_modules/storybook/package.json"))
            .unwrap(),
    )
    .unwrap();
    assert!(
        testry_adapters::runners::storybook::VERIFIED_STORYBOOK_VERSIONS
            .contains(&manifest["version"].as_str().unwrap()),
        "the image has Storybook {}",
        manifest["version"]
    );
}

// -------------------------------------------------------------- command --

/// P12: a command reports success or failure and nothing else.
#[tokio::test]
async fn p12_command_adapter_runs_the_fixed_argv() {
    let project = Project::new();
    let mut parts = suite_parts("gate", RunnerKind::Command, &[]);
    parts.command = vec![
        "sh".to_owned(),
        "-c".to_owned(),
        "echo checked > gate-ran; exit 0".to_owned(),
    ];
    let passing = SuiteDefinition::try_new(parts.clone()).unwrap();
    let result = project
        .run(&CommandAdapter, &passing, &Selection::All)
        .await;
    assert_eq!(result.outcome, TaskOutcome::Passed);
    assert_eq!(result.counts, None);
    assert_eq!(result.executed_files, FileAttribution::Unknown);
    assert!(project.root().join("gate-ran").is_file());

    parts.command = vec!["sh".to_owned(), "-c".to_owned(), "exit 3".to_owned()];
    let failing = SuiteDefinition::try_new(parts).unwrap();
    assert_eq!(
        project
            .run(&CommandAdapter, &failing, &Selection::All)
            .await
            .outcome,
        TaskOutcome::Failed
    );
}

// ----------------------------------------------------- resolver contract --

/// Spec section 7: the resolver must agree with the runner. For every import
/// in this fixture, the file each runner really loads must be among the
/// dependencies the analyzer records. Where runners disagree (extension
/// order), the graph holds every candidate.
#[tokio::test]
async fn the_dependency_graph_contains_what_each_runner_really_loads() {
    for (runner, profile) in [
        (RunnerKind::Bun, ResolverProfile::Bun),
        (RunnerKind::Vitest, ResolverProfile::Vite),
    ] {
        let project = Project::new();
        // Each module records which file it is.
        let module = |name: &str| format!("export const loaded = \"{name}\";\n");
        for name in [
            "src/both.ts",
            "src/both.js",
            "src/mapped.ts",
            "src/dir/index.ts",
            "src/dir/index.js",
            "src/only.tsx",
        ] {
            project.write(name, &module(name));
        }
        let framework = if runner == RunnerKind::Bun {
            "bun:test"
        } else {
            "vitest"
        };
        project.write(
            "src/resolve.test.ts",
            &format!(
                "import {{ test }} from \"{framework}\";\n\
                 import {{ writeFileSync }} from \"node:fs\";\n\
                 import {{ loaded as both }} from \"./both\";\n\
                 import {{ loaded as mapped }} from \"./mapped.js\";\n\
                 import {{ loaded as dir }} from \"./dir\";\n\
                 import {{ loaded as only }} from \"./only\";\n\
                 test(\"records what was loaded\", () => {{\n\
                   writeFileSync(\"loaded.json\", JSON.stringify([both, mapped, dir, only]));\n\
                 }});\n"
            ),
        );

        let suite = project.suite(runner, ".", None);
        let adapter: &dyn RunnerAdapter = if runner == RunnerKind::Bun {
            &BunAdapter
        } else {
            &VitestAdapter
        };
        let result = project
            .run(adapter, &suite, &files(&["src/resolve.test.ts"]))
            .await;
        assert_eq!(
            result.outcome,
            TaskOutcome::Passed,
            "{runner:?}: {result:?}"
        );
        let loaded: Vec<String> =
            serde_json::from_slice(&std::fs::read(project.root().join("loaded.json")).unwrap())
                .unwrap();
        std::fs::remove_file(project.root().join("loaded.json")).unwrap();

        // Analyze the same tree.
        let git = |arguments: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(project.root())
                .args(arguments)
                .status()
                .unwrap();
            assert!(status.success());
        };
        git(&["init", "--quiet"]);
        let snapshot = GitSourceRepository::new()
            .snapshot(
                &SnapshotRequest {
                    workspace_root: project.root().to_path_buf(),
                    mode: Mode::Full,
                    base_ref: None,
                    include_dirty: true,
                    scope: None,
                    exclude: PatternSet::try_new([
                        "**/node_modules/**",
                        "node_modules",
                        ".testry/**",
                    ])
                    .unwrap(),
                    declared_inputs: PatternSet::empty(),
                },
                &context(),
            )
            .await
            .unwrap();
        let analyzer = OxcDependencyAnalyzer::new(Arc::new(GitWorkspaceReader::new(
            project.root().canonicalize().unwrap(),
        )));
        let graph = analyzer
            .analyze(
                &snapshot,
                &AnalysisProfile {
                    side: SnapshotSide::Current,
                    resolver: profile,
                    limits: AnalysisLimits::default(),
                },
                &context(),
            )
            .await
            .unwrap();
        let dependencies: BTreeSet<&str> = graph
            .dependencies_of(&path("src/resolve.test.ts"))
            .map(RepoPath::as_str)
            .collect();

        for file in &loaded {
            assert!(
                dependencies.contains(file.as_str()),
                "{runner:?} loaded {file}, which is not a dependency in the graph: {dependencies:?}"
            );
        }
        assert_eq!(loaded.len(), 4);
        // No import of this fixture is uncertain.
        let unresolved: Vec<_> = graph
            .diagnostics()
            .iter()
            .filter(|diagnostic| diagnostic.file.as_str() == "src/resolve.test.ts")
            .collect();
        assert!(unresolved.is_empty(), "{runner:?}: {unresolved:?}");
    }
}
