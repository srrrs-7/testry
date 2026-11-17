//! Suite definitions (spec sections 9 and 15).

use std::collections::BTreeSet;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::ids::{RunnerProject, SuiteId};
use crate::paths::{RepoDir, RepoPath};
use crate::pattern::PatternSet;
use crate::resources::ResourceRequest;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerKind {
    Bun,
    Vitest,
    StorybookVitest,
    Playwright,
    Command,
}

impl RunnerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bun => "bun",
            Self::Vitest => "vitest",
            Self::StorybookVitest => "storybook_vitest",
            Self::Playwright => "playwright",
            Self::Command => "command",
        }
    }

    /// File-level selection is only offered for Bun and Vitest in v0.1.
    /// Storybook and Playwright always run as a whole suite.
    pub fn supports_file_graph(self) -> bool {
        matches!(self, Self::Bun | Self::Vitest)
    }

    /// The `command` adapter only knows success or failure of a command. It is
    /// a gate, not a test suite.
    pub fn is_test_runner(self) -> bool {
        !matches!(self, Self::Command)
    }

    /// Runners whose inner worker count testry passes on the command line.
    pub fn has_worker_control(self) -> bool {
        matches!(
            self,
            Self::Vitest | Self::StorybookVitest | Self::Playwright
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionStrategy {
    /// Narrow to test files through the dependency graph.
    FileGraph,
    /// Run the whole suite whenever there is a relevant change.
    WholeSuite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipStatus {
    Skipped,
    Todo,
    /// A test declared as failing that failed as declared.
    ExpectedFailure,
}

/// One approved, exactly identified skipped test. There are no wildcards.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct KnownSkip {
    pub project: Option<RunnerProject>,
    pub file: RepoPath,
    pub title_path: Vec<String>,
    pub expected_status: SkipStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SuiteError {
    #[error("command must contain at least a program")]
    EmptyCommand,
    #[error("command argument {index} is empty or contains a NUL byte")]
    InvalidCommandArgument { index: usize },
    #[error(
        "command starts with `{launcher}`, which may download packages implicitly; \
         call the locally installed tool instead"
    )]
    ImplicitDownloadLauncher { launcher: String },
    #[error("selection = \"file_graph\" is only supported for bun and vitest, not {runner}")]
    FileGraphUnsupported { runner: &'static str },
    #[error("runner {runner} needs at least one `include` pattern")]
    MissingInclude { runner: &'static str },
    #[error(
        "runner {runner} needs `project`: the suite is exactly one Vitest project, and without \
         a name every project of the configuration would run"
    )]
    ProjectRequired { runner: &'static str },
    #[error("timeout must be greater than zero")]
    ZeroTimeout,
    #[error("runner {runner} needs an explicit `workers` value so the CPU reservation is honest")]
    WorkersRequired { runner: &'static str },
    #[error("runner {runner} has no worker control; remove `workers`")]
    WorkersUnsupported { runner: &'static str },
    #[error("workers must be at least 1")]
    ZeroWorkers,
    #[error("workers = {workers} exceeds the reserved cpu_slots = {cpu_slots}")]
    WorkersExceedCpuSlots { workers: u32, cpu_slots: u32 },
    #[error("known_skips are not supported for runner {runner}: it has no stable case identity")]
    KnownSkipsUnsupported { runner: &'static str },
    #[error("known_skips[{index}] needs a non-empty title_path without empty titles")]
    InvalidKnownSkip { index: usize },
    #[error("known_skips[{index}] duplicates an earlier entry")]
    DuplicateKnownSkip { index: usize },
}

/// Unvalidated input for [`SuiteDefinition::try_new`].
#[derive(Debug, Clone)]
pub struct SuiteDefinitionParts {
    pub id: SuiteId,
    pub runner: RunnerKind,
    pub cwd: RepoDir,
    pub command: Vec<String>,
    pub project: Option<RunnerProject>,
    pub include: PatternSet,
    pub inputs: PatternSet,
    pub selection: SelectionStrategy,
    pub always_run: bool,
    pub workers: Option<u32>,
    pub timeout: Duration,
    pub resources: ResourceRequest,
    pub known_skips: Vec<KnownSkip>,
}

/// A validated suite. Fields are private; use the accessors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuiteDefinition {
    id: SuiteId,
    runner: RunnerKind,
    cwd: RepoDir,
    command: Vec<String>,
    project: Option<RunnerProject>,
    include: PatternSet,
    inputs: PatternSet,
    selection: SelectionStrategy,
    always_run: bool,
    workers: Option<u32>,
    timeout: Duration,
    resources: ResourceRequest,
    known_skips: Vec<KnownSkip>,
}

impl SuiteDefinition {
    pub fn try_new(parts: SuiteDefinitionParts) -> Result<Self, SuiteError> {
        let runner = parts.runner;
        validate_command(&parts.command)?;
        if parts.selection == SelectionStrategy::FileGraph && !runner.supports_file_graph() {
            return Err(SuiteError::FileGraphUnsupported {
                runner: runner.as_str(),
            });
        }
        if runner.is_test_runner() && parts.include.is_empty() {
            return Err(SuiteError::MissingInclude {
                runner: runner.as_str(),
            });
        }
        if runner == RunnerKind::StorybookVitest && parts.project.is_none() {
            return Err(SuiteError::ProjectRequired {
                runner: runner.as_str(),
            });
        }
        if parts.timeout.is_zero() {
            return Err(SuiteError::ZeroTimeout);
        }
        validate_workers(runner, parts.workers, parts.resources.cpu_slots())?;
        validate_known_skips(runner, &parts.known_skips)?;
        Ok(Self {
            id: parts.id,
            runner,
            cwd: parts.cwd,
            command: parts.command,
            project: parts.project,
            include: parts.include,
            inputs: parts.inputs,
            selection: parts.selection,
            always_run: parts.always_run,
            workers: parts.workers,
            timeout: parts.timeout,
            resources: parts.resources,
            known_skips: parts.known_skips,
        })
    }

    pub fn id(&self) -> &SuiteId {
        &self.id
    }

    pub fn runner(&self) -> RunnerKind {
        self.runner
    }

    pub fn cwd(&self) -> &RepoDir {
        &self.cwd
    }

    /// Fixed argv. Never joined into a shell string.
    pub fn command(&self) -> &[String] {
        &self.command
    }

    pub fn project(&self) -> Option<&RunnerProject> {
        self.project.as_ref()
    }

    /// The test file area of this suite.
    pub fn include(&self) -> &PatternSet {
        &self.include
    }

    /// Declared inputs beyond the import graph. They can only widen a
    /// selection, never justify omitting a suite.
    pub fn inputs(&self) -> &PatternSet {
        &self.inputs
    }

    pub fn selection(&self) -> SelectionStrategy {
        self.selection
    }

    pub fn always_run(&self) -> bool {
        self.always_run
    }

    pub fn workers(&self) -> Option<u32> {
        self.workers
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn resources(&self) -> &ResourceRequest {
        &self.resources
    }

    pub fn known_skips(&self) -> &[KnownSkip] {
        &self.known_skips
    }
}

fn validate_command(command: &[String]) -> Result<(), SuiteError> {
    if command.is_empty() {
        return Err(SuiteError::EmptyCommand);
    }
    for (index, argument) in command.iter().enumerate() {
        if argument.is_empty() || argument.contains('\0') {
            return Err(SuiteError::InvalidCommandArgument { index });
        }
    }
    if let Some(launcher) = implicit_download_launcher(command) {
        return Err(SuiteError::ImplicitDownloadLauncher { launcher });
    }
    Ok(())
}

/// Launchers that fetch a package when it is not installed. They would make a
/// run depend on the network and on whatever the registry serves that day.
fn implicit_download_launcher(command: &[String]) -> Option<String> {
    let program = command.first()?;
    let program_name = program.rsplit('/').next().unwrap_or(program);
    if matches!(program_name, "npx" | "bunx" | "pnpx") {
        return Some(program_name.to_owned());
    }
    let subcommand = command.get(1).map(String::as_str)?;
    let is_launcher = matches!(
        (program_name, subcommand),
        ("bun", "x") | ("npm", "exec" | "x") | ("pnpm" | "yarn", "dlx")
    );
    is_launcher.then(|| format!("{program_name} {subcommand}"))
}

fn validate_workers(
    runner: RunnerKind,
    workers: Option<u32>,
    cpu_slots: u32,
) -> Result<(), SuiteError> {
    match (runner.has_worker_control(), workers) {
        (true, None) => Err(SuiteError::WorkersRequired {
            runner: runner.as_str(),
        }),
        (false, Some(_)) => Err(SuiteError::WorkersUnsupported {
            runner: runner.as_str(),
        }),
        (true, Some(0)) => Err(SuiteError::ZeroWorkers),
        (true, Some(workers)) if workers > cpu_slots => {
            Err(SuiteError::WorkersExceedCpuSlots { workers, cpu_slots })
        }
        _ => Ok(()),
    }
}

fn validate_known_skips(runner: RunnerKind, known_skips: &[KnownSkip]) -> Result<(), SuiteError> {
    if !known_skips.is_empty() && !runner.is_test_runner() {
        return Err(SuiteError::KnownSkipsUnsupported {
            runner: runner.as_str(),
        });
    }
    let mut seen = BTreeSet::new();
    for (index, skip) in known_skips.iter().enumerate() {
        if skip.title_path.is_empty() || skip.title_path.iter().any(String::is_empty) {
            return Err(SuiteError::InvalidKnownSkip { index });
        }
        if !seen.insert((&skip.project, &skip.file, &skip.title_path)) {
            return Err(SuiteError::DuplicateKnownSkip { index });
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub(crate) fn parts(id: &str, runner: RunnerKind) -> SuiteDefinitionParts {
        let is_test_runner = runner.is_test_runner();
        SuiteDefinitionParts {
            id: SuiteId::try_new(id).unwrap(),
            runner,
            cwd: RepoDir::root(),
            command: vec!["bun".to_owned(), "test".to_owned()],
            // A stories suite is exactly one Vitest project.
            project: (runner == RunnerKind::StorybookVitest)
                .then(|| crate::ids::RunnerProject::try_new("storybook").unwrap()),
            include: if is_test_runner {
                PatternSet::try_new(["**/*.test.ts"]).unwrap()
            } else {
                PatternSet::empty()
            },
            inputs: PatternSet::empty(),
            selection: if runner.supports_file_graph() {
                SelectionStrategy::FileGraph
            } else {
                SelectionStrategy::WholeSuite
            },
            always_run: false,
            workers: runner.has_worker_control().then_some(1),
            timeout: Duration::from_secs(60),
            resources: ResourceRequest::try_new(1, 512, []).unwrap(),
            known_skips: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::parts;
    use super::*;

    fn command(arguments: &[&str]) -> Vec<String> {
        arguments
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect()
    }

    #[test]
    fn accepts_the_sample_shapes() {
        for runner in [
            RunnerKind::Bun,
            RunnerKind::Vitest,
            RunnerKind::StorybookVitest,
            RunnerKind::Playwright,
            RunnerKind::Command,
        ] {
            assert!(
                SuiteDefinition::try_new(parts("s", runner)).is_ok(),
                "{runner:?}"
            );
        }
    }

    /// Without a project name, every Vitest project of the configuration
    /// would run under the name of the stories suite.
    #[test]
    fn a_stories_suite_needs_its_vitest_project() {
        let mut stories = parts("stories", RunnerKind::StorybookVitest);
        stories.project = None;
        assert_eq!(
            SuiteDefinition::try_new(stories),
            Err(SuiteError::ProjectRequired {
                runner: "storybook_vitest"
            })
        );

        // A project is optional for the runners that work without one.
        for runner in [RunnerKind::Vitest, RunnerKind::Playwright] {
            let mut suite = parts("s", runner);
            suite.project = None;
            assert!(SuiteDefinition::try_new(suite).is_ok(), "{runner:?}");
        }
    }

    #[test]
    fn rejects_empty_or_malformed_commands() {
        let mut empty = parts("s", RunnerKind::Bun);
        empty.command = Vec::new();
        assert_eq!(
            SuiteDefinition::try_new(empty),
            Err(SuiteError::EmptyCommand)
        );

        let mut blank = parts("s", RunnerKind::Bun);
        blank.command = command(&["bun", ""]);
        assert_eq!(
            SuiteDefinition::try_new(blank),
            Err(SuiteError::InvalidCommandArgument { index: 1 })
        );

        let mut nul = parts("s", RunnerKind::Bun);
        nul.command = command(&["bun", "te\0st"]);
        assert_eq!(
            SuiteDefinition::try_new(nul),
            Err(SuiteError::InvalidCommandArgument { index: 1 })
        );
    }

    #[test]
    fn rejects_launchers_that_download_implicitly() {
        for argv in [
            &["npx", "vitest", "run"][..],
            &["bunx", "vitest"],
            &["/usr/local/bin/npx", "playwright", "test"],
            &["bun", "x", "vitest"],
            &["pnpm", "dlx", "vitest"],
            &["yarn", "dlx", "vitest"],
            &["npm", "exec", "vitest"],
        ] {
            let mut suite = parts("s", RunnerKind::Command);
            suite.command = command(argv);
            assert!(
                matches!(
                    SuiteDefinition::try_new(suite),
                    Err(SuiteError::ImplicitDownloadLauncher { .. })
                ),
                "{argv:?} must be rejected"
            );
        }
        let mut allowed = parts("s", RunnerKind::Command);
        allowed.command = command(&["bun", "run", "typecheck"]);
        assert!(SuiteDefinition::try_new(allowed).is_ok());
    }

    #[test]
    fn file_graph_is_limited_to_bun_and_vitest() {
        for runner in [
            RunnerKind::StorybookVitest,
            RunnerKind::Playwright,
            RunnerKind::Command,
        ] {
            let mut suite = parts("s", runner);
            suite.selection = SelectionStrategy::FileGraph;
            assert!(
                matches!(
                    SuiteDefinition::try_new(suite),
                    Err(SuiteError::FileGraphUnsupported { .. })
                ),
                "{runner:?}"
            );
        }
    }

    #[test]
    fn test_runners_need_an_include_area() {
        let mut suite = parts("s", RunnerKind::Vitest);
        suite.include = PatternSet::empty();
        assert!(matches!(
            SuiteDefinition::try_new(suite),
            Err(SuiteError::MissingInclude { .. })
        ));
    }

    #[test]
    fn timeout_must_be_positive() {
        let mut suite = parts("s", RunnerKind::Bun);
        suite.timeout = Duration::ZERO;
        assert_eq!(
            SuiteDefinition::try_new(suite),
            Err(SuiteError::ZeroTimeout)
        );
    }

    /// R03: the inner worker count must fit the outer CPU reservation.
    #[test]
    fn r03_workers_must_match_the_cpu_reservation() {
        let mut missing = parts("s", RunnerKind::Playwright);
        missing.workers = None;
        assert!(matches!(
            SuiteDefinition::try_new(missing),
            Err(SuiteError::WorkersRequired { .. })
        ));

        let mut too_many = parts("s", RunnerKind::Vitest);
        too_many.workers = Some(4);
        assert_eq!(
            SuiteDefinition::try_new(too_many),
            Err(SuiteError::WorkersExceedCpuSlots {
                workers: 4,
                cpu_slots: 1
            })
        );

        let mut zero = parts("s", RunnerKind::Vitest);
        zero.workers = Some(0);
        assert_eq!(SuiteDefinition::try_new(zero), Err(SuiteError::ZeroWorkers));

        let mut uncontrolled = parts("s", RunnerKind::Bun);
        uncontrolled.workers = Some(2);
        assert!(matches!(
            SuiteDefinition::try_new(uncontrolled),
            Err(SuiteError::WorkersUnsupported { .. })
        ));
    }

    #[test]
    fn known_skips_must_be_exact_and_unique() {
        let skip = KnownSkip {
            project: None,
            file: RepoPath::try_new("a.test.ts").unwrap(),
            title_path: vec!["suite".to_owned(), "case".to_owned()],
            expected_status: SkipStatus::Skipped,
        };

        let mut duplicated = parts("s", RunnerKind::Bun);
        duplicated.known_skips = vec![skip.clone(), skip.clone()];
        assert_eq!(
            SuiteDefinition::try_new(duplicated),
            Err(SuiteError::DuplicateKnownSkip { index: 1 })
        );

        let mut empty_title = parts("s", RunnerKind::Bun);
        empty_title.known_skips = vec![KnownSkip {
            title_path: Vec::new(),
            ..skip.clone()
        }];
        assert_eq!(
            SuiteDefinition::try_new(empty_title),
            Err(SuiteError::InvalidKnownSkip { index: 0 })
        );

        let mut command_gate = parts("s", RunnerKind::Command);
        command_gate.known_skips = vec![skip];
        assert!(matches!(
            SuiteDefinition::try_new(command_gate),
            Err(SuiteError::KnownSkipsUnsupported { .. })
        ));
    }
}
