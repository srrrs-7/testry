//! `testry trust`: review and approve what a workspace would run.
//!
//! `test` and `verify` execute the commands of the configuration. They refuse
//! to do so until exactly that configuration was approved here. The approval
//! lives in the user's state directory, outside the repository.

use std::path::Path;

use serde::Serialize;
use testry_domain::fingerprint::Fingerprint;
use testry_domain::results::ExitCondition;

use crate::args::{Cli, Format, TrustArgs};
use crate::composition_root::{trust_approval, trust_store};
use crate::config;
use crate::error::CliError;

#[derive(Debug, Serialize)]
struct CommandDocument {
    suite_id: String,
    runner: &'static str,
    cwd: String,
    argv: Vec<String>,
}

#[derive(Debug, Serialize)]
struct TrustDocument {
    schema_version: u32,
    workspace: String,
    configuration: String,
    config_digest: String,
    /// `trusted`, `configuration_changed` or `not_trusted`.
    status: &'static str,
    commands: Vec<CommandDocument>,
}

/// Whether `test` and `verify` may execute the commands of a configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustState {
    Trusted,
    /// Another configuration was approved for this workspace.
    ConfigurationChanged,
    NotTrusted,
}

impl TrustState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trusted => "trusted",
            Self::ConfigurationChanged => "configuration_changed",
            Self::NotTrusted => "not_trusted",
        }
    }
}

pub fn trust_state(workspace_root: &Path, digest: &Fingerprint) -> Result<TrustState, CliError> {
    Ok(match trust_approval(workspace_root)? {
        Some(approval) if approval.approved_config_digest == *digest => TrustState::Trusted,
        Some(_) => TrustState::ConfigurationChanged,
        None => TrustState::NotTrusted,
    })
}

pub fn run(cli: &Cli, args: &TrustArgs) -> Result<ExitCondition, CliError> {
    let loaded = config::load(&cli.config)?;
    let settings = &loaded.settings;
    let digest = *settings.config_digest();

    if args.approve {
        trust_store()?.approve(&loaded.workspace_root, &digest)?;
    } else if args.revoke {
        trust_store()?.revoke(&loaded.workspace_root)?;
    }

    let status = trust_state(&loaded.workspace_root, &digest)?.as_str();
    let document = TrustDocument {
        schema_version: 1,
        workspace: loaded.workspace_root.display().to_string(),
        configuration: settings.config_path().to_string(),
        config_digest: digest.to_string(),
        status,
        commands: settings
            .suites()
            .iter()
            .map(|suite| CommandDocument {
                suite_id: suite.id().to_string(),
                runner: suite.runner().as_str(),
                cwd: suite.cwd().to_string(),
                argv: suite.command().to_vec(),
            })
            .collect(),
    };

    match args.format {
        Format::Json => {
            let json = serde_json::to_string_pretty(&document).map_err(|error| {
                CliError::storage(format!("the report could not be serialized: {error}"))
            })?;
            println!("{json}");
        }
        Format::Text => {
            println!("Workspace: {}", document.workspace);
            println!(
                "Configuration: {} ({})",
                document.configuration, document.config_digest
            );
            println!("Commands that `testry test` and `testry verify` run in this workspace:");
            for command in &document.commands {
                println!(
                    "  {} ({}), in {}: {:?}",
                    command.suite_id, command.runner, command.cwd, command.argv
                );
            }
            println!("  (the runner adapters add report, worker and file arguments)");
            match status {
                "trusted" => println!("Status: trusted for this configuration."),
                "configuration_changed" => println!(
                    "Status: NOT trusted. The configuration changed since it was approved. \
                     Review the commands above and run `testry trust --approve`."
                ),
                _ => println!(
                    "Status: NOT trusted. Review the commands above and run `testry trust --approve`."
                ),
            }
        }
    }
    Ok(ExitCondition::Success)
}
