//! Runner adapters (spec section 9).
//!
//! An adapter builds the argv of one runner invocation and normalizes its
//! structured report. Arguments that decide reporting, workers or selection
//! belong to the adapter: a suite command that sets them itself, or that
//! narrows the run in another way, is a configuration error.

pub mod bun;
pub mod command;
pub mod playwright;
pub mod storybook;
pub mod vitest;

use std::collections::BTreeMap;

#[cfg(test)]
use testry_application::ports::execution::ToolVersion;
use testry_application::ports::execution::{AdapterError, RunnerAdapter};
use testry_domain::paths::{RepoDir, RepoPath};
use testry_domain::suite::RunnerKind;

use self::bun::BunAdapter;
use self::command::CommandAdapter;
use self::playwright::PlaywrightAdapter;
use self::storybook::StorybookAdapter;
use self::vitest::VitestAdapter;

/// The adapters that exist. A suite whose runner has no adapter is refused
/// instead of being run by another adapter.
pub struct RunnerRegistry {
    bun: BunAdapter,
    vitest: VitestAdapter,
    storybook: StorybookAdapter,
    playwright: PlaywrightAdapter,
    command: CommandAdapter,
}

impl RunnerRegistry {
    pub fn new() -> Self {
        Self {
            bun: BunAdapter,
            vitest: VitestAdapter,
            storybook: StorybookAdapter,
            playwright: PlaywrightAdapter,
            command: CommandAdapter,
        }
    }

    pub fn adapters(&self) -> BTreeMap<RunnerKind, &dyn RunnerAdapter> {
        BTreeMap::from([
            (RunnerKind::Bun, &self.bun as &dyn RunnerAdapter),
            (RunnerKind::Vitest, &self.vitest as &dyn RunnerAdapter),
            (
                RunnerKind::StorybookVitest,
                &self.storybook as &dyn RunnerAdapter,
            ),
            (
                RunnerKind::Playwright,
                &self.playwright as &dyn RunnerAdapter,
            ),
            (RunnerKind::Command, &self.command as &dyn RunnerAdapter),
        ])
    }
}

impl Default for RunnerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// A command line flag and why a suite command must not contain it.
pub(crate) struct Reserved {
    pub(crate) flags: &'static [&'static str],
    pub(crate) reason: &'static str,
}

/// Rejects a suite command that contains a reserved flag, as `--flag`,
/// `--flag=value`, a short flag or a short flag with an attached value.
pub(crate) fn reject_reserved(
    runner: &str,
    command: &[String],
    reserved: &[Reserved],
) -> Result<(), AdapterError> {
    for argument in command.iter().skip(1) {
        let name = argument
            .split_once('=')
            .map_or(argument.as_str(), |(name, _)| name);
        for entry in reserved {
            // `-j4` is `-j` with an attached value.
            let attached_short = !argument.starts_with("--")
                && entry.flags.iter().any(|flag| {
                    flag.len() == 2 && !flag.starts_with("--") && argument.starts_with(flag)
                });
            if entry.flags.contains(&name) || attached_short {
                return Err(AdapterError::Configuration {
                    message: format!(
                        "the {runner} suite command contains `{argument}`: {}",
                        entry.reason
                    ),
                });
            }
        }
    }
    Ok(())
}

/// The path of `file` relative to the suite directory, or `None` when the
/// file is not below it.
pub(crate) fn relative_to(cwd: &RepoDir, file: &RepoPath) -> Option<String> {
    match cwd.as_path() {
        None => Some(file.as_str().to_owned()),
        Some(directory) => file
            .as_str()
            .strip_prefix(directory.as_str())
            .and_then(|rest| rest.strip_prefix('/'))
            .map(str::to_owned),
    }
}

