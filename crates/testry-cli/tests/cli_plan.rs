//! End-to-end tests of the `testry` binary against a real Git repository
//! (milestone M1: `plan`, `explain`, `doctor`, exit codes).
//!
//! The fixture is a small Bun / Vite / Playwright style monorepo without
//! `node_modules`: planning must work from Git and file reads alone.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

const CONFIG: &str = r#"
schema_version = 1

[workspace]
exclude = ["**/node_modules/**", "fake-bin/**"]

[analysis]
global_inputs = [
  "**/package.json", "**/bun.lock", "**/tsconfig*.json",
  "**/vite.config.*", "**/vitest.config.*", "**/playwright.config.*", "**/bunfig.toml",
  "**/.env", "**/.env.*",
]

[execution]
default_environment = "local"
cpu_slots = 4
memory_budget_mib = 4096
max_parallel_suites = 2

[environments.local]
provider = "host"

[[suites]]
id = "api-unit"
runner = "bun"
command = ["bun", "test"]
include = ["apps/api/**/*.test.ts"]
inputs = ["apps/api/**", "packages/**"]
selection = "file_graph"
[suites.resources]
cpu_slots = 1
memory_mib = 512

[[suites]]
id = "web-unit"
runner = "vitest"
command = ["node", "./node_modules/vitest/vitest.mjs", "run"]
project = "unit"
include = ["apps/web/**/*.test.tsx"]
inputs = ["apps/web/**", "packages/**"]
selection = "file_graph"
workers = 1
[suites.resources]
cpu_slots = 1
memory_mib = 768

[[suites]]
id = "e2e"
runner = "playwright"
command = ["node", "./node_modules/@playwright/test/cli.js", "test"]
include = ["tests/e2e/**/*.spec.ts"]
inputs = ["apps/**", "packages/**", "tests/e2e/**"]
selection = "whole_suite"
workers = 1
[suites.resources]
cpu_slots = 1
memory_mib = 1536
exclusive = ["browser-budget"]
"#;

const TYPECHECK_SUITE: &str = r#"
[[suites]]
id = "typecheck"
runner = "command"
command = ["bun", "run", "typecheck"]
selection = "whole_suite"
always_run = true
[suites.resources]
cpu_slots = 1
memory_mib = 768
"#;

/// A marker that any executed project code or runner would create.
const SIDE_EFFECT: &str =
    "require('node:fs').writeFileSync(process.env.TESTRY_MARKER ?? 'EXECUTED', 'executed');\n";

