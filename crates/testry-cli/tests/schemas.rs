//! The bundled JSON Schemas (`schemas/*.schema.json`) against what the
//! binary really writes (spec section 16: "the schemas are checked against
//! the actual serialization").
//!
//! Every document here comes from the real binary running real suites. For
//! every schema there is also a document that must be rejected, so a schema
//! that accepts anything would fail these tests.
//!
//! These tests need the dev container (Bun, Node and the fixture
//! dependencies); outside of it they fail.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn schema(name: &str) -> Value {
    let path = repository_root().join("schemas").join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{} cannot be read: {error}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("{name} is not JSON: {error}"))
}

/// The problems a document has with a schema. Empty means it is valid.
fn problems(schema_name: &str, document: &Value) -> Vec<String> {
    let validator = jsonschema::validator_for(&schema(schema_name))
        .unwrap_or_else(|error| panic!("{schema_name} is not a usable schema: {error}"));
    validator
        .iter_errors(document)
        .map(|error| format!("{} at {}", error, error.instance_path()))
        .collect()
}

#[track_caller]
fn assert_valid(schema_name: &str, document: &Value) {
    let found = problems(schema_name, document);
    assert!(
        found.is_empty(),
        "the document does not match {schema_name}:\n  {}\n{document:#}",
        found.join("\n  ")
    );
}

#[track_caller]
fn assert_invalid(schema_name: &str, document: &Value, why: &str) {
    assert!(
        !problems(schema_name, document).is_empty(),
        "{schema_name} accepts a document it must reject ({why}):\n{document:#}"
    );
}

/// A copy of `document` with the value at `pointer` replaced.
fn with(document: &Value, pointer: &str, value: Value) -> Value {
    let mut changed = document.clone();
    *changed
        .pointer_mut(pointer)
        .unwrap_or_else(|| panic!("{pointer} is not in {document:#}")) = value;
    changed
}

/// A copy of `document` with an extra key in the object at `pointer`.
fn with_extra_key(document: &Value, pointer: &str) -> Value {
    let mut changed = document.clone();
    changed
        .pointer_mut(pointer)
        .and_then(Value::as_object_mut)
        .unwrap_or_else(|| panic!("{pointer} is not an object in {document:#}"))
        .insert("unexpected".to_owned(), json!(true));
    changed
}

const CONFIG: &str = r#"
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

[storage]
history_retention_days = 3

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
known_skips = [{ file = "apps/api/src/order.test.ts", title_path = ["later"], expected_status = "skipped" }]
[suites.resources]
cpu_slots = 1
memory_mib = 512
exclusive = ["database"]

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
    state: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
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
            ("testry.toml", CONFIG),
            (".gitignore", "node_modules\n"),
            (
                "package.json",
                r#"{ "name": "fixture", "private": true, "type": "module", "devDependencies": { "vitest": "5.1.0" } }"#,
            ),
            (
                "apps/api/src/order.ts",
                "export const order = (): number => 1;\n",
            ),
            (
                "apps/api/src/order.test.ts",
                "import { expect, test } from \"bun:test\";\nimport { order } from \"./order\";\ntest(\"order\", () => { expect(order()).toBe(1); });\ntest.skip(\"later\", () => {});\n",
            ),
            (
                "apps/web/src/App.ts",
                "export const title = (): string => \"web\";\n",
            ),
            (
                "apps/web/src/App.test.ts",
                "import { expect, test } from \"vitest\";\nimport { title } from \"./App\";\ntest(\"title\", () => { expect(title()).toBe(\"web\"); });\n",
            ),
        ];
        for (path, content) in files {
            fixture.write(path, content);
        }
        std::os::unix::fs::symlink(&modules, fixture.root().join("node_modules")).unwrap();
        fixture.git(&["add", "--all"]);
        fixture.git(&["commit", "--quiet", "--message", "fixture"]);
        let approved = fixture.testry(&["trust", "--approve"]);
        assert_eq!(approved.status.code(), Some(0));
        fixture
    }

    fn root(&self) -> &Path {
        self.directory.path()
    }

    fn git(&self, arguments: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(self.root())
            .args(arguments)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {arguments:?}");
    }

    fn write(&self, path: &str, content: &str) {
        let full = self.root().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, content).unwrap();
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
        self.command(arguments).output().unwrap()
    }

    /// Exit code and the JSON document on stdout.
    fn json(&self, arguments: &[&str]) -> (i32, Value) {
        let mut all = arguments.to_vec();
        all.extend_from_slice(&["--format", "json"]);
        let output = self.testry(&all);
        let document = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "{arguments:?}: stdout is not one JSON object ({error}):\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (output.status.code().unwrap(), document)
    }

    fn stored(&self, result: &Value, file: &str) -> String {
        std::fs::read_to_string(
            self.root()
                .join(".testry/runs")
                .join(result["run_id"].as_str().unwrap())
                .join(file),
        )
        .unwrap()
    }
}