/// The first `major.minor.patch` version in `text`.
pub(crate) fn find_version(text: &str) -> Option<String> {
    text.split(|character: char| {
        !(character.is_ascii_alphanumeric() || character == '.' || character == '-')
    })
    .find_map(|token| {
        let token = token.trim_start_matches('v');
        let mut parts = token.splitn(3, '.');
        let numeric = |part: Option<&str>| {
            part.is_some_and(|part| {
                !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())
            })
        };
        let major = parts.next();
        let minor = parts.next();
        let patch = parts.next()?;
        let patch_starts_numeric = patch
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_digit());
        (numeric(major) && numeric(minor) && patch_starts_numeric).then(|| token.to_owned())
    })
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::BTreeSet;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    use testry_application::ports::execution::{
        ArtifactLocations, ArtifactRef, ExecutionDescriptor, ProcessNotes, ProcessResult,
        ReportBundle, ReportContent, ReportFormat, WorkspaceMapping,
    };
    use testry_application::settings::test_support::suite_parts;
    use testry_domain::environment::ProviderKind;
    use testry_domain::paths::RepoDir;
    use testry_domain::results::ProcessExit;
    use testry_domain::selection::Selection;
    use testry_domain::suite::{RunnerKind, SuiteDefinition};

    use super::*;

    pub(crate) fn path(raw: &str) -> RepoPath {
        RepoPath::try_new(raw).unwrap()
    }

    pub(crate) fn files(names: &[&str]) -> Selection {
        Selection::files(names.iter().map(|name| path(name))).unwrap()
    }

    pub(crate) fn suite(runner: RunnerKind, cwd: &str, command: &[&str]) -> SuiteDefinition {
        let mut parts = suite_parts("suite", runner, &["**/*.test.ts"]);
        parts.cwd = RepoDir::try_new(cwd).unwrap();
        parts.command = command
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect();
        SuiteDefinition::try_new(parts).unwrap()
    }

    pub(crate) fn descriptor() -> ExecutionDescriptor {
        ExecutionDescriptor {
            provider: ProviderKind::Host,
            os: "linux".to_owned(),
            arch: "aarch64".to_owned(),
            tool_versions: BTreeMap::new(),
            workspace: WorkspaceMapping {
                local_root: PathBuf::from("/workspace"),
                execution_root: PathBuf::from("/workspace"),
            },
            image: None,
            capabilities: BTreeSet::new(),
        }
    }

    pub(crate) fn locations(report: &str) -> ArtifactLocations {
        ArtifactLocations {
            task_directory: PathBuf::from("/workspace/.testry/runs/r/tasks/suite"),
            report_path: PathBuf::from(format!("/workspace/.testry/runs/r/tasks/suite/{report}")),
        }
    }

    pub(crate) fn process(code: i32) -> ProcessResult {
        ProcessResult {
            exit: ProcessExit::Exited { code },
            started_at: SystemTime::UNIX_EPOCH,
            finished_at: SystemTime::UNIX_EPOCH,
            duration: Duration::from_millis(10),
            stdout: ArtifactRef {
                relative_path: PathBuf::from("tasks/suite/stdout.log"),
            },
            stderr: ArtifactRef {
                relative_path: PathBuf::from("tasks/suite/stderr.log"),
            },
            notes: ProcessNotes::default(),
        }
    }

    pub(crate) fn bundle(format: ReportFormat, bytes: &[u8]) -> ReportBundle {
        ReportBundle {
            format,
            content: ReportContent::Bytes(bytes.to_vec()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::path;
    use super::*;

    #[test]
    fn versions_are_found_in_runner_output() {
        assert_eq!(find_version("1.4.4\n").as_deref(), Some("1.4.4"));
        assert_eq!(
            find_version("vitest/5.1.0 linux-arm64 node-v26.13.1").as_deref(),
            Some("5.1.0")
        );
        assert_eq!(find_version("Version 1.64.1").as_deref(), Some("1.64.1"));
        assert_eq!(
            find_version("1.5.0-canary.3").as_deref(),
            Some("1.5.0-canary.3")
        );
        assert_eq!(find_version("no version here"), None);
        assert_eq!(find_version("1.4"), None);
        assert_eq!(find_version(""), None);
    }

    #[test]
    fn files_are_addressed_relative_to_the_suite_directory() {
        let api = RepoDir::try_new("apps/api").unwrap();
        assert_eq!(
            relative_to(&api, &path("apps/api/src/a.test.ts")).as_deref(),
            Some("src/a.test.ts")
        );
        assert_eq!(relative_to(&api, &path("apps/api-v2/a.test.ts")), None);
        assert_eq!(relative_to(&api, &path("apps/web/a.test.ts")), None);
        assert_eq!(
            relative_to(&RepoDir::root(), &path("a.test.ts")).as_deref(),
            Some("a.test.ts")
        );
    }

    #[test]
    fn reserved_flags_are_found_in_every_spelling() {
        let reserved = [Reserved {
            flags: &["--reporter", "-t"],
            reason: "owned",
        }];
        let command = |arguments: &[&str]| -> Vec<String> {
            arguments
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect()
        };
        assert!(reject_reserved("bun", &command(&["bun", "test"]), &reserved).is_ok());
        assert!(
            reject_reserved(
                "bun",
                &command(&["bun", "test", "--reporter=junit"]),
                &reserved
            )
            .is_err()
        );
        assert!(
            reject_reserved(
                "bun",
                &command(&["bun", "test", "--reporter", "junit"]),
                &reserved
            )
            .is_err()
        );
        assert!(
            reject_reserved("bun", &command(&["bun", "test", "-t", "name"]), &reserved).is_err()
        );
        // A short flag with an attached value is the same flag.
        assert!(reject_reserved("bun", &command(&["bun", "test", "-tname"]), &reserved).is_err());
        // A long flag that merely starts with the same letters is another flag.
        assert!(
            reject_reserved(
                "bun",
                &command(&["bun", "test", "--timeout=5000"]),
                &reserved
            )
            .is_ok()
        );
        // The program itself is never mistaken for a flag.
        assert!(reject_reserved("x", &command(&["-t"]), &reserved).is_ok());
    }

    #[test]
    fn every_runner_kind_has_its_own_adapter() {
        let registry = RunnerRegistry::new();
        let adapters = registry.adapters();
        for kind in [
            RunnerKind::Bun,
            RunnerKind::Vitest,
            RunnerKind::StorybookVitest,
            RunnerKind::Playwright,
            RunnerKind::Command,
        ] {
            let adapter = adapters
                .get(&kind)
                .unwrap_or_else(|| panic!("{} has no adapter", kind.as_str()));
            assert_eq!(
                adapter.kind(),
                kind,
                "a suite is never run by another adapter"
            );
        }
        assert_eq!(adapters.len(), 5);
    }

    /// Only Bun and Vitest select files. Storybook and Playwright run as a
    /// whole whatever version is installed, and a command has no selection.
    #[test]
    fn only_bun_and_vitest_offer_file_selection() {
        let registry = RunnerRegistry::new();
        let adapters = registry.adapters();
        let observed = |tool: &str, version: &str| ToolVersion {
            tool: tool.to_owned(),
            version: version.to_owned(),
        };
        let selects = |kind: RunnerKind, version: &ToolVersion| {
            adapters[&kind].capabilities(Some(version)).file_selection
        };
        assert!(selects(RunnerKind::Bun, &observed("bun", "1.4.4")));
        assert!(selects(RunnerKind::Vitest, &observed("vitest", "5.1.0")));
        assert!(!selects(
            RunnerKind::StorybookVitest,
            &observed("vitest", "5.1.0")
        ));
        assert!(!selects(
            RunnerKind::Playwright,
            &observed("playwright", "1.65.1")
        ));
        assert!(!selects(RunnerKind::Command, &observed("x", "1.0.0")));
        for kind in adapters.keys() {
            assert!(adapters[kind].capabilities(None).full_run);
        }
    }
}
