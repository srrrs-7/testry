//! End-to-end tests of `testry test`, `testry verify`, `testry runs show` and
//! `testry trust` with the real binary, a real Git repository and the real
//! runners (milestone M2).
//!
//! The fixture is a small monorepo: a Bun suite in `apps/api`, a Vitest suite
//! in `apps/web` and a command gate. These tests need the dev container
//! (Bun, Node and the fixture dependencies); outside of it they fail.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

fn config(api_timeout: u64) -> String {
    format!(
        r#"
schema_version = 1

[workspace]
exclude = ["**/node_modules/**", "node_modules"]

[analysis]
global_inputs = ["**/package.json", "**/bun.lock"]

[execution]
default_environment = "local"
cpu_slots = 4
memory_budget_mib = 4096
max_parallel_suites = 2
termination_grace_seconds = 1

[environments.local]
provider = "host"

[environments.colima]
provider = "devcontainer_cli"
docker_context = "colima"
config = ".devcontainer/devcontainer.json"

[[suites]]
id = "api-unit"
runner = "bun"
cwd = "apps/api"
command = ["bun", "test"]
include = ["apps/api/**/*.test.ts"]
inputs = ["apps/api/**"]
selection = "file_graph"
timeout_seconds = {api_timeout}
[suites.resources]
cpu_slots = 1
memory_mib = 512

[[suites]]
id = "web-unit"
runner = "vitest"
cwd = "apps/web"
command = ["node", "../../node_modules/vitest/vitest.mjs", "run"]
include = ["apps/web/**/*.test.ts"]
inputs = ["apps/web/**"]
selection = "file_graph"
workers = 1
timeout_seconds = 120
[suites.resources]
cpu_slots = 1
memory_mib = 768
"#
    )
}

const GATE: &str = r#"
[[suites]]
id = "lint"
runner = "command"
command = ["node", "-e", "process.exit(0)"]
selection = "whole_suite"
always_run = true
timeout_seconds = 60
[suites.resources]
cpu_slots = 1
memory_mib = 256
"#;

struct Fixture {
    directory: tempfile::TempDir,
    /// The user state directory (trust), outside the workspace.
    state: tempfile::TempDir,
}

impl Fixture {
    /// Bun and Vitest suites plus an always-run gate, committed and trusted.
    fn new() -> Self {
        let fixture = Self::untrusted(&format!("{}{GATE}", config(120)));
        fixture.approve();
        fixture
    }

    /// Without the gate, so an affected run can have no work.
    fn without_gate() -> Self {
        let fixture = Self::untrusted(&config(120));
        fixture.approve();
        fixture
    }

    fn untrusted(config: &str) -> Self {
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
            (".gitignore", "node_modules\n"),
            (
                "package.json",
                r#"{ "name": "fixture", "private": true, "type": "module", "devDependencies": { "vitest": "5.1.0" } }"#,
            ),
            (
                "packages/shared/src/date.ts",
                "export const today = (): string => \"2026-11-17\";\n",
            ),
            (
                "apps/api/src/user.ts",
                "import { today } from \"../../../packages/shared/src/date\";\nexport const user = () => ({ name: \"u\", since: today() });\n",
            ),
            (
                "apps/api/src/user.test.ts",
                "import { expect, test } from \"bun:test\";\nimport { user } from \"./user\";\ntest(\"user\", () => { expect(user().name).toBe(\"u\"); });\n",
            ),
            (
                "apps/api/src/order.ts",
                "export const order = (): number => 1;\n",
            ),
            (
                "apps/api/src/order.test.ts",
                "import { expect, test } from \"bun:test\";\nimport { order } from \"./order\";\ntest(\"order\", () => { expect(order()).toBe(1); });\n",
            ),
            (
                "apps/web/src/App.ts",
                "import { today } from \"../../../packages/shared/src/date\";\nexport const title = (): string => `web ${today()}`;\n",
            ),
            (
                "apps/web/src/App.test.ts",
                "import { expect, test } from \"vitest\";\nimport { title } from \"./App\";\ntest(\"title\", () => { expect(title()).toContain(\"web\"); });\n",
            ),
        ];
        for (path, content) in files {
            fixture.write(path, content);
        }
        std::os::unix::fs::symlink(&modules, fixture.root().join("node_modules")).unwrap();
        fixture.git(&["add", "--all"]);
        fixture.git(&["commit", "--quiet", "--message", "fixture"]);
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

    fn command(&self, arguments: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_testry"));
        command
            .current_dir(self.root())
            .args(arguments)
            .env("TESTRY_STATE_HOME", self.state.path())
            .env_remove("TESTRY_TRUSTED_CONFIG_DIGEST")
            .env_remove("CI");
        command
    }

    fn testry(&self, arguments: &[&str]) -> Output {
        self.command(arguments)
            .output()
            .expect("the testry binary must be runnable")
    }

    fn approve(&self) {
        let output = self.testry(&["trust", "--approve"]);
        assert_eq!(code(&output), 0, "{}", stderr(&output));
    }

