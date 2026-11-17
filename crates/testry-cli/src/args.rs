//! Command line contract (spec section 14).

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use testry_domain::plan::Mode;

#[derive(Debug, Parser)]
#[command(
    name = "testry",
    version,
    about = "Change-aware test orchestration: plan, explain and run the tests a change needs.",
    after_help = "Exit codes: 0 verified or read-only success, 1 test or policy failure, \
                  2 configuration / trust / unsupported capability / missing base, \
                  3 infrastructure error (timeout, broken report, changed inputs, cleanup), \
                  4 nothing to verify (not a pass), 130 cancelled."
)]
pub struct Cli {
    /// Configuration file. The workspace is located relative to it.
    #[arg(long, global = true, value_name = "FILE", default_value = crate::config::DEFAULT_CONFIG_FILE)]
    pub config: PathBuf,

    /// Apply the rules for CI quality gates: only `full` or `shadow`, no
    /// `--suite`, and `shadow` needs an explicit `--base`. Also enabled when
    /// the environment variable CI is `true` or `1`.
    #[arg(long, global = true)]
    pub ci: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Read-only diagnostics. Installs nothing, deletes nothing, switches no
    /// Docker context.
    Doctor(DoctorArgs),

    /// Compute which suites and test files a change needs, and why. Uses Git
    /// and file reads only: no runner configuration is executed, nothing is
    /// installed, no environment is started.
    Plan(PlanArgs),

    /// Explain the selection of one suite of a stored plan.
    Explain(ExplainArgs),

    /// Run tests for the local feedback loop. A narrowed or scope-limited run
    /// is advisory, never a quality gate.
    Test(RunArgs),

    /// Run the quality gate: `full` (default) or `shadow`.
    Verify(RunArgs),

    /// Inspect stored runs.
    Runs {
        #[command(subcommand)]
        command: RunsCommand,
    },

    /// Remove run artifacts and plans that are older than the retention
    /// period, and stop processes that interrupted runs left behind. Only
    /// what testry can prove it owns is ever removed.
    Gc(GcArgs),

