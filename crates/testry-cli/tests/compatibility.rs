//! `compatibility.toml` is the record of what was verified. These tests fail
//! when it disagrees with the adapters, the dev image or the runner fixture,
//! so the record cannot claim more (or less) than the tests really run.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use testry_adapters::runners::RunnerRegistry;
use testry_application::ports::execution::{RunnerCapabilities, ToolVersion};
use testry_domain::suite::RunnerKind;
use toml::Table;

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn read(relative: &str) -> String {
    let path = repository_root().join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{} cannot be read: {error}", path.display()))
}

fn compatibility() -> Table {
    read("compatibility.toml").parse::<Table>().unwrap()
}

fn text<'a>(table: &'a Table, path: &[&str]) -> &'a str {
    let mut value = table
        .get(path[0])
        .unwrap_or_else(|| panic!("{path:?} is missing"));
    for key in &path[1..] {
        value = value
            .get(key)
            .unwrap_or_else(|| panic!("{path:?} is missing"));
    }
    value
        .as_str()
        .unwrap_or_else(|| panic!("{path:?} is not a string"))
}

fn flag(table: &Table, runner: &str, name: &str) -> bool {
    table["runners"][runner][name]
        .as_bool()
        .unwrap_or_else(|| panic!("runners.{runner}.{name} is not a boolean"))
}

fn versions(table: &Table, runner: &str) -> Vec<String> {
    table["runners"][runner]["verified_versions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|version| version.as_str().unwrap().to_owned())
        .collect()
}

/// The value of `ARG <name>=<value>` in the Dockerfile.
fn docker_argument(name: &str) -> String {
    let dockerfile = read("Dockerfile");
    let prefix = format!("ARG {name}=");
    dockerfile
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("the Dockerfile has no `{prefix}`"))
        .trim()
        .to_owned()
}

fn recorded_capabilities(
    table: &Table,
    runner: &str,
    version_verified: bool,
) -> RunnerCapabilities {
    RunnerCapabilities {
        full_run: flag(table, runner, "full_run"),
        file_selection: flag(table, runner, "file_selection"),
        file_attribution: flag(table, runner, "file_attribution"),
        structured_report: flag(table, runner, "structured_report"),
        worker_control: flag(table, runner, "worker_control"),
        version_verified,
    }
}

#[test]
fn the_recorded_runner_versions_are_the_ones_the_adapters_accept() {
    let table = compatibility();
    assert_eq!(
        versions(&table, "bun"),
        testry_adapters::runners::bun::VERIFIED_VERSIONS
    );
    assert_eq!(
        versions(&table, "vitest"),
        testry_adapters::runners::vitest::VERIFIED_VERSIONS
    );
    // Stories run through Vitest: the version testry can probe is Vitest's.
    assert_eq!(
        versions(&table, "storybook_vitest"),
        testry_adapters::runners::vitest::VERIFIED_VERSIONS
    );
    assert_eq!(
        [text(
            &table,
            &["runners", "storybook_vitest", "packages", "storybook"]
        )],
        testry_adapters::runners::storybook::VERIFIED_STORYBOOK_VERSIONS
    );
    assert_eq!(
        versions(&table, "playwright"),
        testry_adapters::runners::playwright::VERIFIED_VERSIONS
    );
    assert!(
        versions(&table, "command").is_empty(),
        "a command has no version"
    );
}