struct Fixture {
    directory: tempfile::TempDir,
    /// The user state directory of this fixture, outside the workspace.
    state: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self::with_config(&format!("{CONFIG}{TYPECHECK_SUITE}"))
    }

    fn with_config(config: &str) -> Self {
        let fixture = Self {
            directory: tempfile::tempdir().unwrap(),
            state: tempfile::tempdir().unwrap(),
        };
        fixture.git(&["init", "--quiet", "--initial-branch=main"]);
        fixture.git(&["config", "user.name", "testry test"]);
        fixture.git(&["config", "user.email", "test@example.invalid"]);
        fixture.git(&["config", "commit.gpgsign", "false"]);

        let files: &[(&str, &str)] = &[
            ("testry.toml", config),
            (".gitignore", "node_modules/\n.env\nfake-bin/\n"),
            (
                "package.json",
                r#"{ "name": "fixture", "private": true, "workspaces": ["apps/*", "packages/*"] }"#,
            ),
            ("bun.lock", "lockfile v1\n"),
            (
                "tsconfig.json",
                r#"{ "compilerOptions": { "paths": { "@shared/*": ["./packages/shared/src/*"] } } }"#,
            ),
            (
                "apps/api/package.json",
                r#"{ "name": "api", "dependencies": { "hono": "4", "@fixture/shared": "workspace:*" } }"#,
            ),
            (
                "apps/api/src/user.ts",
                "import { Hono } from 'hono';\nimport { today } from '@fixture/shared';\nexport const user = () => ({ name: 'u', since: today(), app: Hono });\n",
            ),
            (
                "apps/api/src/user.test.ts",
                "import { expect, test } from 'bun:test';\nimport { user } from './user';\ntest('user', () => expect(user().name).toBe('u'));\n",
            ),
            ("apps/api/src/order.ts", "export const order = () => 1;\n"),
            (
                "apps/api/src/order.test.ts",
                "import { expect, test } from 'bun:test';\nimport { order } from './order';\ntest('order', () => expect(order()).toBe(1));\n",
            ),
            (
                "apps/web/package.json",
                r#"{ "name": "web", "dependencies": { "react": "19" }, "devDependencies": { "vitest": "3", "@playwright/test": "1" } }"#,
            ),
            (
                "apps/web/src/App.tsx",
                "import { today } from '@shared/date';\nexport const App = () => <p>{today()}</p>;\n",
            ),
            (
                "apps/web/src/App.test.tsx",
                "import { expect, test } from 'vitest';\nimport { App } from './App';\ntest('app', () => expect(App).toBeDefined());\n",
            ),
            (
                "packages/shared/package.json",
                r#"{ "name": "@fixture/shared", "exports": { ".": "./src/index.ts" } }"#,
            ),
            ("packages/shared/src/index.ts", "export * from './date';\n"),
            (
                "packages/shared/src/date.ts",
                "export const today = () => '2026-11-17';\n",
            ),
            (
                "tests/e2e/login.spec.ts",
                "import { test } from '@playwright/test';\nimport { url } from './helpers';\ntest('login', async ({ page }) => { await page.goto(url); });\n",
            ),
            (
                "tests/e2e/helpers.ts",
                "export const url = 'http://localhost:3000';\n",
            ),
        ];
        for (path, content) in files {
            fixture.write(path, content);
        }
        // Configuration files that would leave a marker if anything loaded them.
        for path in [
            "vitest.config.ts",
            "playwright.config.ts",
            "apps/web/vite.config.ts",
        ] {
            fixture.write(path, SIDE_EFFECT);
        }
        fixture.write(
            "bunfig.toml",
            "[test]\npreload = [\"./setup-that-must-not-run.ts\"]\n",
        );
        fixture.write("setup-that-must-not-run.ts", SIDE_EFFECT);
        fixture.git(&["add", "--all"]);
        fixture.git(&["commit", "--quiet", "--message", "fixture"]);
        fixture.fake_runtimes();
        fixture
    }

    fn root(&self) -> &Path {
        self.directory.path()
    }

    fn git(&self, arguments: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(self.root())
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn write(&self, path: &str, content: &str) {
        let full = self.root().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, content).unwrap();
    }

    fn append(&self, path: &str, content: &str) {
        let full = self.root().join(path);
        let mut existing = std::fs::read_to_string(&full).unwrap();
        existing.push_str(content);
        std::fs::write(full, existing).unwrap();
    }

    /// Directory with `bun`, `node`, `npm`, `npx` and `bunx` stand-ins that
    /// leave a marker when they are started.
    fn fake_runtimes(&self) -> PathBuf {
        let bin = self.root().join("fake-bin");
        std::fs::create_dir_all(&bin).unwrap();
        for name in [
            "bun",
            "bunx",
            "node",
            "npm",
            "npx",
            "pnpm",
            "yarn",
            "docker",
            "devcontainer",
        ] {
            let script = bin.join(name);
            std::fs::write(
                &script,
                format!("#!/bin/sh\ntouch '{}'\n", self.marker().display()),
            )
            .unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        bin
    }

    fn marker(&self) -> PathBuf {
        self.root().join("fake-bin").join("STARTED")
    }

    fn testry(&self, arguments: &[&str]) -> Output {
        let path = format!(
            "{}:{}",
            self.fake_runtimes().display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Command::new(env!("CARGO_BIN_EXE_testry"))
            .current_dir(self.root())
            .args(arguments)
            .env("PATH", path)
            .env("TESTRY_MARKER", self.marker())
            .env("TESTRY_STATE_HOME", self.state.path())
            .env_remove("TESTRY_TRUSTED_CONFIG_DIGEST")
            .env_remove("CI")
            .output()
            .expect("the testry binary must be runnable")
    }

    /// Runs `plan --format json` and returns the single JSON object of stdout.
    fn plan(&self, arguments: &[&str]) -> Value {
        let mut all = vec!["plan", "--format", "json"];
        all.extend_from_slice(arguments);
        let output = self.testry(&all);
        assert_eq!(code(&output), 0, "stderr: {}", stderr(&output));
        json(&output)
    }
}

fn code(output: &Output) -> i32 {
    output
        .status
        .code()
        .expect("the process must exit normally")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

/// stdout must be exactly one JSON value.
fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is not one JSON object ({error}): {}",
            stdout(output)
        )
    })
}