    /// Runs a command with `--format json` and returns exit code and the
    /// single JSON object of stdout.
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

    fn run_directories(&self) -> Vec<PathBuf> {
        let runs = self.root().join(".testry/runs");
        if !runs.exists() {
            return Vec::new();
        }
        std::fs::read_dir(runs)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect()
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

fn mode_of(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// Whether a process with this exact argument exists.
fn process_with_argument_exists(marker: &str) -> bool {
    std::fs::read_dir("/proc").unwrap().flatten().any(|entry| {
        std::fs::read(entry.path().join("cmdline")).is_ok_and(|cmdline| {
            cmdline
                .split(|byte| *byte == 0)
                .any(|argument| argument == marker.as_bytes())
        })
    })
}

fn wait_until(mut condition: impl FnMut() -> bool, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    condition()
}

#[test]
fn verify_full_runs_every_suite_with_the_real_runners() {
    let fixture = Fixture::new();
    let (exit, result) = fixture.json(&["verify"]);
    assert_eq!(exit, 0, "{result}");

    assert_eq!(result["schema_version"], 1);
    assert_eq!(result["mode"], "full");
    assert_eq!(result["outcome"], "passed");
    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["assurance"], "full_suite");
    assert_eq!(result["input_validation"], "unchanged");
    assert_eq!(
        result["scope"],
        serde_json::json!(["api-unit", "lint", "web-unit"])
    );
    assert_eq!(
        result["cleanup"],
        serde_json::json!({ "outcome": "succeeded", "residual_resources": [] })
    );
    assert_eq!(result["shadow"], Value::Null);

    let environment = &result["environment"];
    assert_eq!(environment["provider"], "host");
    assert_eq!(environment["tool_versions"]["bun"], "1.4.4");
    assert_eq!(environment["tool_versions"]["vitest"], "5.1.0");

    let api = suite(&result, "api-unit");
    assert_eq!(api["outcome"], "passed");
    assert_eq!(api["runner"], "bun");
    assert_eq!(
        api["counts"],
        serde_json::json!({ "passed": 2, "failed": 0, "skipped": 0, "flaky": 0 })
    );
    assert_eq!(
        api["executed_files"],
        serde_json::json!(["apps/api/src/order.test.ts", "apps/api/src/user.test.ts"])
    );
    assert_eq!(api["report"], "tasks/api-unit/report.xml");
    assert_eq!(api["attempts"], 1);

    let web = suite(&result, "web-unit");
    assert_eq!(web["outcome"], "passed");
    assert_eq!(
        web["executed_files"],
        serde_json::json!(["apps/web/src/App.test.ts"])
    );
    assert_eq!(web["report"], "tasks/web-unit/report.json");

    // P12: the gate succeeded; its test count is unknown, not zero.
    let lint = suite(&result, "lint");
    assert_eq!(lint["outcome"], "passed");
    assert_eq!(lint["counts"], Value::Null);
    assert_eq!(lint["file_attribution"], "unknown");

    // Artifacts of the run.
    let run = fixture.run_directory(&result);
    for file in [
        "run.json",
        "plan.json",
        "events.jsonl",
        "result.json",
        "environment.json",
        "junit.xml",
        "tasks/api-unit/stdout.log",
        "tasks/api-unit/stderr.log",
        "tasks/api-unit/report.xml",
        "tasks/web-unit/report.json",
        "tasks/lint/stdout.log",
    ] {
        let path = run.join(file);
        assert!(path.is_file(), "{file} is missing");
        assert_eq!(mode_of(&path), 0o600, "{file} must be private");
    }
    assert_eq!(mode_of(&run), 0o700);
    let stored: Value =
        serde_json::from_slice(&std::fs::read(run.join("result.json")).unwrap()).unwrap();
    assert_eq!(
        stored, result,
        "stdout and result.json are the same document"
    );
    let junit = std::fs::read_to_string(run.join("junit.xml")).unwrap();
    assert!(junit.contains("<testsuite name=\"api-unit\""));
    assert!(junit.contains("classname=\"web-unit:apps/web/src/App.test.ts\""));

    // The run left the repository clean: its state is ignored by Git.
    assert_eq!(
        fixture.git(&["status", "--porcelain=v1", "--untracked-files=all"]),
        ""
    );
}

#[test]
fn events_are_ordered_and_complete() {
    let fixture = Fixture::new();
    let output = fixture.testry(&["verify", "--format", "jsonl"]);
    assert_eq!(code(&output), 0, "{}", stderr(&output));

    let events: Vec<Value> = stdout(&output)
        .lines()
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|error| panic!("not JSON ({error}): {line}"))
        })
        .collect();
    let sequence: Vec<u64> = events
        .iter()
        .map(|event| event["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(
        sequence,
        (1..=events.len() as u64).collect::<Vec<_>>(),
        "seq must increase by one"
    );
    let types: Vec<&str> = events
        .iter()
        .map(|event| event["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        &types[..4],
        [
            "run_started",
            "plan_created",
            "environment_preparing",
            "environment_ready"
        ]
    );
    assert_eq!(
        &types[types.len() - 3..],
        ["cleanup_started", "cleanup_finished", "run_finished"]
    );
    assert_eq!(
        types.iter().filter(|kind| **kind == "task_started").count(),
        3
    );
    assert_eq!(
        types
            .iter()
            .filter(|kind| **kind == "task_finished")
            .count(),
        3
    );
    for event in &events {
        assert_eq!(event["schema_version"], 1);
        let timestamp = event["timestamp"].as_str().unwrap();
        assert!(
            timestamp.len() == 20 && timestamp.ends_with('Z'),
            "{timestamp}"
        );
        assert!(event["payload"].is_object());
    }
    let finished = events.last().unwrap();
    assert_eq!(
        finished["payload"],
        serde_json::json!({ "outcome": "passed", "exit_code": 0 })
    );

    // The same lines are stored with the run.
    let run_id = events[0]["run_id"].as_str().unwrap();
    let stored = std::fs::read_to_string(
        fixture
            .root()
            .join(".testry/runs")
            .join(run_id)
            .join("events.jsonl"),
    )
    .unwrap();
    assert_eq!(stored, stdout(&output));
}

/// M2: `test --mode affected` runs exactly the affected test file.
#[test]
fn test_affected_runs_only_the_affected_test_file() {
    let fixture = Fixture::new();
    fixture.append("apps/api/src/order.ts", "export const extra = 2;\n");

    let (exit, result) = fixture.json(&["test", "--mode", "affected"]);
    assert_eq!(exit, 0, "{result}");
    assert_eq!(result["mode"], "affected");
    assert_eq!(result["assurance"], "advisory");
    assert_eq!(
        suite_ids(&result),
        ["api-unit", "lint"],
        "web-unit has no affected test"
    );

    let api = suite(&result, "api-unit");
    assert_eq!(
        api["selection"],
        serde_json::json!({ "kind": "files", "files": ["apps/api/src/order.test.ts"] })
    );
    assert_eq!(
        api["executed_files"],
        serde_json::json!(["apps/api/src/order.test.ts"])
    );
    assert_eq!(api["counts"]["passed"], 1);

    // The plan with the reasons is stored next to what really ran.
    let plan: Value = serde_json::from_slice(
        &std::fs::read(fixture.run_directory(&result).join("plan.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(plan["plan_id"], result["plan_id"]);
    assert_eq!(
        plan["changed_paths"],
        serde_json::json!(["apps/api/src/order.ts"])
    );
}

#[test]
fn a_change_in_a_shared_file_reaches_both_runners() {
    let fixture = Fixture::new();
    fixture.append(
        "packages/shared/src/date.ts",
        "export const tomorrow = (): string => \"later\";\n",
    );
    let (exit, result) = fixture.json(&["test"]);
    assert_eq!(exit, 0, "{result}");
    assert_eq!(
        suite(&result, "api-unit")["executed_files"],
        serde_json::json!(["apps/api/src/user.test.ts"])
    );
    assert_eq!(
        suite(&result, "web-unit")["executed_files"],
        serde_json::json!(["apps/web/src/App.test.ts"])
    );
}

#[test]
fn a_failing_test_exits_with_one_and_is_reported() {
    let fixture = Fixture::new();
    fixture.write(
        "apps/api/src/order.ts",
        "export const order = (): number => 2;\n",
    );
    let (exit, result) = fixture.json(&["test"]);
    assert_eq!(exit, 1, "{result}");
    assert_eq!(result["outcome"], "failed");
    let api = suite(&result, "api-unit");
    assert_eq!(api["outcome"], "failed");
    assert_eq!(api["counts"]["failed"], 1);
    assert!(result["metrics"]["first_failure_ms"].is_u64());

    let junit = std::fs::read_to_string(fixture.run_directory(&result).join("junit.xml")).unwrap();
    assert!(junit.contains("<failure message="), "{junit}");

    let text = fixture.testry(&["test"]);
    assert_eq!(code(&text), 1);
    let shown = stdout(&text);
    assert!(shown.contains("Outcome: FAILED"), "{shown}");
    assert!(
        shown.contains("failed: apps/api/src/order.test.ts > order"),
        "{shown}"
    );
}

/// P03: no change means no work: exit 4, and no environment or runner starts.
#[test]
fn p03_no_changes_exit_with_four() {
    let fixture = Fixture::without_gate();
    let (exit, result) = fixture.json(&["test"]);
    assert_eq!(exit, 4, "{result}");
    assert_eq!(result["outcome"], "no_work");
    assert_eq!(result["suites"], serde_json::json!([]));
    assert_eq!(result["environment"], Value::Null);

    let text = fixture.testry(&["test"]);
    assert_eq!(code(&text), 4);
    assert!(stdout(&text).contains("NO WORK"), "{}", stdout(&text));
}

/// P06: shadow runs everything once and records a failure outside the candidate.
#[test]
fn p06_shadow_records_a_failure_outside_the_affected_candidate() {
    let fixture = Fixture::new();
    // Break a test that the change below does not touch.
    fixture.write(
        "apps/web/src/App.test.ts",
        "import { expect, test } from \"vitest\";\ntest(\"title\", () => { expect(1).toBe(2); });\n",
    );
    fixture.git(&["commit", "--quiet", "--all", "--message", "break web"]);
    fixture.append("apps/api/src/order.ts", "export const extra = 2;\n");

    let (exit, result) = fixture.json(&["verify", "--mode", "shadow"]);
    assert_eq!(exit, 1, "{result}");
    assert_eq!(result["assurance"], "full_suite");
    assert_eq!(
        suite_ids(&result),
        ["api-unit", "lint", "web-unit"],
        "everything ran once"
    );
    assert_eq!(
        suite(&result, "api-unit")["selection"],
        serde_json::json!({ "kind": "all" })
    );
    assert_eq!(
        result["shadow"]["gap_candidates"],
        serde_json::json!([{ "suite_id": "web-unit", "file": "apps/web/src/App.test.ts" }])
    );
}

/// P13: a test that changes an input during the run invalidates the result.
#[test]
fn p13_inputs_changed_during_the_run_invalidate_it() {
    let fixture = Fixture::new();
    fixture.write(
        "apps/api/src/order.test.ts",
        "import { expect, test } from \"bun:test\";\nimport { writeFileSync } from \"node:fs\";\nimport { order } from \"./order\";\n\
         test(\"order\", () => { writeFileSync(new URL(\"./generated.ts\", import.meta.url), \"export {};\\n\"); expect(order()).toBe(1); });\n",
    );
    let (exit, result) = fixture.json(&["test"]);
    assert_eq!(exit, 3, "{result}");
    assert_eq!(result["outcome"], "invalidated");
    assert_eq!(result["input_validation"], "changed");
    assert_eq!(
        suite(&result, "api-unit")["outcome"],
        "passed",
        "the suite result itself is kept"
    );
    assert_eq!(result["assurance"], "advisory");
}

/// R05: a suite that exceeds its timeout is stopped with its whole process group.
#[test]
fn r05_a_timeout_stops_the_runner_and_its_children() {
    let fixture = Fixture::untrusted(&config(2));
    fixture.approve();
    fixture.write(
        "apps/api/src/slow.test.ts",
        "import { test } from \"bun:test\";\nimport { spawn } from \"node:child_process\";\n\
         test(\"hangs\", async () => { spawn(\"sleep\", [\"41001\"], { stdio: \"ignore\" }); await new Promise((resolve) => setTimeout(resolve, 600000)); }, 900000);\n",
    );
    let started = Instant::now();
    let (exit, result) = fixture.json(&["test", "--suite", "api-unit"]);
    assert_eq!(exit, 3, "{result}");
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "took {:?}",
        started.elapsed()
    );
    assert_eq!(suite(&result, "api-unit")["outcome"], "timed_out");
    assert_eq!(result["outcome"], "infrastructure_error");
    assert!(
        wait_until(
            || !process_with_argument_exists("41001"),
            Duration::from_secs(5)
        ),
        "a child of the timed out runner survived"
    );
}

fn spawn_long_run(fixture: &Fixture, marker: &str) -> Child {
    fixture.write(
        "apps/api/src/slow.test.ts",
        &format!(
            "import {{ test }} from \"bun:test\";\nimport {{ spawn }} from \"node:child_process\";\n\
             test(\"long\", async () => {{ spawn(\"sleep\", [\"{marker}\"], {{ stdio: \"ignore\" }}); await new Promise((resolve) => setTimeout(resolve, 600000)); }}, 900000);\n"
        ),
    );
    let child = fixture
        .command(&["test", "--suite", "api-unit", "--format", "json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    assert!(
        wait_until(
            || process_with_argument_exists(marker),
            Duration::from_secs(60)
        ),
        "the long test never started"
    );
    child
}

/// R06: SIGINT stops the run, cleans up and exits with 130.
#[test]
fn r06_sigint_cancels_the_run_and_cleans_up() {
    let fixture = Fixture::new();
    let child = spawn_long_run(&fixture, "41002");
    let interrupted = Command::new("sh")
        .args(["-c", &format!("kill -INT {}", child.id())])
        .status()
        .unwrap();
    assert!(interrupted.success());

    let started = Instant::now();
    let output = child.wait_with_output().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "cancellation took {:?}",
        started.elapsed()
    );
    assert_eq!(code(&output), 130, "{}", stderr(&output));

    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["outcome"], "cancelled");
    assert_eq!(result["exit_code"], 130);
    assert_eq!(suite(&result, "api-unit")["outcome"], "cancelled");
    assert_eq!(result["cleanup"]["outcome"], "succeeded");
    assert!(
        wait_until(
            || !process_with_argument_exists("41002"),
            Duration::from_secs(5)
        ),
        "a process of the cancelled run survived"
    );

    // The cancelled run has its terminal record and its events.
    let run = fixture.run_directory(&result);
    assert!(run.join("result.json").is_file());
    let events = std::fs::read_to_string(run.join("events.jsonl")).unwrap();
    assert!(events.contains("\"cleanup_finished\"") && events.contains("\"run_finished\""));
}

/// R17: a second run in the same workspace is refused while one is active.
#[test]
fn r17_a_second_run_in_the_same_workspace_is_refused() {
    let fixture = Fixture::new();
    let mut child = spawn_long_run(&fixture, "41003");

    let (exit, error) = fixture.json(&["test", "--suite", "api-unit"]);
    assert_eq!(exit, 3, "{error}");
    assert_eq!(error["error"]["code"], "workspace_busy");
    assert_eq!(
        fixture.run_directories().len(),
        1,
        "the refused run must not leave a directory"
    );

    Command::new("sh")
        .args(["-c", &format!("kill -INT {}", child.id())])
        .status()
        .unwrap();
    child.wait().unwrap();
    assert!(wait_until(
        || !process_with_argument_exists("41003"),
        Duration::from_secs(5)
    ));

    // The lock is released with the first run.
    std::fs::remove_file(fixture.root().join("apps/api/src/slow.test.ts")).unwrap();
    let (exit, _) = fixture.json(&["verify"]);
    assert_eq!(exit, 0);
}

#[test]
fn nothing_runs_until_the_configuration_is_trusted() {
    let fixture = Fixture::untrusted(&format!("{}{GATE}", config(120)));
    let (exit, status) = fixture.json(&["trust"]);
    assert_eq!(exit, 0);
    assert_eq!(status["status"], "not_trusted");
    assert_eq!(
        status["commands"][0]["argv"],
        serde_json::json!(["bun", "test"])
    );

    let (exit, error) = fixture.json(&["verify"]);
    assert_eq!(exit, 2, "{error}");
    assert_eq!(error["error"]["code"], "trust_required");
    assert!(fixture.run_directories().is_empty(), "nothing was started");

    let (exit, status) = fixture.json(&["trust", "--approve"]);
    assert_eq!(exit, 0);
    assert_eq!(status["status"], "trusted");
    assert_eq!(fixture.json(&["verify"]).0, 0);
    // The approval is stored outside the repository.
    assert!(fixture.state.path().join("trust.json").is_file());
    assert_eq!(
        fixture.git(&["status", "--porcelain=v1", "--untracked-files=all"]),
        ""
    );

    // A changed configuration needs a new approval.
    fixture.append("testry.toml", "\n# a reviewed change\n");
    let (_, status) = fixture.json(&["trust"]);
    assert_eq!(status["status"], "configuration_changed");
    let (exit, error) = fixture.json(&["verify"]);
    assert_eq!(exit, 2);
    assert_eq!(error["error"]["code"], "trust_required");

    let (_, status) = fixture.json(&["trust", "--revoke"]);
    assert_eq!(status["status"], "not_trusted");
}

#[test]
fn a_job_definition_can_pin_the_trusted_configuration_digest() {
    let fixture = Fixture::untrusted(&format!("{}{GATE}", config(120)));
    let (_, status) = fixture.json(&["trust"]);
    let digest = status["config_digest"].as_str().unwrap().to_owned();

    let pinned = fixture
        .command(&["verify", "--ci", "--format", "json"])
        .env("TESTRY_TRUSTED_CONFIG_DIGEST", &digest)
        .output()
        .unwrap();
    assert_eq!(code(&pinned), 0, "{}", stderr(&pinned));

    // A pull request that changes the configuration no longer matches the pin.
    fixture.append("testry.toml", "\n# changed in a pull request\n");
    let changed = fixture
        .command(&["verify", "--ci", "--format", "json"])
        .env("TESTRY_TRUSTED_CONFIG_DIGEST", &digest)
        .output()
        .unwrap();
    assert_eq!(code(&changed), 2);

    let invalid = fixture
        .command(&["verify", "--format", "json"])
        .env("TESTRY_TRUSTED_CONFIG_DIGEST", "true")
        .output()
        .unwrap();
    assert_eq!(
        code(&invalid),
        2,
        "a value that is not a digest is not an approval"
    );
}

#[test]
fn runs_show_reads_finished_and_interrupted_runs() {
    let fixture = Fixture::new();
    let (_, result) = fixture.json(&["verify"]);
    let run_id = result["run_id"].as_str().unwrap();

    let (exit, shown) = fixture.json(&["runs", "show", run_id]);
    assert_eq!(exit, 0);
    assert_eq!(shown, result);
    let text = fixture.testry(&["runs", "show", run_id]);
    assert!(
        stdout(&text).contains("outcome: passed"),
        "{}",
        stdout(&text)
    );

    // A run that never wrote its terminal record was interrupted.
    let finished = fixture.run_directory(&result);
    let interrupted_id = "123e4567-e89b-42d3-a456-426614174000";
    let interrupted = fixture.root().join(".testry/runs").join(interrupted_id);
    std::fs::create_dir(&interrupted).unwrap();
    std::fs::copy(finished.join("run.json"), interrupted.join("run.json")).unwrap();
    let (exit, shown) = fixture.json(&["runs", "show", interrupted_id]);
    assert_eq!(exit, 0);
    assert_eq!(shown["state"], "interrupted");
    assert!(
        shown.get("outcome").is_none(),
        "an interrupted run has no outcome: {shown}"
    );

    let (exit, error) = fixture.json(&["runs", "show", "00000000-0000-4000-8000-000000000000"]);
    assert_eq!(exit, 2, "{error}");
    let (exit, _) = fixture.json(&["runs", "show", "../../etc/passwd"]);
    assert_eq!(exit, 2);
}

#[test]
fn each_run_has_its_own_artifacts() {
    let fixture = Fixture::new();
    let (_, first) = fixture.json(&["verify"]);
    let (_, second) = fixture.json(&["verify"]);
    assert_ne!(first["run_id"], second["run_id"]);
    assert_eq!(
        first["plan_id"], second["plan_id"],
        "the same inputs give the same plan"
    );
    assert_eq!(fixture.run_directories().len(), 2);
    assert!(
        fixture
            .run_directory(&first)
            .join("tasks/api-unit/report.xml")
            .is_file()
    );
    assert!(
        fixture
            .run_directory(&second)
            .join("tasks/api-unit/report.xml")
            .is_file()
    );
}

#[test]
fn unsupported_environments_are_refused() {
    let fixture = Fixture::new();
    let (exit, error) = fixture.json(&["verify", "--environment", "colima"]);
    assert_eq!(exit, 2, "{error}");
    assert_eq!(error["error"]["code"], "unsupported_capability");
    let (exit, _) = fixture.json(&["verify", "--environment", "nope"]);
    assert_eq!(exit, 2);
    assert!(fixture.run_directories().is_empty());
}

/// P10: a new skip violates the default policy; the runner's own exit code was 0.
#[test]
fn p10_an_unexpected_skip_fails_the_run() {
    let fixture = Fixture::new();
    fixture.append(
        "apps/api/src/order.test.ts",
        "test.skip(\"not yet\", () => {});\n",
    );
    let (exit, result) = fixture.json(&["test"]);
    assert_eq!(exit, 1, "{result}");
    let api = suite(&result, "api-unit");
    assert_eq!(api["outcome"], "failed");
    assert_eq!(
        api["counts"],
        serde_json::json!({ "passed": 1, "failed": 0, "skipped": 1, "flaky": 0 })
    );
    assert_eq!(api["policy_findings"][0]["code"], "unexpected_skips");
    assert_eq!(api["policy_findings"][0]["fails_suite"], true);
}

// ---------------------------------------------------- history and gc (M5) --

fn listed_runs(fixture: &Fixture) -> Vec<Value> {
    let (exit, list) = fixture.json(&["runs", "list"]);
    assert_eq!(exit, 0, "{list}");
    assert_eq!(list["schema_version"], 1);
    list["runs"].as_array().unwrap().clone()
}

#[test]
fn runs_are_listed_from_the_history_newest_first() {
    let fixture = Fixture::new();
    assert!(listed_runs(&fixture).is_empty());
    assert!(
        !fixture.root().join(".testry/state.sqlite3").exists(),
        "listing creates no history"
    );

    let (_, first) = fixture.json(&["verify"]);
    fixture.write(
        "apps/api/src/order.ts",
        "export const order = (): number => 2;\n",
    );
    let (exit, second) = fixture.json(&["test"]);
    assert_eq!(exit, 1);

    let runs = listed_runs(&fixture);
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0]["run_id"], second["run_id"], "newest first");
    assert_eq!(runs[0]["state"], "finished");
    assert_eq!(runs[0]["mode"], "affected");
    assert_eq!(runs[0]["outcome"], "failed");
    assert_eq!(runs[0]["exit_code"], 1);
    assert_eq!(runs[0]["assurance"], "advisory");
    assert_eq!(runs[1]["run_id"], first["run_id"]);
    assert_eq!(runs[1]["outcome"], "passed");
    assert_eq!(runs[1]["assurance"], "full_suite");
    assert_eq!(runs[1]["plan_id"], first["plan_id"]);
    assert!(runs[1]["total_ms"].is_u64());
    for run in &runs {
        let started = run["started_at"].as_str().unwrap();
        assert!(started.len() == 20 && started.ends_with('Z'), "{started}");
    }

    let (exit, limited) = fixture.json(&["runs", "list", "--limit", "1"]);
    assert_eq!(exit, 0);
    assert_eq!(limited["runs"].as_array().unwrap().len(), 1);

    let text = fixture.testry(&["runs", "list"]);
    let shown = stdout(&text);
    assert!(shown.contains("failed (exit 1), advisory"), "{shown}");
    assert!(shown.contains("passed (exit 0), full_suite"), "{shown}");

    // The history is private and ignored by Git like the rest of the state.
    let database = fixture.root().join(".testry/state.sqlite3");
    assert_eq!(mode_of(&database), 0o600);
    assert_eq!(
        fixture.git(&["status", "--porcelain=v1", "--untracked-files=all"]),
        "M apps/api/src/order.ts",
        "only the edit of this test is visible to Git"
    );
}

