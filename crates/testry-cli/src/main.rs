//! `testry` command line: argument parsing, configuration loading, dependency
//! wiring, output and exit codes.
#![forbid(unsafe_code)]

mod args;
mod commands;
mod composition_root;
mod config;
mod error;

use std::process::ExitCode;

use clap::Parser;

use crate::args::Cli;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let wants_json = cli.command.wants_json();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("error: the async runtime could not be started: {error}");
            return ExitCode::from(
                testry_domain::results::ExitCondition::Infrastructure.exit_code(),
            );
        }
    };

    match runtime.block_on(commands::run(&cli)) {
        Ok(condition) => ExitCode::from(condition.exit_code()),
        Err(error) => {
            eprintln!("error: {}", error.message);
            if wants_json {
                // `--format json` promises exactly one JSON object on stdout.
                println!("{}", error.to_json());
            }
            ExitCode::from(error.exit_code())
        }
    }
}