fn suite<'a>(plan: &'a Value, id: &str) -> &'a Value {
    plan["suites"]
        .as_array()
        .unwrap()
        .iter()
        .find(|suite| suite["suite_id"] == id)
        .unwrap_or_else(|| panic!("suite {id} is not in the plan"))
}

fn selection(plan: &Value, id: &str) -> Value {
    suite(plan, id)["effective_selection"].clone()
}

fn files(names: &[&str]) -> Value {
    serde_json::json!({ "kind": "files", "files": names })
}

fn all() -> Value {
    serde_json::json!({ "kind": "all" })
}

fn omit(reason: &str) -> Value {
    serde_json::json!({ "kind": "omit", "reason": reason })
}

fn stored_plan(fixture: &Fixture, plan: &Value) -> PathBuf {
    let id = plan["plan_id"].as_str().unwrap().replace(':', "-");
    fixture
        .root()
        .join(".testry/plans")
        .join(format!("{id}.json"))
}

/// G01: a test that imports A is selected when A changes, and the path is explained.
#[test]
fn g01_changed_module_selects_its_test_and_explains_the_path() {
    let fixture = Fixture::new();
    fixture.append("apps/api/src/order.ts", "export const extra = 2;\n");

    let plan = fixture.plan(&[]);
    assert_eq!(plan["schema_version"], 1);
    assert_eq!(plan["mode"], "affected");
    assert_eq!(plan["assurance"], "advisory");
    assert_eq!(
        plan["changed_paths"],
        serde_json::json!(["apps/api/src/order.ts"])
    );
    assert_eq!(plan["baseline_commit"], fixture.git(&["rev-parse", "HEAD"]));
    assert_eq!(plan["fallbacks"], serde_json::json!([]));

    assert_eq!(
        selection(&plan, "api-unit"),
        files(&["apps/api/src/order.test.ts"])
    );
    assert_eq!(selection(&plan, "web-unit"), omit("no_affected_tests"));
    assert_eq!(selection(&plan, "e2e"), all());
    assert_eq!(selection(&plan, "typecheck"), all());

    let api = suite(&plan, "api-unit");
    assert_eq!(api["capability_verification"], "pending");
    assert_eq!(
        api["reasons"],
        serde_json::json!([{
            "code": "transitive_dependency",
            "dependency_path": ["apps/api/src/order.test.ts", "apps/api/src/order.ts"],
        }])
    );
    assert_eq!(
        suite(&plan, "e2e")["reasons"][0]["code"],
        "whole_suite_policy"
    );
    assert_eq!(
        suite(&plan, "typecheck")["reasons"][0]["code"],
        "always_run"
    );
}

/// G02 / G03: a change deep in a workspace package reaches tests through a
/// package export, a barrel and a tsconfig path alias.
#[test]
fn g02_g03_transitive_change_through_package_barrel_and_alias() {
    let fixture = Fixture::new();
    fixture.append(
        "packages/shared/src/date.ts",
        "export const tomorrow = () => 'later';\n",
    );

    let plan = fixture.plan(&[]);
    assert_eq!(
        plan["fallbacks"],
        serde_json::json!([]),
        "diagnostics: {}",
        plan["diagnostics"]
    );
    assert_eq!(
        selection(&plan, "api-unit"),
        files(&["apps/api/src/user.test.ts"])
    );
    assert_eq!(
        selection(&plan, "web-unit"),
        files(&["apps/web/src/App.test.tsx"])
    );
    assert_eq!(
        suite(&plan, "api-unit")["reasons"][0]["dependency_path"],
        serde_json::json!([
            "apps/api/src/user.test.ts",
            "apps/api/src/user.ts",
            "packages/shared/src/index.ts",
            "packages/shared/src/date.ts",
        ])
    );
    assert_eq!(
        suite(&plan, "web-unit")["reasons"][0]["dependency_path"],
        serde_json::json!([
            "apps/web/src/App.test.tsx",
            "apps/web/src/App.tsx",
            "packages/shared/src/date.ts"
        ])
    );
}

