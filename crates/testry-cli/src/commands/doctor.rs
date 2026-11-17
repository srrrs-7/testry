//! `testry doctor`: read-only diagnostics.
//!
//! Doctor installs nothing, deletes nothing, starts no environment and
//! switches no Docker context. Checks that are not implemented yet are
//! reported as `not_checked`; they are never reported as passing.

use serde::Serialize;
use testry_adapters::runners::RunnerRegistry;
use testry_adapters::runtime::host::container_marker_present;
use testry_adapters::source::git::GitSourceRepository;
use testry_application::ports::planning::{SnapshotRequest, SourceRepository};
use testry_application::settings::{EnvironmentSettings, Settings};
use testry_domain::pattern::PatternSet;
use testry_domain::plan::Mode;
use testry_domain::results::ExitCondition;
use testry_domain::suite::RunnerKind;

use super::trust::{TrustState, trust_state};
use crate::args::{Cli, DoctorArgs, Format};
use crate::composition_root::{ALLOW_OUTSIDE_CONTAINER_VARIABLE, operation_context};
use crate::config::{self, LoadedConfig};
use crate::error::CliError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Ok,
    Warning,
    Error,
    NotChecked,
}

#[derive(Debug, Serialize)]
struct Check {
    id: &'static str,
    status: Status,
    detail: String,
}

#[derive(Debug, Serialize)]
struct Report {
    schema_version: u32,
    testry_version: &'static str,
    status: Status,
    checks: Vec<Check>,
}

fn check(id: &'static str, status: Status, detail: impl Into<String>) -> Check {
    Check {
        id,
        status,
        detail: detail.into(),
    }
}

pub async fn run(cli: &Cli, args: &DoctorArgs) -> Result<ExitCondition, CliError> {
    let mut checks = Vec::new();

    let in_container = container_marker_present();
    let allow_outside =
        std::env::var(ALLOW_OUTSIDE_CONTAINER_VARIABLE).is_ok_and(|value| value == "1");
    checks.push(container_check(in_container, allow_outside));

    let git = GitSourceRepository::new();
    checks.push(match git.version().await {
        Ok(version) => check("git", Status::Ok, version),
        Err(error) => check("git", Status::Error, error.to_string()),
    });

    let loaded = match config::load(&cli.config) {
        Ok(loaded) => {
            checks.push(configuration_check(&loaded));
            Some(loaded)
        }
        Err(error) => {
            checks.push(check("configuration", Status::Error, error.to_string()));
            None
        }
    };

    checks.push(match &loaded {
        Some(loaded) => workspace_check(&git, loaded).await?,
        None => check(
            "workspace",
            Status::NotChecked,
            "needs a valid configuration",
        ),
    });

    match &loaded {
        Some(loaded) => {
            let settings = &loaded.settings;
            checks.push(
                match trust_state(&loaded.workspace_root, settings.config_digest()) {
                    Ok(state) => trust_check(state),
                    Err(error) => check("trust", Status::Error, error.message),
                },
            );
            let registry = RunnerRegistry::new();
            let supported: Vec<RunnerKind> = registry.adapters().into_keys().collect();
            checks.push(runners_check(settings, &supported));
            checks.push(environments_check(settings));
        }
        None => {
            for id in ["trust", "runners", "environments"] {
                checks.push(check(id, Status::NotChecked, "needs a valid configuration"));
            }
        }
    }
    // Doctor starts no command of the configuration, so it cannot know which
    // runner versions are installed. Every run probes them before it starts.
    checks.push(check(
        "runner_versions",
        Status::NotChecked,
        format!(
            "doctor does not execute suite commands. `test` and `verify` probe the installed \
             version before the first suite starts; a version outside the verified set \
             (bun {}, vitest {}) runs the whole suite instead of selected files",
            testry_adapters::runners::bun::VERIFIED_VERSIONS.join(", "),
            testry_adapters::runners::vitest::VERIFIED_VERSIONS.join(", "),
        ),
    ));

    let status = if checks.iter().any(|check| check.status == Status::Error) {
        Status::Error
    } else if checks.iter().any(|check| check.status == Status::Warning) {
        Status::Warning
    } else {
        Status::Ok
    };
    let report = Report {
        schema_version: 1,
        testry_version: env!("CARGO_PKG_VERSION"),
        status,
        checks,
    };

    match args.format {
        Format::Json => {
            let json = serde_json::to_string_pretty(&report).map_err(|error| {
                CliError::storage(format!("the report could not be serialized: {error}"))
            })?;
            println!("{json}");
        }
        Format::Text => {
            println!("testry {} doctor", report.testry_version);
            for check in &report.checks {
                let label = match check.status {
                    Status::Ok => "ok",
                    Status::Warning => "warning",
                    Status::Error => "error",
                    Status::NotChecked => "not checked",
                };
                println!("  [{label}] {}: {}", check.id, check.detail);
            }
        }
    }

    // A problem found by doctor is a configuration problem of the setup.
    Ok(if status == Status::Error {
        ExitCondition::Configuration
    } else {
        ExitCondition::Success
    })
}

/// ADR-0002: this project runs testry and every runtime inside containers.
fn container_check(in_container: bool, allow_outside: bool) -> Check {
    if in_container {
        check("container", Status::Ok, "testry runs inside a container")
    } else if allow_outside {
        check(
            "container",
            Status::Warning,
            format!(
                "no container marker was found and {ALLOW_OUTSIDE_CONTAINER_VARIABLE}=1 is set: \
                 `test` and `verify` execute on this machine, which is unverified"
            ),
        )
    } else {
        check(
            "container",
            Status::Warning,
            "no container marker was found. `plan` and `explain` work, but `test` and `verify` \
             refuse to execute outside a container; execution outside one is unverified",
        )
    }
}

