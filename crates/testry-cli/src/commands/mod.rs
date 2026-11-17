//! Command dispatch.

mod doctor;
mod explain;
mod gc;
mod plan;
mod render;
mod run;
mod runs;
mod trust;

use std::collections::BTreeSet;

use testry_application::commands::plan_workspace::{PlanPurpose, PlanRequest};
use testry_application::ports::PortError;
use testry_application::settings::Settings;
use testry_domain::ids::SuiteId;
use testry_domain::plan::Mode;
use testry_domain::results::ExitCondition;

use crate::args::{Cli, Command, RunsCommand, SelectionArgs};
use crate::error::CliError;

pub async fn run(cli: &Cli) -> Result<ExitCondition, CliError> {
    let ci = cli.ci || ci_from_environment(std::env::var("CI").ok().as_deref());
    match &cli.command {
        Command::Doctor(args) => doctor::run(cli, args).await,
        Command::Plan(args) => plan::run(cli, args, ci).await,
        Command::Explain(args) => explain::run(args),
        Command::Test(args) => run::run(cli, args, PlanPurpose::Test, ci).await,
        Command::Verify(args) => run::run(cli, args, PlanPurpose::Verify, ci).await,
        Command::Runs {
            command: RunsCommand::Show(args),
        } => runs::show(cli, args),
        Command::Runs {
            command: RunsCommand::List(args),
        } => runs::list(cli, args),
        Command::Trust(args) => trust::run(cli, args),
        Command::Gc(args) => gc::run(cli, args).await,
    }
}

/// CI providers set `CI=true`. An explicit `--ci` flag works everywhere.
fn ci_from_environment(value: Option<&str>) -> bool {
    value.is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
}

/// The mode of a request: the explicit `--mode`, otherwise the default of the
/// command and of the environment it runs in.
fn resolve_mode(
    selection: &SelectionArgs,
    purpose: PlanPurpose,
    settings: &Settings,
    ci: bool,
) -> Mode {
    if let Some(mode) = selection.mode {
        return mode.into();
    }
    match purpose {
        // The quality gate always defaults to a mode that can gate.
        PlanPurpose::Verify if !ci => Mode::Full,
        _ if ci => settings.ci_default_mode(),
        _ => settings.local_default_mode(),
    }
}

fn plan_request(
    selection: &SelectionArgs,
    purpose: PlanPurpose,
    settings: &Settings,
    ci: bool,
) -> Result<PlanRequest, CliError> {
    let scope = if selection.suites.is_empty() {
        None
    } else {
        let mut scope = BTreeSet::new();
        for suite in &selection.suites {
            let id = SuiteId::try_new(suite.clone())
                .map_err(|error| CliError::configuration(format!("--suite: {error}")))?;
            scope.insert(id);
        }
        Some(scope)
    };
    let request = PlanRequest {
        purpose,
        mode: resolve_mode(selection, purpose, settings, ci),
        base_ref: selection.base.clone(),
        scope,
        // `plan` only inspects, so the gate rules of CI do not reject it. CI
        // still decides the default mode.
        ci: ci && purpose != PlanPurpose::Inspect,
    };
    request.validate(settings).map_err(PortError::from)?;
    Ok(request)
}

#[cfg(test)]
mod tests {
    use crate::config;
    use testry_domain::paths::RepoPath;

    use super::*;
    use crate::args::ModeArg;

    fn settings() -> Settings {
        let sample = include_str!("../../tests/fixtures/sample.testry.toml");
        config::parse(sample, RepoPath::try_new("testry.toml").unwrap()).unwrap()
    }

    fn selection(mode: Option<ModeArg>, base: Option<&str>, suites: &[&str]) -> SelectionArgs {
        SelectionArgs {
            mode,
            base: base.map(str::to_owned),
            suites: suites.iter().map(|suite| (*suite).to_owned()).collect(),
        }
    }

    #[test]
    fn ci_is_detected_from_the_conventional_variable() {
        assert!(ci_from_environment(Some("true")));
        assert!(ci_from_environment(Some("TRUE")));
        assert!(ci_from_environment(Some("1")));
        assert!(!ci_from_environment(Some("false")));
        assert!(!ci_from_environment(Some("")));
        assert!(!ci_from_environment(None));
    }

    #[test]
    fn default_modes_follow_command_and_environment() {
        let settings = settings();
        let none = selection(None, None, &[]);
        assert_eq!(
            resolve_mode(&none, PlanPurpose::Test, &settings, false),
            Mode::Affected
        );
        assert_eq!(
            resolve_mode(&none, PlanPurpose::Inspect, &settings, false),
            Mode::Affected
        );
        assert_eq!(
            resolve_mode(&none, PlanPurpose::Verify, &settings, false),
            Mode::Full
        );
        for purpose in [PlanPurpose::Test, PlanPurpose::Inspect, PlanPurpose::Verify] {
            assert_eq!(resolve_mode(&none, purpose, &settings, true), Mode::Full);
        }
        let explicit = selection(Some(ModeArg::Shadow), None, &[]);
        assert_eq!(
            resolve_mode(&explicit, PlanPurpose::Test, &settings, true),
            Mode::Shadow
        );
    }

    /// P07: CI rejects affected and scope limits for the commands that gate.
    #[test]
    fn p07_gate_rules_apply_to_test_and_verify_but_not_to_plan() {
        let settings = settings();
        let affected = selection(Some(ModeArg::Affected), None, &[]);
        let scoped = selection(Some(ModeArg::Full), None, &["api-unit"]);
        let shadow = selection(Some(ModeArg::Shadow), None, &[]);

        for purpose in [PlanPurpose::Test, PlanPurpose::Verify] {
            for rejected in [&affected, &scoped, &shadow] {
                let error = plan_request(rejected, purpose, &settings, true).unwrap_err();
                assert_eq!(error.exit_code(), 2, "{purpose:?} {rejected:?}");
                assert_eq!(error.code, "planning");
            }
        }
        for inspected in [&affected, &scoped, &shadow] {
            let request = plan_request(inspected, PlanPurpose::Inspect, &settings, true).unwrap();
            assert!(!request.ci);
        }

        // Locally a scope limit is allowed; `verify` still refuses affected.
        assert!(plan_request(&scoped, PlanPurpose::Verify, &settings, false).is_ok());
        assert!(plan_request(&affected, PlanPurpose::Test, &settings, false).is_ok());
        assert!(plan_request(&affected, PlanPurpose::Verify, &settings, false).is_err());

        let with_base = selection(Some(ModeArg::Shadow), Some("origin/main"), &[]);
        assert!(plan_request(&with_base, PlanPurpose::Verify, &settings, true).is_ok());
    }

    #[test]
    fn unknown_or_malformed_suites_are_rejected() {
        let settings = settings();
        let unknown = selection(None, None, &["nope"]);
        assert_eq!(
            plan_request(&unknown, PlanPurpose::Test, &settings, false)
                .unwrap_err()
                .exit_code(),
            2
        );
        let malformed = selection(None, None, &["../x"]);
        let error = plan_request(&malformed, PlanPurpose::Test, &settings, false).unwrap_err();
        assert_eq!(error.code, "configuration");
    }
}