/// G06 / G07: uncertainty widens every suite and is listed in the plan.
#[test]
fn g06_g07_uncertainty_widens_every_suite() {
    let fixture = Fixture::new();
    fixture.write(
        "apps/web/src/lazy.ts",
        "export const load = (name: string) => import(`./pages/${name}`);\n",
    );
    fixture.write("apps/web/src/broken.ts", "import { from './App'\n");
    fixture.append("apps/api/src/order.ts", "import '@missing/alias';\n");

    let plan = fixture.plan(&[]);
    for id in ["api-unit", "web-unit", "e2e", "typecheck"] {
        assert_eq!(selection(&plan, id), all(), "{id}");
    }
    let fallbacks = plan["fallbacks"].as_array().unwrap();
    assert!(
        fallbacks
            .iter()
            .any(|fallback| fallback["code"] == "unknown_dependency")
    );
    let codes: Vec<&str> = plan["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|diagnostic| diagnostic["code"].as_str().unwrap())
        .collect();
    for expected in ["dynamic_dependency", "parse_failure", "unresolved_import"] {
        assert!(codes.contains(&expected), "{expected} missing in {codes:?}");
    }
    assert!(
        suite(&plan, "api-unit")["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reason| reason["code"] == "expanded_by_fallback"
                && reason["fallback"] == "unknown_dependency")
    );
}

/// G08 / G09: deleting or renaming a dependency keeps its consumers selected.
#[test]
fn g08_g09_deleted_and_renamed_dependencies_keep_their_consumers() {
    let fixture = Fixture::new();
    fixture.git(&["mv", "apps/api/src/order.ts", "apps/api/src/purchase.ts"]);
    fixture.write(
        "apps/api/src/order.test.ts",
        "import { expect, test } from 'bun:test';\nimport { order } from './purchase';\ntest('order', () => expect(order()).toBe(1));\n",
    );

    let plan = fixture.plan(&[]);
    assert_eq!(
        plan["changed_paths"],
        serde_json::json!([
            "apps/api/src/order.test.ts",
            "apps/api/src/order.ts",
            "apps/api/src/purchase.ts"
        ])
    );
    assert_eq!(
        plan["fallbacks"],
        serde_json::json!([]),
        "diagnostics: {}",
        plan["diagnostics"]
    );
    assert_eq!(
        selection(&plan, "api-unit"),
        files(&["apps/api/src/order.test.ts"])
    );

    // Renaming without updating the importer leaves an unresolved import,
    // which widens instead of dropping the test.
    let stale = Fixture::new();
    stale.git(&["mv", "apps/api/src/order.ts", "apps/api/src/purchase.ts"]);
    let plan = stale.plan(&[]);
    assert_eq!(selection(&plan, "api-unit"), all());
    assert!(
        plan["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| {
                diagnostic["code"] == "unresolved_import"
                    && diagnostic["path"] == "apps/api/src/order.test.ts"
            })
    );
}

/// G10: a new untracked test file is part of the inventory and is selected.
#[test]
fn g10_new_untracked_test_is_selected() {
    let fixture = Fixture::new();
    fixture.write(
        "apps/api/src/new feature.test.ts",
        "import { test } from 'bun:test';\nimport './order';\ntest('new', () => {});\n",
    );
    let plan = fixture.plan(&[]);
    assert_eq!(
        selection(&plan, "api-unit"),
        files(&["apps/api/src/new feature.test.ts"])
    );
    assert_eq!(
        suite(&plan, "api-unit")["reasons"],
        serde_json::json!([{ "code": "direct_change", "path": "apps/api/src/new feature.test.ts" }])
    );
}

/// G12: a lockfile or shared configuration change widens every suite.
#[test]
fn g12_global_inputs_widen_every_suite() {
    for changed in [
        "bun.lock",
        "tsconfig.json",
        "vitest.config.ts",
        "testry.toml",
        "apps/api/package.json",
    ] {
        let fixture = Fixture::new();
        fixture.append(changed, "\n");
        let plan = fixture.plan(&[]);
        for id in ["api-unit", "web-unit", "e2e", "typecheck"] {
            assert_eq!(selection(&plan, id), all(), "{changed} / {id}");
        }
        assert_eq!(
            plan["fallbacks"][0]["code"], "global_input_changed",
            "{changed}"
        );
        assert_eq!(plan["fallbacks"][0]["path"], changed);
    }
}

/// P02: a changed file that no test depends on is not assumed to be irrelevant.
#[test]
fn p02_unrelated_change_widens_every_suite() {
    let fixture = Fixture::new();
    fixture.write("README.md", "# docs\n");
    let plan = fixture.plan(&[]);
    assert_eq!(selection(&plan, "api-unit"), all());
    assert_eq!(
        plan["fallbacks"],
        serde_json::json!([{ "code": "no_related_tests", "path": "README.md" }])
    );
}