/// R15: testry itself is killed in the middle of a run. The run has no
/// result, its runner and the runner's children keep running. Afterwards the
/// run is shown as interrupted and never as a result, what it left behind
/// can be seen without changing anything, and `gc --apply` stops it.
#[test]
fn r15_a_run_whose_testry_was_killed_is_found_and_cleaned_up() {
    let fixture = Fixture::new();
    let mut child = spawn_long_run(&fixture, "41004");
    let killed = Command::new("sh")
        .args(["-c", &format!("kill -KILL {}", child.id())])
        .status()
        .unwrap();
    assert!(killed.success());
    child.wait().unwrap();
    assert!(
        process_with_argument_exists("41004"),
        "the runner outlives a killed testry; that is what this test is about"
    );

    // No terminal record: interrupted, without an outcome.
    let runs = listed_runs(&fixture);
    assert_eq!(runs.len(), 1);
    let run_id = runs[0]["run_id"].as_str().unwrap().to_owned();
    assert_eq!(runs[0]["state"], "interrupted");
    assert_eq!(runs[0]["outcome"], Value::Null);
    assert_eq!(runs[0]["exit_code"], Value::Null);
    let (exit, shown) = fixture.json(&["runs", "show", &run_id]);
    assert_eq!(exit, 0);
    assert_eq!(shown["state"], "interrupted");
    let text = fixture.testry(&["runs", "list"]);
    assert!(stdout(&text).contains("INTERRUPTED"), "{}", stdout(&text));

    // A dry run shows the processes the run left behind and touches nothing.
    let (exit, dry) = fixture.json(&["gc", "--dry-run"]);
    assert_eq!(exit, 0, "{dry}");
    assert_eq!(dry["mode"], "dry_run");
    assert_eq!(dry["run_active"], false);
    assert_eq!(dry["history"], Value::Null);
    let leftovers = dry["leftover_processes"].as_array().unwrap();
    assert!(!leftovers.is_empty(), "{dry}");
    assert!(
        leftovers.iter().all(
            |process| process["run_id"] == run_id.as_str() && process["action"] == "would_stop"
        ),
        "{dry}"
    );
    assert!(
        leftovers.iter().any(|process| process["name"] == "sleep"),
        "{dry}"
    );
    let interrupted = dry["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|run| run["run_id"] == run_id.as_str())
        .unwrap();
    assert_eq!(interrupted["state"], "interrupted");
    assert_eq!(
        interrupted["action"], "keep",
        "it is evidence until it is old"
    );
    assert!(
        process_with_argument_exists("41004"),
        "a dry run stops nothing"
    );

    // Applying stops them, and repairs the history.
    let (exit, applied) = fixture.json(&["gc", "--apply"]);
    assert_eq!(exit, 0, "{applied}");
    assert_eq!(applied["mode"], "apply");
    assert_eq!(applied["failures"], serde_json::json!([]));
    assert_eq!(
        applied["history"]["interrupted"],
        serde_json::json!([run_id])
    );
    assert!(
        applied["leftover_processes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|process| process["action"] == "stopped"),
        "{applied}"
    );
    assert!(
        wait_until(
            || !process_with_argument_exists("41004"),
            Duration::from_secs(5)
        ),
        "the leftover process survived `gc --apply`"
    );
    assert_eq!(applied["removed_runs"], serde_json::json!([]));
    assert!(fixture.root().join(".testry/runs").join(&run_id).is_dir());

    // Nothing is left to do, and the workspace runs again.
    let (_, again) = fixture.json(&["gc", "--dry-run"]);
    assert_eq!(again["leftover_processes"], serde_json::json!([]));
    std::fs::remove_file(fixture.root().join("apps/api/src/slow.test.ts")).unwrap();
    let (exit, result) = fixture.json(&["verify"]);
    assert_eq!(exit, 0, "{result}");
    let runs = listed_runs(&fixture);
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0]["state"], "finished");
    assert_eq!(runs[1]["state"], "interrupted");
    assert_eq!(runs[1]["outcome"], Value::Null);
}