#[test]
fn every_bundled_schema_is_a_valid_schema() {
    let directory = repository_root().join("schemas");
    let mut names: Vec<String> = std::fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "config.schema.json",
            "error.schema.json",
            "event.schema.json",
            "gc.schema.json",
            "interrupted-run.schema.json",
            "plan.schema.json",
            "result.schema.json",
            "runs-list.schema.json",
        ]
    );
    for name in &names {
        let schema = schema(name);
        assert_eq!(
            schema["$schema"], "https://json-schema.org/draft/2020-12/schema",
            "{name}"
        );
        jsonschema::meta::validate(&schema)
            .unwrap_or_else(|error| panic!("{name} is not a valid schema: {error}"));
        // The schema can be compiled without fetching anything.
        jsonschema::validator_for(&schema).unwrap_or_else(|error| panic!("{name}: {error}"));
    }
}

#[test]
fn plans_match_the_plan_schema() {
    let fixture = Fixture::new();
    // Full: no baseline, no diff.
    let (exit, full) = fixture.json(&["plan", "--mode", "full"]);
    assert_eq!(exit, 0);
    assert_valid("plan.schema.json", &full);
    assert_eq!(full["changed_paths"], Value::Null);

    // Affected without changes: every suite but the gate is omitted.
    let (_, unchanged) = fixture.json(&["plan"]);
    assert_valid("plan.schema.json", &unchanged);

    // Affected with a narrowed suite and an uncertain import in another.
    fixture.write(
        "apps/api/src/order.ts",
        "export const order = (): number => 1;\nexport const extra = 2;\n",
    );
    let (_, narrowed) = fixture.json(&["plan"]);
    assert_valid("plan.schema.json", &narrowed);
    assert_eq!(
        narrowed["suites"][0]["effective_selection"]["kind"],
        "files"
    );
    fixture.write(
        "apps/web/src/App.ts",
        "import { missing } from \"./nope\";\nexport const title = (): string => missing;\n",
    );
    let (_, widened) = fixture.json(&["plan"]);
    assert_valid("plan.schema.json", &widened);
    assert!(!widened["diagnostics"].as_array().unwrap().is_empty());
    assert!(!widened["fallbacks"].as_array().unwrap().is_empty());

    // Shadow: everything runs, the affected selection is the candidate.
    fixture.write(
        "apps/web/src/App.ts",
        "export const title = (): string => \"web\";\n",
    );
    let (_, shadow) = fixture.json(&["plan", "--mode", "shadow"]);
    assert_valid("plan.schema.json", &shadow);
    assert!(shadow["suites"][0]["candidate"].is_object());

    // The stored plan documents are the same documents.
    for entry in std::fs::read_dir(fixture.root().join(".testry/plans")).unwrap() {
        let stored: Value =
            serde_json::from_str(&std::fs::read_to_string(entry.unwrap().path()).unwrap()).unwrap();
        assert_valid("plan.schema.json", &stored);
    }

    // What the schema must refuse.
    assert_invalid(
        "plan.schema.json",
        &with(&narrowed, "/suites/0/effective_selection/files", json!([])),
        "P01: an empty file selection",
    );
    assert_invalid(
        "plan.schema.json",
        &with(
            &narrowed,
            "/suites/0/effective_selection",
            json!({ "kind": "some" }),
        ),
        "an unknown selection kind",
    );
    assert_invalid(
        "plan.schema.json",
        &with(&full, "/assurance", json!("guaranteed")),
        "an unknown assurance",
    );
    assert_invalid(
        "plan.schema.json",
        &with(&full, "/plan_id", json!("sha256:abc")),
        "a plan ID that is not a fingerprint",
    );
    assert_invalid(
        "plan.schema.json",
        &with(&full, "/schema_version", json!(2)),
        "another schema version",
    );
    assert_invalid(
        "plan.schema.json",
        &with(&narrowed, "/changed_paths", json!(["../outside.ts"])),
        "a path that leaves the repository",
    );
    assert_invalid(
        "plan.schema.json",
        &with_extra_key(&full, ""),
        "an unknown key",
    );
    assert_invalid(
        "plan.schema.json",
        &with_extra_key(&full, "/suites/0"),
        "an unknown key in a suite",
    );
    let mut without_reasons = full.clone();
    without_reasons["suites"][0]["reasons"] = json!([]);
    assert_invalid(
        "plan.schema.json",
        &without_reasons,
        "a selection without a reason",
    );
}