/// P03: no change means no work. `plan` still succeeds; it only shows the plan.
#[test]
fn p03_no_changes_is_no_work() {
    let fixture = Fixture::with_config(CONFIG);
    let plan = fixture.plan(&[]);
    assert_eq!(plan["changed_paths"], serde_json::json!([]));
    for id in ["api-unit", "web-unit", "e2e"] {
        assert_eq!(selection(&plan, id), omit("no_changes"), "{id}");
    }

    let text = fixture.testry(&["plan"]);
    assert_eq!(code(&text), 0);
    assert!(
        stdout(&text).contains("not a successful verification"),
        "{}",
        stdout(&text)
    );
}

#[test]
fn full_mode_needs_no_baseline_and_works_without_commits() {
    let fixture = Fixture::new();
    let plan = fixture.plan(&["--mode", "full"]);
    assert_eq!(plan["mode"], "full");
    assert_eq!(plan["assurance"], "full_suite");
    assert_eq!(plan["baseline_commit"], Value::Null);
    assert_eq!(plan["changed_paths"], Value::Null);
    assert_eq!(
        plan["scope"],
        serde_json::json!(["api-unit", "e2e", "typecheck", "web-unit"])
    );
    for id in ["api-unit", "web-unit", "e2e", "typecheck"] {
        assert_eq!(selection(&plan, id), all());
        assert_eq!(
            suite(&plan, id)["reasons"],
            serde_json::json!([{ "code": "full_verification" }])
        );
    }

    // A repository without any commit can still be verified in full.
    let empty = tempfile::tempdir().unwrap();
    let git = |arguments: &[&str]| {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(empty.path())
                .args(arguments)
                .status()
                .unwrap()
                .success()
        );
    };
    git(&["init", "--quiet"]);
    std::fs::write(empty.path().join("testry.toml"), CONFIG).unwrap();
    for file in [
        "apps/api/a.test.ts",
        "apps/web/a.test.tsx",
        "tests/e2e/a.spec.ts",
    ] {
        std::fs::create_dir_all(empty.path().join(file).parent().unwrap()).unwrap();
        std::fs::write(empty.path().join(file), "export {};\n").unwrap();
    }
    let run = |arguments: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_testry"))
            .current_dir(empty.path())
            .args(arguments)
            .env_remove("CI")
            .output()
            .unwrap()
    };
    assert_eq!(
        code(&run(&["plan", "--mode", "full", "--format", "json"])),
        0
    );
    let affected = run(&["plan", "--mode", "affected", "--format", "json"]);
    assert_eq!(
        code(&affected),
        2,
        "no commit means no baseline; that is an error, not an empty diff"
    );
    assert_eq!(json(&affected)["error"]["code"], "source_unavailable");
}

/// P06 (planning part): shadow executes everything and records the candidate.
#[test]
fn p06_shadow_plans_everything_and_records_the_candidate() {
    let fixture = Fixture::new();
    fixture.append("apps/api/src/order.ts", "export const extra = 2;\n");
    let plan = fixture.plan(&["--mode", "shadow"]);
    assert_eq!(plan["assurance"], "full_suite");
    for id in ["api-unit", "web-unit", "e2e", "typecheck"] {
        assert_eq!(selection(&plan, id), all());
    }
    assert_eq!(
        suite(&plan, "api-unit")["candidate"]["selection"],
        files(&["apps/api/src/order.test.ts"])
    );
    assert_eq!(
        suite(&plan, "web-unit")["candidate"]["selection"],
        omit("no_affected_tests")
    );
}

#[test]
fn a_local_suite_scope_is_advisory() {
    let fixture = Fixture::new();
    let plan = fixture.plan(&["--mode", "full", "--suite", "api-unit"]);
    assert_eq!(plan["scope"], serde_json::json!(["api-unit"]));
    assert_eq!(plan["scope_limited"], true);
    assert_eq!(plan["assurance"], "advisory");

    let unknown = fixture.testry(&["plan", "--suite", "nope", "--format", "json"]);
    assert_eq!(code(&unknown), 2);
    assert_eq!(json(&unknown)["error"]["code"], "planning");
}