fn age(path: &Path, days: u64) {
    let file = std::fs::File::options().write(true).open(path).unwrap();
    file.set_modified(std::time::SystemTime::now() - Duration::from_secs(days * 86_400))
        .unwrap();
}

/// Runs older than `storage.history_retention_days` are removed with their
/// rows; newer ones and anything testry does not own stay.
#[test]
fn gc_removes_only_what_is_older_than_the_retention_and_owned() {
    let fixture = Fixture::untrusted(&format!(
        "{}{GATE}\n[storage]\nhistory_retention_days = 3\n",
        config(120)
    ));
    fixture.approve();
    let (_, old) = fixture.json(&["verify"]);
    let (_, recent) = fixture.json(&["verify"]);
    let old_id = old["run_id"].as_str().unwrap();
    let recent_id = recent["run_id"].as_str().unwrap();
    age(&fixture.run_directory(&old).join("result.json"), 4);
    age(&fixture.run_directory(&recent).join("result.json"), 2);
    let notes = fixture.root().join(".testry/runs/notes.txt");
    std::fs::write(&notes, "not a run").unwrap();
    age(&notes, 400);

    let (exit, dry) = fixture.json(&["gc", "--dry-run"]);
    assert_eq!(exit, 0, "{dry}");
    assert_eq!(dry["retention_days"], 3);
    let action = |report: &Value, run_id: &str| {
        report["runs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|run| run["run_id"] == run_id)
            .unwrap_or_else(|| panic!("{run_id} is not in {report}"))["action"]
            .clone()
    };
    assert_eq!(action(&dry, old_id), "delete");
    assert_eq!(action(&dry, recent_id), "keep");
    assert_eq!(dry["unmanaged"], serde_json::json!(["runs/notes.txt"]));
    assert!(dry["freed_bytes"].as_u64().unwrap() > 0);
    assert_eq!(dry["removed_runs"], serde_json::json!([]));
    assert!(
        fixture.run_directory(&old).is_dir(),
        "a dry run removes nothing"
    );
    assert_eq!(listed_runs(&fixture).len(), 2);
    let text = fixture.testry(&["gc", "--dry-run"]);
    assert!(
        stdout(&text).contains("would remove (older_than_retention)"),
        "{}",
        stdout(&text)
    );
    assert!(
        stdout(&text).contains("nothing is changed"),
        "{}",
        stdout(&text)
    );

    let (exit, applied) = fixture.json(&["gc", "--apply"]);
    assert_eq!(exit, 0, "{applied}");
    assert_eq!(applied["removed_runs"], serde_json::json!([old_id]));
    assert_eq!(applied["failures"], serde_json::json!([]));
    assert!(!fixture.run_directory(&old).exists());
    assert!(fixture.run_directory(&recent).join("result.json").is_file());
    assert_eq!(std::fs::read_to_string(&notes).unwrap(), "not a run");

    // The history follows: the removed run is gone, the other is intact.
    let runs = listed_runs(&fixture);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["run_id"], recent_id);
    let (exit, _) = fixture.json(&["runs", "show", old_id]);
    assert_eq!(exit, 2, "a removed run is not stored any more");
    assert_eq!(fixture.json(&["runs", "show", recent_id]).1, recent);

    // A second pass has nothing to do.
    let (exit, again) = fixture.json(&["gc", "--apply"]);
    assert_eq!(exit, 0);
    assert_eq!(again["removed_runs"], serde_json::json!([]));
    assert_eq!(again["freed_bytes"], 0);
}

