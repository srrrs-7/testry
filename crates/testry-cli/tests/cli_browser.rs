//! End-to-end tests of the browser based suites (milestone M3): Storybook
//! stories through the Storybook Vitest addon, and Playwright, next to Bun
//! and Vitest unit suites in one repository. The real binary, a real Git
//! repository, the real runners and a real browser.
//!
//! These tests need the dev container (Bun, Node, Chromium and the fixture
//! dependencies); outside of it they fail. They need no network.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};

const CONFIG: &str = r#"
schema_version = 1

[workspace]
exclude = ["**/node_modules/**", "node_modules"]

[analysis]
global_inputs = [
  "**/package.json", "**/bun.lock",
  "**/vitest.config.*", "**/playwright.config.*", ".storybook/**",
]

[execution]
default_environment = "local"
cpu_slots = 4
memory_budget_mib = 8192
max_parallel_suites = 3
termination_grace_seconds = 2

[environments.local]
provider = "host"

[[suites]]
id = "api-unit"
runner = "bun"
cwd = "apps/api"
command = ["bun", "test"]
include = ["apps/api/**/*.test.ts"]
inputs = ["apps/api/**"]
selection = "file_graph"
timeout_seconds = 120
[suites.resources]
cpu_slots = 1
memory_mib = 512

[[suites]]
id = "web-unit"
runner = "vitest"
command = ["node", "./node_modules/vitest/vitest.mjs", "run"]
project = "unit"
include = ["apps/web/**/*.test.ts"]
inputs = ["apps/web/**"]
selection = "file_graph"
workers = 1
timeout_seconds = 120
[suites.resources]
cpu_slots = 1
memory_mib = 768

[[suites]]
id = "stories"
runner = "storybook_vitest"
command = ["node", "./node_modules/vitest/vitest.mjs", "run"]
project = "storybook"
include = ["apps/web/**/*.stories.ts"]
inputs = ["apps/web/**", ".storybook/**"]
selection = "whole_suite"
workers = 1
timeout_seconds = 180
[suites.resources]
cpu_slots = 1
memory_mib = 1024
exclusive = ["browser-budget"]
"#;

const E2E_SUITE: &str = r#"
[[suites]]
id = "e2e"
runner = "playwright"
command = ["node", "./node_modules/@playwright/test/cli.js", "test"]
include = ["tests/e2e/**/*.spec.ts"]
inputs = ["apps/**", "tests/e2e/**"]
selection = "whole_suite"
workers = 1
timeout_seconds = 180
[suites.resources]
cpu_slots = 1
memory_mib = 1536
exclusive = ["browser-budget"]
"#;

const VITEST_CONFIG: &str = r#"import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { defineConfig } from "vitest/config";
import { storybookTest } from "@storybook/addon-vitest/vitest-plugin";
import { playwright } from "@vitest/browser-playwright";
const dirname = path.dirname(fileURLToPath(import.meta.url));
export default defineConfig({
  cacheDir: path.join(dirname, ".cache/vite"),
  // The fixture links node_modules to a directory outside the project.
  server: { fs: { allow: [dirname, fs.realpathSync(path.join(dirname, "node_modules"))] } },
  test: {
    projects: [
      {
        extends: true,
        plugins: [storybookTest({ configDir: path.join(dirname, ".storybook") })],
        test: {
          name: "storybook",
          browser: { enabled: true, headless: true, provider: playwright({}), instances: [{ browser: "chromium" }] },
        },
      },
      { extends: true, test: { name: "unit", include: ["apps/web/src/**/*.test.ts"] } },
    ],
  },
});
"#;

const PLAYWRIGHT_CONFIG: &str = r#"import { defineConfig } from "@playwright/test";
export default defineConfig({
  testDir: "./tests/e2e",
  projects: [
    { name: "setup", testMatch: /global\.setup\.ts/ },
    { name: "chromium", testMatch: /.*\.spec\.ts/, dependencies: ["setup"], use: { browserName: "chromium" } },
  ],
});
"#;