#[test]
fn results_and_events_match_their_schemas() {
    let fixture = Fixture::new();

    // Passed, with a command gate whose counts are unknown.
    let (exit, passed) = fixture.json(&["verify"]);
    assert_eq!(exit, 0, "{passed}");
    assert_valid("result.schema.json", &passed);
    let stored: Value = serde_json::from_str(&fixture.stored(&passed, "result.json")).unwrap();
    assert_valid("result.schema.json", &stored);
    let events = fixture.stored(&passed, "events.jsonl");
    let mut types = std::collections::BTreeSet::new();
    for line in events.lines() {
        let event: Value = serde_json::from_str(line).unwrap();
        assert_valid("event.schema.json", &event);
        types.insert(event["type"].as_str().unwrap().to_owned());
    }
    assert!(
        types.len() >= 9,
        "every event type of a normal run: {types:?}"
    );

    // Failed by a test, and failed by policy (an unapproved skip).
    fixture.write(
        "apps/api/src/order.test.ts",
        "import { expect, test } from \"bun:test\";\nimport { order } from \"./order\";\ntest(\"order\", () => { expect(order()).toBe(2); });\ntest.skip(\"later\", () => {});\ntest.skip(\"unapproved\", () => {});\n",
    );
    let (exit, failed) = fixture.json(&["verify", "--mode", "shadow"]);
    assert_eq!(exit, 1, "{failed}");
    assert_valid("result.schema.json", &failed);
    assert!(failed["shadow"].is_object());
    assert!(
        !failed["suites"][0]["policy_findings"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    for line in fixture.stored(&failed, "events.jsonl").lines() {
        assert_valid("event.schema.json", &serde_json::from_str(line).unwrap());
    }
    fixture.git(&["checkout", "--quiet", "apps/api/src/order.test.ts"]);

    // An affected run that narrowed a suite.
    fixture.write(
        "apps/api/src/order.ts",
        "export const order = (): number => 1;\nexport const extra = 2;\n",
    );
    let (exit, narrowed) = fixture.json(&["test"]);
    assert_eq!(exit, 0, "{narrowed}");
    assert_valid("result.schema.json", &narrowed);
    assert_eq!(narrowed["suites"][0]["selection"]["kind"], "files");
    fixture.git(&["checkout", "--quiet", "apps/api/src/order.ts"]);

    // No work: exit code 4, no suites, no environment.
    let (exit, no_work) = fixture.json(&["test", "--suite", "api-unit"]);
    assert_eq!(exit, 4, "{no_work}");
    assert_valid("result.schema.json", &no_work);

    // An infrastructure error: the suite exceeds its timeout.
    fixture.write(
        "apps/api/src/slow.test.ts",
        "import { test } from \"bun:test\";\ntest(\"slow\", async () => { await new Promise((resolve) => setTimeout(resolve, 600000)); }, 900000);\n",
    );
    let slow = CONFIG.replace(
        "timeout_seconds = 120\nknown_skips",
        "timeout_seconds = 1\nknown_skips",
    );
    assert_ne!(slow, CONFIG);
    fixture.write("testry.toml", &slow);
    assert_eq!(
        fixture.testry(&["trust", "--approve"]).status.code(),
        Some(0)
    );
    let (exit, timed_out) = fixture.json(&["test", "--suite", "api-unit"]);
    assert_eq!(exit, 3, "{timed_out}");
    assert_valid("result.schema.json", &timed_out);
    assert_eq!(timed_out["suites"][0]["outcome"], "timed_out");

    // What the result schema must refuse.
    assert_invalid(
        "result.schema.json",
        &with(&passed, "/exit_code", json!(1)),
        "passed with a failing exit code",
    );
    assert_invalid(
        "result.schema.json",
        &with(&failed, "/exit_code", json!(0)),
        "failed with exit code 0",
    );
    assert_invalid(
        "result.schema.json",
        &with(&no_work, "/exit_code", json!(0)),
        "P03: no work reported as success",
    );
    assert_invalid(
        "result.schema.json",
        &with(&passed, "/outcome", json!("ok")),
        "an unknown outcome",
    );
    let gate = passed["suites"]
        .as_array()
        .unwrap()
        .iter()
        .position(|suite| suite["suite_id"] == "lint")
        .unwrap();
    assert_eq!(passed["suites"][gate]["counts"], Value::Null, "P12");
    assert_invalid(
        "result.schema.json",
        &with(
            &passed,
            &format!("/suites/{gate}/executed_files"),
            json!([]),
        ),
        "unknown file attribution written as an empty list",
    );
    assert_invalid(
        "result.schema.json",
        &with(
            &passed,
            "/suites/0/counts",
            json!({ "passed": 1, "failed": 0, "skipped": 0 }),
        ),
        "counts without the flaky count",
    );
    assert_invalid(
        "result.schema.json",
        &with(&passed, "/suites/0/outcome", json!("success")),
        "an unknown suite outcome",
    );
    assert_invalid(
        "result.schema.json",
        &with_extra_key(&passed, ""),
        "an unknown key",
    );
    assert_invalid(
        "result.schema.json",
        &with_extra_key(&passed, "/metrics"),
        "an unknown metric",
    );

    // What the event schema must refuse.
    let first: Value = serde_json::from_str(events.lines().next().unwrap()).unwrap();
    assert_invalid(
        "event.schema.json",
        &with(&first, "/seq", json!(0)),
        "a sequence number below 1",
    );
    assert_invalid(
        "event.schema.json",
        &with(&first, "/type", json!("log")),
        "an unknown event type",
    );
    assert_invalid(
        "event.schema.json",
        &with(
            &first,
            "/payload",
            json!({ "mode": "full", "log": "the whole output" }),
        ),
        "a payload with more than the event defines",
    );
    assert_invalid(
        "event.schema.json",
        &with(&first, "/timestamp", json!("yesterday")),
        "a timestamp that is not UTC",
    );
    // The payload belongs to the type: the payload of another event is wrong.
    assert_invalid(
        "event.schema.json",
        &with(&first, "/payload", json!({ "complete": true })),
        "the payload of another event type",
    );
    // An event that never occurred in these runs, as the adapter writes it.
    assert_valid(
        "event.schema.json",
        &json!({
            "schema_version": 1,
            "run_id": first["run_id"],
            "seq": 5,
            "timestamp": "2026-11-17T09:00:00Z",
            "type": "selection_expanded",
            "payload": { "suite_id": "api-unit", "from": "files", "to": "all", "reason_code": "unsupported_runner_version" }
        }),
    );
}

fn wait_for_process(marker: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let found = std::fs::read_dir("/proc").unwrap().flatten().any(|entry| {
            std::fs::read(entry.path().join("cmdline")).is_ok_and(|cmdline| {
                cmdline
                    .split(|byte| *byte == 0)
                    .any(|argument| argument == marker.as_bytes())
            })
        });
        if found {
            return;
        }
        assert!(Instant::now() < deadline, "the long test never started");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn errors_history_and_gc_documents_match_their_schemas() {
    let fixture = Fixture::new();

    // Errors, with different codes.
    let (exit, unknown_environment) = fixture.json(&["verify", "--environment", "nope"]);
    assert_eq!(exit, 2);
    assert_valid("error.schema.json", &unknown_environment);
    let (exit, gate_rule) = fixture.json(&["verify", "--ci", "--mode", "affected"]);
    assert_eq!(exit, 2);
    assert_valid("error.schema.json", &gate_rule);
    assert_ne!(
        unknown_environment["error"]["code"],
        gate_rule["error"]["code"]
    );
    assert_invalid(
        "error.schema.json",
        &json!({ "schema_version": 1, "error": { "message": "no code" } }),
        "an error without a machine-readable code",
    );
    assert_invalid(
        "error.schema.json",
        &with_extra_key(&unknown_environment, "/error"),
        "an unknown key",
    );

    // An empty history, then a finished and an interrupted run.
    let (_, empty) = fixture.json(&["runs", "list"]);
    assert_valid("runs-list.schema.json", &empty);
    let (exit, finished) = fixture.json(&["verify"]);
    assert_eq!(exit, 0);

    fixture.write(
        "apps/api/src/slow.test.ts",
        "import { test } from \"bun:test\";\nimport { spawn } from \"node:child_process\";\n\
         test(\"long\", async () => { spawn(\"sleep\", [\"42001\"], { stdio: \"ignore\" }); await new Promise((resolve) => setTimeout(resolve, 600000)); }, 900000);\n",
    );
    let mut child = fixture
        .command(&["test", "--suite", "api-unit", "--format", "json"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for_process("42001");
    // While the run is active.
    let (_, during) = fixture.json(&["runs", "list"]);
    assert_valid("runs-list.schema.json", &during);
    assert_eq!(during["runs"][0]["state"], "running");
    let (_, dry_while_active) = fixture.json(&["gc", "--dry-run"]);
    assert_valid("gc.schema.json", &dry_while_active);

    Command::new("sh")
        .args(["-c", &format!("kill -KILL {}", child.id())])
        .status()
        .unwrap();
    child.wait().unwrap();

    let (_, list) = fixture.json(&["runs", "list"]);
    assert_valid("runs-list.schema.json", &list);
    assert_eq!(list["runs"][0]["state"], "interrupted");
    assert_eq!(list["runs"][1]["state"], "finished");
    let interrupted_id = list["runs"][0]["run_id"].as_str().unwrap();
    let (exit, interrupted) = fixture.json(&["runs", "show", interrupted_id]);
    assert_eq!(exit, 0);
    assert_valid("interrupted-run.schema.json", &interrupted);
    // An interrupted run is not a result, and a result is not an interrupted run.
    assert_invalid(
        "result.schema.json",
        &interrupted,
        "an interrupted run read as a result",
    );
    let (_, shown) = fixture.json(&["runs", "show", finished["run_id"].as_str().unwrap()]);
    assert_valid("result.schema.json", &shown);
    assert_invalid(
        "interrupted-run.schema.json",
        &shown,
        "a result read as an interrupted run",
    );

    // gc: a dry run with leftovers, then applying it.
    let (exit, dry) = fixture.json(&["gc", "--dry-run"]);
    assert_eq!(exit, 0);
    assert_valid("gc.schema.json", &dry);
    assert!(!dry["leftover_processes"].as_array().unwrap().is_empty());
    let (exit, applied) = fixture.json(&["gc", "--apply"]);
    assert_eq!(exit, 0, "{applied}");
    assert_valid("gc.schema.json", &applied);
    assert!(applied["history"].is_object());

    // What the schemas must refuse.
    assert_invalid(
        "runs-list.schema.json",
        &with(&list, "/runs/0/outcome", json!("passed")),
        "an interrupted run with an outcome",
    );
    assert_invalid(
        "runs-list.schema.json",
        &with(&list, "/runs/1/outcome", Value::Null),
        "a finished run without an outcome",
    );
    assert_invalid(
        "runs-list.schema.json",
        &with(&list, "/runs/0/state", json!("succeeded")),
        "an unknown state",
    );
    assert_invalid(
        "gc.schema.json",
        &with(&dry, "/removed_runs", json!([interrupted_id])),
        "a dry run that removed something",
    );
    assert_invalid(
        "gc.schema.json",
        &with(&dry, "/leftover_processes/0/action", json!("stopped")),
        "a dry run that stopped a process",
    );
    let mut unverified = dry.clone();
    unverified["runs"][0]["state"] = json!("unverified");
    unverified["runs"][0]["action"] = json!("delete");
    assert_invalid(
        "gc.schema.json",
        &unverified,
        "removing a directory that is not proven to be a run",
    );
    assert_invalid(
        "gc.schema.json",
        &with_extra_key(&applied, ""),
        "an unknown key",
    );
}

/// The TOML document as the JSON data model the schema describes.
fn toml_as_json(text: &str) -> Value {
    let table: toml::Table = text.parse().unwrap();
    serde_json::to_value(table).unwrap()
}

#[test]
fn configurations_match_the_config_schema() {
    let sample = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample.testry.toml"),
    )
    .unwrap();
    let sample = toml_as_json(&sample);
    assert_valid("config.schema.json", &sample);
    let fixture = toml_as_json(CONFIG);
    assert_valid("config.schema.json", &fixture);

    // The rules testry enforces when it loads a configuration, as far as the
    // schema can express them.
    assert_invalid(
        "config.schema.json",
        &with_extra_key(&fixture, "/execution"),
        "an unknown key",
    );
    // The fixture has no `[policy]` table; these add one.
    let with_policy = |policy: Value| {
        let mut changed = fixture.clone();
        changed["policy"] = policy;
        changed
    };
    assert_valid(
        "config.schema.json",
        &with_policy(json!({ "ci_default_mode": "shadow", "fail_on_flaky": false })),
    );
    assert_invalid(
        "config.schema.json",
        &with_policy(json!({ "allow_result_cache": true })),
        "the result cache cannot be enabled",
    );
    assert_invalid(
        "config.schema.json",
        &with_policy(json!({ "allow_ai_test_exclusion": true })),
        "excluding tests by AI cannot be enabled",
    );
    assert_invalid(
        "config.schema.json",
        &with_policy(json!({ "ci_default_mode": "affected" })),
        "P07: affected is not a default for CI",
    );
    assert_invalid(
        "config.schema.json",
        &with(&fixture, "/suites/2/selection", json!("file_graph")),
        "file selection for a command",
    );
    assert_invalid(
        "config.schema.json",
        &with(&fixture, "/suites/1/runner", json!("storybook_vitest")),
        "a stories suite without a project",
    );
    assert_invalid(
        "config.schema.json",
        &with(&fixture, "/suites/0/command", json!("bun test")),
        "a command as a shell string",
    );
    assert_invalid(
        "config.schema.json",
        &with(&fixture, "/suites/0/command", json!([])),
        "an empty command",
    );
    let mut bun_with_workers = fixture.clone();
    bun_with_workers["suites"][0]["workers"] = json!(2);
    assert_invalid(
        "config.schema.json",
        &bun_with_workers,
        "workers for a runner without worker control",
    );
    let mut vitest_without_workers = fixture.clone();
    vitest_without_workers["suites"][1]
        .as_object_mut()
        .unwrap()
        .remove("workers");
    assert_invalid(
        "config.schema.json",
        &vitest_without_workers,
        "a vitest suite without workers",
    );
    assert_invalid(
        "config.schema.json",
        &with(
            &fixture,
            "/suites/0/known_skips/0/expected_status",
            json!("any"),
        ),
        "an unknown skip status",
    );
    assert_invalid(
        "config.schema.json",
        &with(&fixture, "/environments", json!({})),
        "no environment",
    );
    assert_invalid(
        "config.schema.json",
        &with(
            &fixture,
            "/environments/local",
            json!({ "provider": "docker" }),
        ),
        "an unknown provider",
    );
    assert_invalid(
        "config.schema.json",
        &with(&fixture, "/schema_version", json!(2)),
        "another schema version",
    );
}