#[test]
fn the_recorded_capabilities_are_what_the_adapters_report() {
    let table = compatibility();
    let registry = RunnerRegistry::new();
    let adapters = registry.adapters();

    for (kind, name, tool) in [
        (RunnerKind::Bun, "bun", "bun"),
        (RunnerKind::Vitest, "vitest", "vitest"),
        (RunnerKind::StorybookVitest, "storybook_vitest", "vitest"),
        (RunnerKind::Playwright, "playwright", "playwright"),
    ] {
        assert_eq!(text(&table, &["runners", name, "status"]), "verified");
        let adapter = adapters[&kind];
        for version in versions(&table, name) {
            let observed = ToolVersion {
                tool: tool.to_owned(),
                version: version.clone(),
            };
            assert_eq!(
                adapter.capabilities(Some(&observed)),
                recorded_capabilities(&table, name, true),
                "{name} {version}"
            );
        }

        // A version that is not recorded keeps everything except selection:
        // the whole suite runs.
        let unknown = ToolVersion {
            tool: tool.to_owned(),
            version: "0.0.1".to_owned(),
        };
        let expected = RunnerCapabilities {
            file_selection: false,
            ..recorded_capabilities(&table, name, false)
        };
        assert_eq!(
            adapter.capabilities(Some(&unknown)),
            expected,
            "{name} unknown"
        );
        assert_eq!(adapter.capabilities(None), expected, "{name} unprobed");
    }

    assert_eq!(
        adapters[&RunnerKind::Command].capabilities(None),
        recorded_capabilities(&table, "command", false)
    );

    // Storybook and Playwright never select files, whatever the version.
    for name in ["storybook_vitest", "playwright"] {
        assert!(
            !flag(&table, name, "file_selection"),
            "{name} always runs as a whole"
        );
    }
}

#[test]
fn runners_without_an_adapter_are_recorded_as_unsupported() {
    let table = compatibility();
    let registry = RunnerRegistry::new();
    let adapters = registry.adapters();
    let recorded = table["runners"].as_table().unwrap();

    for (name, entry) in recorded {
        let supported = adapters.keys().any(|kind| kind.as_str() == name);
        let status = entry["status"].as_str().unwrap();
        assert_eq!(
            status == "verified",
            supported,
            "runners.{name} is recorded as `{status}` but an adapter {}",
            if supported {
                "exists"
            } else {
                "does not exist"
            }
        );
    }
    for kind in adapters.keys() {
        assert!(
            recorded.contains_key(kind.as_str()),
            "the {} adapter is missing in compatibility.toml",
            kind.as_str()
        );
    }
    assert_eq!(
        text(&table, &["environments", "devcontainer_cli", "status"]),
        "unsupported"
    );
}

#[test]
fn the_recorded_runtimes_are_the_ones_of_the_dev_image() {
    let table = compatibility();
    assert_eq!(
        text(&table, &["runtimes", "bun"]),
        docker_argument("BUN_VERSION")
    );
    assert_eq!(
        text(&table, &["runtimes", "node"]),
        docker_argument("NODE_VERSION")
    );
    assert_eq!(
        text(&table, &["toolchain", "rust"]),
        docker_argument("RUST_VERSION")
    );
    assert_eq!(
        versions(&table, "bun"),
        [docker_argument("BUN_VERSION")],
        "the contract tests ran with the image's Bun"
    );

    let toolchain = read("rust-toolchain.toml").parse::<Table>().unwrap();
    assert_eq!(
        text(&toolchain, &["toolchain", "channel"]),
        text(&table, &["toolchain", "rust"])
    );
    let image = text(&table, &["platform", "image"]);
    assert!(
        image.contains(text(&table, &["toolchain", "rust"])),
        "{image}"
    );
}

#[test]
fn the_recorded_package_versions_are_the_ones_of_the_runner_fixture() {
    let table = compatibility();
    let manifest: serde_json::Value =
        serde_json::from_str(&read("tests/fixtures/runners/package.json")).unwrap();
    let installed = |package: &str| {
        manifest["devDependencies"][package]
            .as_str()
            .unwrap_or_else(|| panic!("the fixture does not install {package}"))
            .to_owned()
    };

    assert_eq!(versions(&table, "vitest"), [installed("vitest")]);
    assert_eq!(
        versions(&table, "playwright"),
        [installed("@playwright/test")]
    );

    let packages = table["runners"]["storybook_vitest"]["packages"]
        .as_table()
        .unwrap();
    assert!(packages.len() >= 5, "{packages:?}");
    for (package, version) in packages {
        assert_eq!(
            version.as_str().unwrap(),
            installed(package),
            "{package} is recorded with another version than the contract tests ran with"
        );
    }
    // Every dependency of the fixture is pinned to an exact version.
    for (package, version) in manifest["devDependencies"].as_object().unwrap() {
        let version = version.as_str().unwrap();
        assert!(
            version
                .chars()
                .next()
                .is_some_and(|first| first.is_ascii_digit()),
            "{package} = {version:?} is a range, not an exact version"
        );
    }
}