    /// Show which commands this workspace would run and whether you approved
    /// them. `test` and `verify` refuse to run commands of a configuration
    /// that was not approved. The approval is stored outside the repository.
    Trust(TrustArgs),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ModeArg {
    /// Narrow through the dependency graph; widen on any uncertainty.
    Affected,
    /// Run every configured suite.
    Full,
    /// Compute the affected plan as a candidate, but execute everything once.
    Shadow,
}

impl From<ModeArg> for Mode {
    fn from(mode: ModeArg) -> Self {
        match mode {
            ModeArg::Affected => Self::Affected,
            ModeArg::Full => Self::Full,
            ModeArg::Shadow => Self::Shadow,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum Format {
    /// Human-readable text.
    #[default]
    Text,
    /// Exactly one JSON object on stdout. Progress goes to stderr.
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum RunFormat {
    #[default]
    Text,
    /// Exactly one JSON result object on stdout.
    Json,
    /// One JSON event per line on stdout.
    Jsonl,
}

#[derive(Debug, Args)]
pub struct SelectionArgs {
    /// Selection mode. Defaults to policy.local_default_mode, or to
    /// policy.ci_default_mode in CI (`verify` defaults to full).
    #[arg(long, value_enum)]
    pub mode: Option<ModeArg>,

    /// Git ref to compare with. The default is HEAD, which selects by your
    /// UNCOMMITTED changes (staged, unstaged and untracked) only. Pass e.g.
    /// `--base origin/main` to cover the whole branch; the merge base with
    /// HEAD is used. Nothing is fetched. Ignored in full mode.
    #[arg(long, value_name = "REF")]
    pub base: Option<String>,

    /// Limit the scope to this suite (repeatable). Local use only; the result
    /// is advisory and CI rejects it.
    #[arg(long = "suite", value_name = "SUITE_ID")]
    pub suites: Vec<String>,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    #[arg(long, value_enum, default_value_t)]
    pub format: Format,
}

#[derive(Debug, Args)]
pub struct PlanArgs {
    #[command(flatten)]
    pub selection: SelectionArgs,

    #[arg(long, value_enum, default_value_t)]
    pub format: Format,
}

#[derive(Debug, Args)]
pub struct ExplainArgs {
    /// A plan written by `testry plan` (see its output for the path).
    #[arg(long, value_name = "FILE")]
    pub plan: PathBuf,

    /// The suite to explain.
    #[arg(long, value_name = "SUITE_ID")]
    pub suite: String,

    #[arg(long, value_enum, default_value_t)]
    pub format: Format,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    #[command(flatten)]
    pub selection: SelectionArgs,

    /// Execution environment defined under [environments]. Defaults to
    /// execution.default_environment.
    #[arg(long, value_name = "NAME")]
    pub environment: Option<String>,

    #[arg(long, value_enum, default_value_t)]
    pub format: RunFormat,
}

#[derive(Debug, Subcommand)]
pub enum RunsCommand {
    /// Show the stored result of a run.
    Show(RunsShowArgs),

    /// List the runs in the history of this workspace, newest first.
    List(RunsListArgs),
}

#[derive(Debug, Args)]
pub struct RunsListArgs {
    /// The largest number of runs to list.
    #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..=1000))]
    pub limit: u32,

    #[arg(long, value_enum, default_value_t)]
    pub format: Format,
}

#[derive(Debug, Args)]
pub struct RunsShowArgs {
    pub run_id: String,

    #[arg(long, value_enum, default_value_t)]
    pub format: Format,
}

#[derive(Debug, Args)]
pub struct GcArgs {
    /// List what would be removed without removing anything. Exactly one of
    /// `--dry-run` and `--apply` is required.
    #[arg(long, conflicts_with = "apply", required_unless_present = "apply")]
    pub dry_run: bool,

    /// Remove what is older than the retention period and provably owned by
    /// testry, and stop processes that interrupted runs left behind.
    #[arg(long)]
    pub apply: bool,

    #[arg(long, value_enum, default_value_t)]
    pub format: Format,
}

#[derive(Debug, Args)]
pub struct TrustArgs {
    /// Approve the current configuration of this workspace.
    #[arg(long, conflicts_with = "revoke")]
    pub approve: bool,

    /// Remove the approval of this workspace.
    #[arg(long)]
    pub revoke: bool,

    #[arg(long, value_enum, default_value_t)]
    pub format: Format,
}

impl Command {
    /// Whether stdout must carry exactly one JSON object.
    pub fn wants_json(&self) -> bool {
        match self {
            Self::Doctor(args) => args.format == Format::Json,
            Self::Plan(args) => args.format == Format::Json,
            Self::Explain(args) => args.format == Format::Json,
            Self::Test(args) | Self::Verify(args) => args.format == RunFormat::Json,
            Self::Runs {
                command: RunsCommand::Show(args),
            } => args.format == Format::Json,
            Self::Runs {
                command: RunsCommand::List(args),
            } => args.format == Format::Json,
            Self::Gc(args) => args.format == Format::Json,
            Self::Trust(args) => args.format == Format::Json,
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    fn parse(arguments: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("testry").chain(arguments.iter().copied()))
    }

    #[test]
    fn clap_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn the_documented_commands_parse() {
        for arguments in [
            &["doctor", "--format", "json"][..],
            &[
                "plan",
                "--mode",
                "affected",
                "--base",
                "origin/main",
                "--format",
                "json",
            ],
            &[
                "explain",
                "--plan",
                ".testry/plans/x.json",
                "--suite",
                "api-unit",
            ],
            &[
                "test", "--mode", "affected", "--base", "HEAD", "--suite", "api-unit",
            ],
            &["verify", "--mode", "full"],
            &["verify", "--mode", "shadow", "--base", "origin/main"],
            &["verify", "--mode", "full", "--environment", "colima"],
            &[
                "runs",
                "show",
                "123e4567-e89b-42d3-a456-426614174000",
                "--format",
                "json",
            ],
            &["gc", "--dry-run"],
            &["gc", "--apply"],
            &["trust"],
            &["trust", "--approve"],
            &["trust", "--revoke", "--format", "json"],
            &["--config", "ci/testry.toml", "--ci", "verify"],
            &["verify", "--ci", "--config", "ci/testry.toml"],
        ] {
            assert!(parse(arguments).is_ok(), "{arguments:?}");
        }
    }

    #[test]
    fn defaults() {
        let cli = parse(&["plan"]).unwrap();
        assert_eq!(cli.config, PathBuf::from("testry.toml"));
        assert!(!cli.ci);
        let Command::Plan(plan) = cli.command else {
            panic!("expected plan")
        };
        assert_eq!(plan.selection.mode, None);
        assert_eq!(plan.selection.base, None);
        assert!(plan.selection.suites.is_empty());
        assert_eq!(plan.format, Format::Text);
    }

    #[test]
    fn suite_is_repeatable() {
        let cli = parse(&["test", "--suite", "a", "--suite", "b"]).unwrap();
        let Command::Test(run) = cli.command else {
            panic!("expected test")
        };
        assert_eq!(run.selection.suites, ["a", "b"]);
    }

    #[test]
    fn invalid_usage_is_rejected_with_exit_code_two() {
        for arguments in [
            &["plan", "--mode", "safe"][..],
            &["gc"],
            &["gc", "--dry-run", "--apply"],
            &["trust", "--approve", "--revoke"],
            &["explain", "--suite", "api-unit"],
            &["runs", "show"],
            &["unknown-command"],
            &["plan", "--format", "jsonl"],
            &["plan", "--unknown-flag"],
        ] {
            let error = parse(arguments).unwrap_err();
            assert_eq!(error.exit_code(), 2, "{arguments:?}");
        }
    }

    #[test]
    fn help_states_that_the_default_base_is_uncommitted_changes() {
        let mut command = Cli::command();
        let plan = command.find_subcommand_mut("plan").unwrap();
        let help = plan.render_long_help().to_string();
        assert!(help.contains("UNCOMMITTED"), "{help}");
        assert!(help.contains("origin/main"), "{help}");
    }

    #[test]
    fn json_format_is_detected_per_command() {
        assert!(
            parse(&["plan", "--format", "json"])
                .unwrap()
                .command
                .wants_json()
        );
        assert!(!parse(&["plan"]).unwrap().command.wants_json());
        assert!(
            parse(&["verify", "--format", "json"])
                .unwrap()
                .command
                .wants_json()
        );
        assert!(
            !parse(&["verify", "--format", "jsonl"])
                .unwrap()
                .command
                .wants_json()
        );
    }
}