#[test]
fn an_explicit_base_covers_the_whole_branch() {
    let fixture = Fixture::new();
    let fork = fixture.git(&["rev-parse", "HEAD"]);
    fixture.git(&["checkout", "--quiet", "-b", "feature"]);
    fixture.append("apps/api/src/order.ts", "export const committed = 1;\n");
    fixture.git(&["commit", "--quiet", "--all", "--message", "feature"]);

    // The default base is HEAD: committed work is not part of the diff.
    let uncommitted_only = fixture.plan(&[]);
    assert_eq!(uncommitted_only["changed_paths"], serde_json::json!([]));

    let branch = fixture.plan(&["--base", "main"]);
    assert_eq!(branch["baseline_commit"], fork);
    assert_eq!(
        branch["changed_paths"],
        serde_json::json!(["apps/api/src/order.ts"])
    );
    assert_eq!(
        selection(&branch, "api-unit"),
        files(&["apps/api/src/order.test.ts"])
    );

    let missing = fixture.testry(&["plan", "--base", "origin/main", "--format", "json"]);
    assert_eq!(code(&missing), 2);
    let error = json(&missing);
    assert_eq!(error["schema_version"], 1);
    assert_eq!(error["error"]["code"], "source_unavailable");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("never fetches")
    );
}

/// G18: the same inputs give a byte-identical plan.
#[test]
fn g18_plans_are_deterministic() {
    let fixture = Fixture::new();
    fixture.append("packages/shared/src/date.ts", "export const x = 1;\n");
    fixture.write("apps/api/src/z.test.ts", "import './order';\n");
    let first = fixture.testry(&["plan", "--format", "json"]);
    let second = fixture.testry(&["plan", "--format", "json"]);
    assert_eq!(code(&first), 0);
    assert_eq!(stdout(&first), stdout(&second));
    assert!(stdout(&first).ends_with("}\n"));
}

/// Spec section 14: `plan` uses Git and file reads only.
#[test]
fn plan_executes_no_project_code_and_starts_no_runtime() {
    let fixture = Fixture::new();
    fixture.append("apps/api/src/order.ts", "export const extra = 2;\n");
    for arguments in [
        &["plan"][..],
        &["plan", "--mode", "full"],
        &["plan", "--mode", "shadow"],
        &["doctor"],
    ] {
        let output = fixture.testry(arguments);
        assert_eq!(code(&output), 0, "{arguments:?}: {}", stderr(&output));
    }
    assert!(
        !fixture.marker().exists(),
        "a runtime or project code was started during planning"
    );
    assert!(!fixture.root().join("EXECUTED").exists());
    assert!(
        !fixture.root().join("node_modules").exists(),
        "nothing may be installed"
    );
}

/// Spec section 6: planning leaves the work tree, the index and refs alone.
#[test]
fn plan_leaves_the_repository_untouched_apart_from_its_ignored_state() {
    let fixture = Fixture::new();
    fixture.append("apps/api/src/order.ts", "export const extra = 2;\n");
    fixture.git(&["add", "apps/api/src/order.ts"]);
    fixture.append("apps/api/src/user.ts", "export const more = 1;\n");
    let state = |fixture: &Fixture| {
        (
            fixture.git(&["status", "--porcelain=v1", "--untracked-files=all"]),
            fixture.git(&["rev-parse", "HEAD"]),
            fixture.git(&["stash", "list"]),
        )
    };
    let index = |fixture: &Fixture| std::fs::read(fixture.root().join(".git/index")).unwrap();
    let before = state(&fixture);
    let index_before = index(&fixture);
    let plan = fixture.plan(&[]);
    assert_eq!(
        state(&fixture),
        before,
        "only ignored state below .testry may appear"
    );
    assert!(
        index(&fixture) == index_before,
        "the index file was rewritten"
    );

    let stored = stored_plan(&fixture, &plan);
    assert!(stored.is_file());
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&stored), 0o600);
    assert_eq!(mode(&fixture.root().join(".testry")), 0o700);
    // The stored document is the plan that was printed.
    let stored: Value = serde_json::from_slice(&std::fs::read(&stored).unwrap()).unwrap();
    assert_eq!(stored, plan);
}

#[test]
fn explain_shows_the_dependency_path_of_a_stored_plan() {
    let fixture = Fixture::new();
    fixture.append("packages/shared/src/date.ts", "export const x = 1;\n");
    let plan = fixture.plan(&[]);
    let stored = stored_plan(&fixture, &plan);
    let stored = stored.to_str().unwrap();

    let text = fixture.testry(&["explain", "--plan", stored, "--suite", "api-unit"]);
    assert_eq!(code(&text), 0, "{}", stderr(&text));
    let shown = stdout(&text);
    assert!(shown.contains("Suite api-unit (bun)"), "{shown}");
    assert!(
        shown.contains(
            "apps/api/src/user.test.ts -> apps/api/src/user.ts -> packages/shared/src/index.ts -> packages/shared/src/date.ts"
        ),
        "{shown}"
    );

    let object = fixture.testry(&[
        "explain", "--plan", stored, "--suite", "web-unit", "--format", "json",
    ]);
    assert_eq!(code(&object), 0);
    let explanation = json(&object);
    assert_eq!(explanation["schema_version"], 1);
    assert_eq!(explanation["plan_id"], plan["plan_id"]);
    assert_eq!(explanation["suite"]["suite_id"], "web-unit");
    assert_eq!(
        explanation["suite"]["effective_selection"],
        files(&["apps/web/src/App.test.tsx"])
    );

    let unknown = fixture.testry(&[
        "explain", "--plan", stored, "--suite", "nope", "--format", "json",
    ]);
    assert_eq!(code(&unknown), 2);
    assert_eq!(json(&unknown)["error"]["code"], "planning");
}