const BUTTON_STORIES: &str = r#"import type { Meta, StoryObj } from "@storybook/html-vite";
import { expect, within } from "storybook/test";
import { label } from "./label";
const meta: Meta = {
  title: "Button",
  render: () => { const button = document.createElement("button"); button.textContent = label(); return button; },
};
export default meta;
export const Primary: StoryObj = {};
export const Labelled: StoryObj = {
  play: async ({ canvasElement }) => { await expect(within(canvasElement).getByRole("button")).toHaveTextContent("Save"); },
};
"#;

struct Fixture {
    directory: tempfile::TempDir,
    state: tempfile::TempDir,
}

impl Fixture {
    /// Unit suites, stories and E2E, committed and trusted.
    fn new() -> Self {
        Self::with_config(&format!("{CONFIG}{E2E_SUITE}"))
    }

    fn with_config(config: &str) -> Self {
        let modules = std::env::var("TESTRY_RUNNER_FIXTURE_MODULES").expect(
            "TESTRY_RUNNER_FIXTURE_MODULES is not set: these tests must run in the dev container (make test)",
        );
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
            (".gitignore", "node_modules\n.cache\n"),
            (
                "package.json",
                r#"{ "name": "fixture", "private": true, "type": "module", "devDependencies": {
                    "@playwright/test": "1.65.1", "@storybook/addon-vitest": "10.6.7", "@storybook/html-vite": "10.6.7",
                    "@vitest/browser-playwright": "5.1.0", "storybook": "10.6.7", "vitest": "5.1.0" } }"#,
            ),
            ("vitest.config.ts", VITEST_CONFIG),
            ("playwright.config.ts", PLAYWRIGHT_CONFIG),
            (
                ".storybook/main.ts",
                "export default {\n  stories: [\"../apps/web/src/**/*.stories.ts\"],\n  addons: [\"@storybook/addon-vitest\"],\n  framework: { name: \"@storybook/html-vite\", options: {} },\n  core: { disableTelemetry: true },\n};\n",
            ),
            (".storybook/preview.ts", "export default {};\n"),
            (
                "apps/api/src/order.ts",
                "export const order = (): number => 1;\n",
            ),
            (
                "apps/api/src/order.test.ts",
                "import { expect, test } from \"bun:test\";\nimport { order } from \"./order\";\ntest(\"order\", () => { expect(order()).toBe(1); });\n",
            ),
            (
                "apps/web/src/label.ts",
                "export const label = (): string => \"Save\";\n",
            ),
            (
                "apps/web/src/label.test.ts",
                "import { expect, test } from \"vitest\";\nimport { label } from \"./label\";\ntest(\"label\", () => { expect(label()).toBe(\"Save\"); });\n",
            ),
            ("apps/web/src/Button.stories.ts", BUTTON_STORIES),
            (
                "tests/e2e/global.setup.ts",
                "import { test as setup } from \"@playwright/test\";\nsetup(\"prepare\", async () => {});\n",
            ),
            (
                "tests/e2e/home.spec.ts",
                "import { expect, test } from \"@playwright/test\";\n\
                 test(\"shows the heading\", async ({ page }) => {\n  await page.setContent(\"<h1>home</h1>\");\n  await expect(page.locator(\"h1\")).toHaveText(\"home\");\n});\n\
                 test(\"has a title\", async ({ page }) => {\n  await page.setContent(\"<title>fixture</title>\");\n  await expect(page).toHaveTitle(\"fixture\");\n});\n",
            ),
        ];
        for (path, content) in files {
            fixture.write(path, content);
        }
        std::os::unix::fs::symlink(&modules, fixture.root().join("node_modules")).unwrap();
        fixture.git(&["add", "--all"]);
        fixture.git(&["commit", "--quiet", "--message", "fixture"]);
        let approved = fixture.testry(&["trust", "--approve"]);
        assert_eq!(code(&approved), 0, "{}", stderr(&approved));
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
            "git {arguments:?}: {}",
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

    fn testry(&self, arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_testry"))
            .current_dir(self.root())
            .args(arguments)
            .env("TESTRY_STATE_HOME", self.state.path())
            .env_remove("TESTRY_TRUSTED_CONFIG_DIGEST")
            .env_remove("CI")
            .output()
            .expect("the testry binary must be runnable")
    }

    fn json(&self, arguments: &[&str]) -> (i32, Value) {
        let mut all = arguments.to_vec();
        all.extend_from_slice(&["--format", "json"]);
        let output = self.testry(&all);
        let value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "stdout is not one JSON object ({error}):\n{}\nstderr:\n{}",
                stdout(&output),
                stderr(&output)
            )
        });
        (code(&output), value)
    }

    fn run_directory(&self, result: &Value) -> PathBuf {
        self.root()
            .join(".testry/runs")
            .join(result["run_id"].as_str().unwrap())
    }
}