fn trust_check(state: TrustState) -> Check {
    match state {
        TrustState::Trusted => check(
            "trust",
            Status::Ok,
            "this configuration is approved; `test` and `verify` may run its commands",
        ),
        TrustState::ConfigurationChanged => check(
            "trust",
            Status::Warning,
            "the configuration changed since it was approved; `test` and `verify` refuse to run \
             until the commands are reviewed again with `testry trust`",
        ),
        TrustState::NotTrusted => check(
            "trust",
            Status::Warning,
            "this configuration is not approved; `test` and `verify` refuse to run until the \
             commands are reviewed with `testry trust` and approved with `testry trust --approve`",
        ),
    }
}

/// Which suites this build can execute. A suite without an adapter is never
/// run by another adapter: `test` and `verify` refuse the whole request.
fn runners_check(settings: &Settings, supported: &[RunnerKind]) -> Check {
    let unsupported: Vec<String> = settings
        .suites()
        .iter()
        .filter(|suite| !supported.contains(&suite.runner()))
        .map(|suite| format!("{} ({})", suite.id(), suite.runner().as_str()))
        .collect();
    let adapters: Vec<&str> = supported.iter().map(|runner| runner.as_str()).collect();
    if unsupported.is_empty() {
        check(
            "runners",
            Status::Ok,
            format!(
                "every suite has a runner adapter in this build ({})",
                adapters.join(", ")
            ),
        )
    } else {
        check(
            "runners",
            Status::Warning,
            format!(
                "no adapter in this build for: {}. A request that includes one of these suites \
                 is refused (exit 2); it can still be planned. Adapters in this build: {}",
                unsupported.join(", "),
                adapters.join(", ")
            ),
        )
    }
}

/// Which configured environments this build can execute in. Nothing is
/// started or contacted here.
fn environments_check(settings: &Settings) -> Check {
    let mut unsupported = Vec::new();
    for (name, environment) in settings.environments() {
        if matches!(environment, EnvironmentSettings::DevcontainerCli(_)) {
            unsupported.push(name.as_str());
        }
    }
    let default = settings.default_environment().as_str();
    if unsupported.contains(&default) {
        check(
            "environments",
            Status::Warning,
            format!(
                "the default environment `{default}` uses the Dev Container provider, which \
                 testry does not implement (ADR-0006): `test` and `verify` are refused unless \
                 --environment names a host environment"
            ),
        )
    } else if unsupported.is_empty() {
        check(
            "environments",
            Status::Ok,
            format!(
                "the default environment `{default}` uses the host provider: commands run as \
                 child processes of testry, without isolation between suites"
            ),
        )
    } else {
        check(
            "environments",
            Status::NotChecked,
            format!(
                "the default environment `{default}` uses the host provider. Not checked, and \
                 always refused by `--environment` (the Dev Container provider is not \
                 implemented, ADR-0006): {}",
                unsupported.join(", ")
            ),
        )
    }
}

fn configuration_check(loaded: &LoadedConfig) -> Check {
    let settings = &loaded.settings;
    let suites: Vec<String> = settings
        .suites()
        .iter()
        .map(|suite| format!("{} ({})", suite.id(), suite.runner().as_str()))
        .collect();
    check(
        "configuration",
        Status::Ok,
        format!(
            "{} is valid: {} suite(s): {}. `full` means exactly these suites; tests that are \
             not registered in a suite are not discovered",
            settings.config_path(),
            suites.len(),
            suites.join(", ")
        ),
    )
}

/// Takes a full-mode snapshot: it needs no commit and no baseline, and it
/// proves that the workspace is a readable Git work tree.
async fn workspace_check(
    git: &GitSourceRepository,
    loaded: &LoadedConfig,
) -> Result<Check, CliError> {
    let request = SnapshotRequest {
        workspace_root: loaded.workspace_root.clone(),
        mode: Mode::Full,
        base_ref: None,
        include_dirty: true,
        scope: None,
        exclude: loaded.settings.exclude().clone(),
        declared_inputs: PatternSet::empty(),
    };
    let context = operation_context()?;
    Ok(match git.snapshot(&request, &context).await {
        Ok(snapshot) => check(
            "workspace",
            Status::Ok,
            format!(
                "{} is the top level of a Git work tree with {} input file(s)",
                loaded.workspace_root.display(),
                snapshot.current().len()
            ),
        ),
        Err(error) => check("workspace", Status::Error, error.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_outside_a_container_is_a_warning_not_a_pass() {
        assert_eq!(container_check(true, false).status, Status::Ok);
        let outside = container_check(false, false);
        assert_eq!(outside.status, Status::Warning);
        assert!(outside.detail.contains("refuse to execute"));
        let allowed = container_check(false, true);
        assert_eq!(allowed.status, Status::Warning);
        assert!(allowed.detail.contains("unverified"));
    }

    #[test]
    fn only_an_approved_configuration_is_reported_as_trusted() {
        assert_eq!(trust_check(TrustState::Trusted).status, Status::Ok);
        assert_eq!(trust_check(TrustState::NotTrusted).status, Status::Warning);
        let changed = trust_check(TrustState::ConfigurationChanged);
        assert_eq!(changed.status, Status::Warning);
        assert!(changed.detail.contains("changed since it was approved"));
    }

    #[test]
    fn unchecked_is_a_distinct_status() {
        let json = serde_json::to_string(&check("runners", Status::NotChecked, "later")).unwrap();
        assert!(json.contains("\"status\":\"not_checked\""), "{json}");
    }
}