#[test]
fn explain_rejects_modified_and_foreign_plan_documents() {
    let fixture = Fixture::new();
    fixture.append("apps/api/src/order.ts", "export const extra = 2;\n");
    let plan = fixture.plan(&[]);
    let text = std::fs::read_to_string(stored_plan(&fixture, &plan)).unwrap();

    let cases = [
        (
            "tampered.json",
            text.replace("apps/api/src/order.test.ts", "apps/api/src/user.test.ts"),
        ),
        (
            "version.json",
            text.replace("\"schema_version\": 1", "\"schema_version\": 2"),
        ),
        (
            "garbage.json",
            "{ \"argv\": [\"rm\", \"-rf\", \"/\"] }".to_owned(),
        ),
    ];
    for (name, content) in cases {
        let path = fixture.root().join(name);
        std::fs::write(&path, content).unwrap();
        let output = fixture.testry(&[
            "explain",
            "--plan",
            path.to_str().unwrap(),
            "--suite",
            "api-unit",
        ]);
        assert_eq!(code(&output), 2, "{name}");
        assert!(
            stdout(&output).is_empty(),
            "{name}: nothing may be explained from a rejected plan"
        );
    }
    let missing = fixture.testry(&["explain", "--plan", "missing.json", "--suite", "api-unit"]);
    assert_eq!(code(&missing), 2);
}

/// P07: affected alone or a limited scope is not a CI quality gate.
#[test]
fn p07_ci_rejects_affected_and_scope_limits() {
    let fixture = Fixture::new();
    let cases: &[&[&str]] = &[
        &["verify", "--ci", "--mode", "affected"],
        &["test", "--ci", "--mode", "affected"],
        &["verify", "--ci", "--suite", "api-unit"],
        &["verify", "--ci", "--mode", "shadow"],
        &["verify", "--mode", "affected"],
    ];
    for arguments in cases {
        let mut with_format = arguments.to_vec();
        with_format.extend_from_slice(&["--format", "json"]);
        let output = fixture.testry(&with_format);
        assert_eq!(code(&output), 2, "{arguments:?}");
        assert_eq!(json(&output)["error"]["code"], "planning", "{arguments:?}");
    }

    // Without an explicit mode, CI uses policy.ci_default_mode (full), which
    // passes the gate rules. It then stops because nothing was approved.
    let default_in_ci = fixture.testry(&["test", "--ci", "--format", "json"]);
    assert_eq!(code(&default_in_ci), 2);
    assert_eq!(json(&default_in_ci)["error"]["code"], "trust_required");

    // The CI environment variable has the same effect as --ci.
    let from_environment = Command::new(env!("CARGO_BIN_EXE_testry"))
        .current_dir(fixture.root())
        .args(["test", "--mode", "affected", "--format", "json"])
        .env("CI", "true")
        .output()
        .unwrap();
    assert_eq!(code(&from_environment), 2);
    assert_eq!(json(&from_environment)["error"]["code"], "planning");
}