fn code(output: &Output) -> i32 {
    output
        .status
        .code()
        .expect("the process must exit normally")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn suite<'a>(result: &'a Value, id: &str) -> &'a Value {
    result["suites"]
        .as_array()
        .unwrap()
        .iter()
        .find(|suite| suite["suite_id"] == id)
        .unwrap_or_else(|| panic!("suite {id} has no result in {result}"))
}

fn suite_ids(result: &Value) -> Vec<&str> {
    result["suites"]
        .as_array()
        .unwrap()
        .iter()
        .map(|suite| suite["suite_id"].as_str().unwrap())
        .collect()
}

fn anomaly_codes(suite: &Value) -> Vec<&str> {
    suite["anomalies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|anomaly| anomaly["code"].as_str().unwrap())
        .collect()
}

/// M3: every kind of suite runs through its own adapter, stories as one
/// whole Vitest project and Playwright as a whole suite, and the result
/// keeps them apart.
#[test]
fn verify_full_runs_unit_story_and_browser_suites_with_the_real_runners() {
    let fixture = Fixture::new();
    let (exit, result) = fixture.json(&["verify"]);
    assert_eq!(exit, 0, "{result}");
    // Shown with `--nocapture`, for a rough idea of where the time goes.
    eprintln!("metrics of the full run: {}", result["metrics"]);
    for suite in result["suites"].as_array().unwrap() {
        eprintln!("  {} took {} ms", suite["suite_id"], suite["duration_ms"]);
    }
    assert_eq!(result["outcome"], "passed");
    assert_eq!(result["assurance"], "full_suite");
    assert_eq!(
        suite_ids(&result),
        ["api-unit", "e2e", "stories", "web-unit"]
    );

    let versions = &result["environment"]["tool_versions"];
    assert_eq!(versions["bun"], "1.4.4");
    assert_eq!(versions["vitest"], "5.1.0");
    assert_eq!(versions["playwright"], "1.65.1");

    let stories = suite(&result, "stories");
    assert_eq!(stories["runner"], "storybook_vitest");
    assert_eq!(stories["outcome"], "passed");
    assert_eq!(stories["selection"], json!({ "kind": "all" }));
    assert_eq!(
        stories["executed_files"],
        json!(["apps/web/src/Button.stories.ts"]),
        "the stories project ran, and the unit test of the same configuration did not"
    );
    assert_eq!(
        stories["counts"],
        json!({ "passed": 2, "failed": 0, "skipped": 0, "flaky": 0 })
    );

    let e2e = suite(&result, "e2e");
    assert_eq!(e2e["runner"], "playwright");
    assert_eq!(e2e["outcome"], "passed");
    assert_eq!(
        e2e["executed_files"],
        json!(["tests/e2e/global.setup.ts", "tests/e2e/home.spec.ts"]),
        "R18: the setup project ran with the suite"
    );
    assert_eq!(
        e2e["counts"],
        json!({ "passed": 3, "failed": 0, "skipped": 0, "flaky": 0 })
    );
    assert_eq!(e2e["anomalies"], json!([]));

    // The unit suite of the same Vitest configuration is its own suite.
    let unit = suite(&result, "web-unit");
    assert_eq!(unit["runner"], "vitest");
    assert_eq!(
        unit["executed_files"],
        json!(["apps/web/src/label.test.ts"])
    );

    // The unified report keeps the browser project of every E2E case.
    let run = fixture.run_directory(&result);
    let junit = std::fs::read_to_string(run.join("junit.xml")).unwrap();
    assert!(
        junit.contains(
            "classname=\"e2e[chromium]:tests/e2e/home.spec.ts\" name=\"shows the heading\""
        ),
        "{junit}"
    );
    assert!(
        junit.contains("classname=\"e2e[setup]:tests/e2e/global.setup.ts\""),
        "{junit}"
    );
    assert!(
        junit.contains("classname=\"stories:apps/web/src/Button.stories.ts\" name=\"Labelled\""),
        "{junit}"
    );

    // Nothing was left in the work tree: output of Playwright went to the run.
    assert!(!fixture.root().join("test-results").exists());
    assert!(run.join("tasks/e2e/report.json").is_file());
    assert_eq!(
        fixture.git(&["status", "--porcelain=v1", "--untracked-files=all"]),
        ""
    );
}

/// M3: the result can be compared with what the project's own full run says.
#[test]
fn the_unified_result_agrees_with_the_runner_run_directly() {
    let fixture = Fixture::new();
    let (exit, result) = fixture.json(&["verify"]);
    assert_eq!(exit, 0, "{result}");

    // The same suite, started the way the project would start it itself.
    let report = fixture.root().join(".cache/direct-playwright.json");
    std::fs::create_dir_all(report.parent().unwrap()).unwrap();
    let direct = Command::new("node")
        .current_dir(fixture.root())
        .args([
            "./node_modules/@playwright/test/cli.js",
            "test",
            "--reporter=json",
            "--output=.cache/direct-output",
        ])
        .env("PLAYWRIGHT_JSON_OUTPUT_FILE", &report)
        .output()
        .unwrap();
    assert!(direct.status.success(), "{}", stderr(&direct));
    let direct: Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    assert_eq!(
        suite(&result, "e2e")["counts"]["passed"],
        direct["stats"]["expected"],
        "testry counts the tests the runner counts"
    );
    assert_eq!(direct["stats"]["unexpected"], 0);

    let report = fixture.root().join(".cache/direct-stories.json");
    let direct = Command::new("node")
        .current_dir(fixture.root())
        .args([
            "./node_modules/vitest/vitest.mjs",
            "run",
            "--project",
            "storybook",
            "--reporter=json",
        ])
        .arg(format!("--outputFile={}", report.display()))
        .env("STORYBOOK_DISABLE_TELEMETRY", "1")
        .output()
        .unwrap();
    assert!(direct.status.success(), "{}", stderr(&direct));
    let direct: Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    assert_eq!(
        suite(&result, "stories")["counts"]["passed"],
        direct["numPassedTests"]
    );
}

/// M3: a change in the API narrows the API unit suite to its test file, and
/// the browser suites are neither narrowed nor silently left out.
#[test]
fn an_api_change_narrows_unit_tests_but_never_the_browser_suites() {
    let fixture = Fixture::new();
    fixture.append("apps/api/src/order.ts", "export const extra = 2;\n");

    let (exit, plan) = fixture.json(&["plan"]);
    assert_eq!(exit, 0, "{plan}");
    let planned = |id: &str| {
        plan["suites"]
            .as_array()
            .unwrap()
            .iter()
            .find(|suite| suite["suite_id"] == id)
            .unwrap_or_else(|| panic!("{id} is not planned"))["effective_selection"]
            .clone()
    };
    assert_eq!(
        planned("api-unit"),
        json!({ "kind": "files", "files": ["apps/api/src/order.test.ts"] })
    );
    assert_eq!(planned("stories"), json!({ "kind": "all" }));
    assert_eq!(planned("e2e"), json!({ "kind": "all" }));
    assert_eq!(planned("web-unit")["kind"], "omit");

    let (exit, result) = fixture.json(&["test"]);
    assert_eq!(exit, 0, "{result}");
    assert_eq!(result["assurance"], "advisory");
    assert_eq!(suite_ids(&result), ["api-unit", "e2e", "stories"]);
    assert_eq!(
        suite(&result, "api-unit")["executed_files"],
        json!(["apps/api/src/order.test.ts"])
    );
    assert_eq!(suite(&result, "stories")["counts"]["passed"], 2);
    assert_eq!(suite(&result, "e2e")["counts"]["passed"], 3);
}

/// A failing story fails its own suite. The E2E suite of the same run keeps
/// its own result: a passing component test says nothing about the E2E, and
/// the other way round.
#[test]
fn a_failing_story_fails_only_the_stories_suite() {
    let fixture = Fixture::new();
    fixture.write(
        "apps/web/src/label.ts",
        "export const label = (): string => \"Send\";\n",
    );

    let (exit, result) = fixture.json(&["test"]);
    assert_eq!(exit, 1, "{result}");
    assert_eq!(result["outcome"], "failed");
    let stories = suite(&result, "stories");
    assert_eq!(stories["outcome"], "failed");
    assert_eq!(
        stories["counts"],
        json!({ "passed": 1, "failed": 1, "skipped": 0, "flaky": 0 })
    );
    assert_eq!(suite(&result, "web-unit")["outcome"], "failed");
    assert_eq!(suite(&result, "e2e")["outcome"], "passed");

    let text = fixture.testry(&["test"]);
    assert_eq!(code(&text), 1);
    let shown = stdout(&text);
    assert!(
        shown.contains("failed: apps/web/src/Button.stories.ts > Labelled"),
        "{shown}"
    );
}

/// R18: a failing setup project fails the suite; the tests behind it are
/// reported as not run, never as passed.
#[test]
fn r18_a_failing_setup_project_is_never_skipped_over() {
    let fixture = Fixture::new();
    fixture.write(
        "tests/e2e/global.setup.ts",
        "import { test as setup } from \"@playwright/test\";\nsetup(\"prepare\", async () => { throw new Error(\"no database\"); });\n",
    );
    let (exit, result) = fixture.json(&["test", "--suite", "e2e"]);
    assert_eq!(exit, 1, "{result}");
    let e2e = suite(&result, "e2e");
    assert_eq!(e2e["outcome"], "failed");
    assert_eq!(
        e2e["counts"],
        json!({ "passed": 0, "failed": 1, "skipped": 2, "flaky": 0 })
    );
    assert_eq!(e2e["policy_findings"][0]["code"], "unexpected_skips");
}

/// P10: a test declared as failing verifies nothing. It needs an approval
/// for exactly that test in exactly that browser project.
#[test]
fn p10_an_expected_failure_needs_an_exact_approval() {
    let known = r#"known_skips = [
  { project = "chromium", file = "tests/e2e/home.spec.ts", title_path = ["known bug"], expected_status = "expected_failure" },
]
"#;
    let spec = "import { expect, test } from \"@playwright/test\";\n\
                test(\"works\", () => {});\n\
                test(\"known bug\", () => { test.fail(); expect(1).toBe(2); });\n";

    let unapproved = Fixture::new();
    unapproved.write("tests/e2e/home.spec.ts", spec);
    let (exit, result) = unapproved.json(&["test", "--suite", "e2e"]);
    assert_eq!(exit, 1, "{result}");
    let e2e = suite(&result, "e2e");
    assert_eq!(e2e["outcome"], "failed");
    assert_eq!(
        e2e["counts"],
        json!({ "passed": 2, "failed": 0, "skipped": 1, "flaky": 0 }),
        "the expected failure is not counted as passed"
    );
    assert_eq!(e2e["policy_findings"][0]["code"], "unexpected_skips");

    // `known_skips` belongs to the suite table, before its sub-table.
    let approved_suite =
        E2E_SUITE.replace("[suites.resources]", &format!("{known}[suites.resources]"));
    let approved = Fixture::with_config(&format!("{CONFIG}{approved_suite}"));
    approved.write("tests/e2e/home.spec.ts", spec);
    let (exit, result) = approved.json(&["test", "--suite", "e2e"]);
    assert_eq!(exit, 0, "{result}");
    let e2e = suite(&result, "e2e");
    assert_eq!(e2e["outcome"], "passed");
    assert_eq!(e2e["counts"]["skipped"], 1, "approved, and still visible");
    assert_eq!(e2e["policy_findings"], json!([]));
}