/// Artifacts of a run that is in progress are never removed: `gc --apply`
/// needs the workspace for itself.
#[test]
fn gc_apply_is_refused_while_a_run_is_active() {
    let fixture = Fixture::new();
    let mut child = spawn_long_run(&fixture, "41005");

    let (exit, error) = fixture.json(&["gc", "--apply"]);
    assert_eq!(exit, 3, "{error}");
    assert_eq!(error["error"]["code"], "workspace_busy");
    assert!(
        process_with_argument_exists("41005"),
        "the active run was disturbed"
    );

    // A dry run is allowed, says a run is active and does not call the
    // processes of that run leftovers.
    let (exit, dry) = fixture.json(&["gc", "--dry-run"]);
    assert_eq!(exit, 0, "{dry}");
    assert_eq!(dry["run_active"], true);
    assert_eq!(dry["leftover_processes"], serde_json::json!([]));
    let runs = listed_runs(&fixture);
    assert_eq!(runs[0]["state"], "running");

    Command::new("sh")
        .args(["-c", &format!("kill -INT {}", child.id())])
        .status()
        .unwrap();
    child.wait().unwrap();
    assert!(wait_until(
        || !process_with_argument_exists("41005"),
        Duration::from_secs(5)
    ));
    assert_eq!(listed_runs(&fixture)[0]["state"], "finished");
    assert_eq!(listed_runs(&fixture)[0]["outcome"], "cancelled");
}

#[test]
fn gc_in_a_workspace_without_runs_does_nothing() {
    let fixture = Fixture::new();
    let (exit, dry) = fixture.json(&["gc", "--dry-run"]);
    assert_eq!(exit, 0, "{dry}");
    assert_eq!(dry["runs"], serde_json::json!([]));
    assert_eq!(dry["freed_bytes"], 0);
    assert!(
        !fixture.root().join(".testry").exists(),
        "a dry run creates nothing"
    );
}