/// A command never reports success for something it did not verify.
#[test]
fn execution_is_refused_without_trust_and_nothing_is_started() {
    let fixture = Fixture::new();
    // Every suite has an adapter, but nothing was approved.
    let full = fixture.testry(&["verify", "--format", "json"]);
    assert_eq!(code(&full), 2);
    assert_eq!(json(&full)["error"]["code"], "trust_required");

    // A run that would execute commands needs an approval first.
    fixture.append("apps/api/src/order.ts", "export const extra = 2;\n");
    let untrusted = fixture.testry(&["test", "--suite", "api-unit", "--format", "json"]);
    assert_eq!(code(&untrusted), 2);
    assert_eq!(json(&untrusted)["error"]["code"], "trust_required");

    // `gc` needs exactly one of its two modes; without one nothing happens.
    let ambiguous = fixture.testry(&["gc"]);
    assert_eq!(code(&ambiguous), 2);
    let both = fixture.testry(&["gc", "--dry-run", "--apply"]);
    assert_eq!(code(&both), 2);
    let unknown_run = fixture.testry(&["runs", "show", "123e4567-e89b-42d3-a456-426614174000"]);
    assert_eq!(code(&unknown_run), 2);

    let environment = fixture.testry(&["verify", "--environment", "nope"]);
    assert_eq!(code(&environment), 2);
    assert!(stderr(&environment).contains("not defined under [environments]"));

    // Nothing was started, and a refused request leaves no run behind.
    assert!(!fixture.marker().exists());
    let runs = fixture.root().join(".testry/runs");
    assert!(!runs.exists() || std::fs::read_dir(&runs).unwrap().next().is_none());
}

#[test]
fn configuration_problems_exit_with_two() {
    let fixture = Fixture::new();
    fixture.write(
        "testry.toml",
        &CONFIG.replace("cpu_slots = 4", "cpu_slots = 0"),
    );
    let invalid = fixture.testry(&["plan", "--format", "json"]);
    assert_eq!(code(&invalid), 2);
    assert_eq!(json(&invalid)["error"]["code"], "configuration");

    fixture.write(
        "testry.toml",
        &format!("{CONFIG}\n[unknown_section]\nkey = 1\n"),
    );
    assert_eq!(code(&fixture.testry(&["plan"])), 2);

    std::fs::remove_file(fixture.root().join("testry.toml")).unwrap();
    let missing = fixture.testry(&["plan"]);
    assert_eq!(code(&missing), 2);
    assert!(stderr(&missing).contains("cannot read configuration"));

    let usage = fixture.testry(&["plan", "--mode", "safe"]);
    assert_eq!(code(&usage), 2);
}

#[test]
fn a_suite_without_test_files_is_reported_instead_of_planned() {
    let fixture = Fixture::new();
    fixture.git(&["rm", "--quiet", "-r", "tests/e2e"]);
    let output = fixture.testry(&["plan", "--mode", "full", "--format", "json"]);
    assert_eq!(code(&output), 2);
    let error = json(&output);
    assert_eq!(error["error"]["code"], "planning");
    assert!(error["error"]["message"].as_str().unwrap().contains("e2e"));
}

#[test]
fn doctor_reports_checks_without_claiming_unverified_ones() {
    let fixture = Fixture::new();
    let output = fixture.testry(&["doctor", "--format", "json"]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let report = json(&output);
    assert_eq!(report["schema_version"], 1);
    let status = |id: &str| {
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["id"] == id)
            .unwrap_or_else(|| panic!("check {id} missing"))["status"]
            .clone()
    };
    assert_eq!(status("git"), "ok");
    assert_eq!(status("configuration"), "ok");
    assert_eq!(status("workspace"), "ok");
    assert_eq!(status("runners"), "ok");
    assert_eq!(status("environments"), "ok");
    // Doctor starts no suite command, so installed versions stay unchecked.
    assert_eq!(status("runner_versions"), "not_checked");
    assert_eq!(status("trust"), "warning");
    assert!(status("container") == "ok" || status("container") == "warning");
    assert_eq!(report["status"], "warning");

    let approved = fixture.testry(&["trust", "--approve"]);
    assert_eq!(code(&approved), 0, "{}", stderr(&approved));
    let trusted = json(&fixture.testry(&["doctor", "--format", "json"]));
    let trust = trusted["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["id"] == "trust")
        .unwrap();
    assert_eq!(trust["status"], "ok");

    std::fs::remove_file(fixture.root().join("testry.toml")).unwrap();
    let broken = fixture.testry(&["doctor", "--format", "json"]);
    assert_eq!(code(&broken), 2);
    assert_eq!(json(&broken)["status"], "error");
}

#[test]
fn text_output_states_the_assurance_and_the_storage_path() {
    let fixture = Fixture::new();
    fixture.append("apps/api/src/order.ts", "export const extra = 2;\n");
    let output = fixture.testry(&["plan"]);
    assert_eq!(code(&output), 0);
    let shown = stdout(&output);
    assert!(shown.contains("assurance: advisory"), "{shown}");
    assert!(shown.contains("Suite api-unit (bun)"), "{shown}");
    assert!(shown.contains("runs: 1 test file(s)"), "{shown}");
    assert!(shown.contains("Stored at .testry/plans/blake3-"), "{shown}");
}