/// A shard in the Playwright configuration makes the suite run a part of
/// itself. Files of the suite are then missing from the report, which is not
/// a pass, and the narrowing is stated.
#[test]
fn a_narrowing_playwright_configuration_is_not_a_full_run() {
    let fixture = Fixture::new();
    fixture.write(
        "tests/e2e/other.spec.ts",
        "import { test } from \"@playwright/test\";\ntest(\"other\", () => {});\n",
    );
    fixture.git(&["add", "--all"]);
    fixture.git(&["commit", "--quiet", "--message", "second spec"]);
    fixture.write(
        "playwright.config.ts",
        &PLAYWRIGHT_CONFIG.replace("testDir:", "shard: { total: 2, current: 1 },\n  testDir:"),
    );

    let (exit, result) = fixture.json(&["verify", "--suite", "e2e"]);
    let e2e = suite(&result, "e2e");
    assert!(
        anomaly_codes(e2e).contains(&"scope_limited_by_runner_config"),
        "{e2e}"
    );
    assert!(
        anomaly_codes(e2e).contains(&"expected_files_missing"),
        "{e2e}"
    );
    assert_eq!(e2e["outcome"], "infrastructure_error");
    assert_eq!(exit, 3, "{result}");
    assert_eq!(result["assurance"], "advisory");
}

/// The suite command may not narrow the run or take over what the adapter
/// owns. That is a configuration error of the whole request: it is found
/// before the first suite starts, so no suite runs, also not the ones whose
/// command is fine.
#[test]
fn a_playwright_command_that_narrows_the_suite_is_refused() {
    for flag in [
        "--grep=home",
        "--no-deps",
        "--pass-with-no-tests",
        "--shard=1/2",
    ] {
        let narrowed = E2E_SUITE.replace("\"test\"]", &format!("\"test\", \"{flag}\"]"));
        let fixture = Fixture::with_config(&format!("{CONFIG}{narrowed}"));
        let (exit, error) = fixture.json(&["verify"]);
        assert_eq!(exit, 2, "{flag}: {error}");
        assert_eq!(error["error"]["code"], "configuration", "{flag}");
        let name = flag.split('=').next().unwrap();
        assert!(
            error["error"]["message"].as_str().unwrap().contains(name),
            "{flag}: {error}"
        );

        // The refused run is recorded as such, and no suite produced anything.
        let runs: Vec<PathBuf> = std::fs::read_dir(fixture.root().join(".testry/runs"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(runs.len(), 1, "{flag}");
        let stored: Value =
            serde_json::from_slice(&std::fs::read(runs[0].join("result.json")).unwrap()).unwrap();
        assert_eq!(stored["outcome"], "infrastructure_error", "{flag}");
        assert_eq!(stored["exit_code"], 2, "{flag}");
        assert_eq!(stored["suites"], json!([]), "{flag}: no suite may have run");
        for suite in ["api-unit", "web-unit", "stories", "e2e"] {
            assert!(
                !runs[0]
                    .join("tasks")
                    .join(suite)
                    .join("stdout.log")
                    .exists(),
                "{flag}: {suite} was started"
            );
        }
    }
}
