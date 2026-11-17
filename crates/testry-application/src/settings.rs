//! Validated configuration (spec section 15).
//!
//! The CLI parses `testry.toml` into [`SettingsParts`]; [`Settings::try_new`]
//! applies the rules that span several keys. Every problem is collected, so a
//! user sees all of them at once.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use testry_domain::environment::ProviderKind;
use testry_domain::fingerprint::Fingerprint;
use testry_domain::ids::{EnvironmentName, SuiteId};
use testry_domain::paths::RepoPath;
use testry_domain::pattern::{PatternError, PatternSet};
use testry_domain::plan::Mode;
use testry_domain::resources::{ResourceBudget, ResourceError};
use testry_domain::suite::SuiteDefinition;

use crate::ports::planning::ResolverProfile;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevcontainerSettings {
    /// The Docker context to use. It is passed to child processes only; the
    /// user's global context is never switched.
    pub docker_context: String,
    pub config: RepoPath,
    /// `initializeCommand` runs outside the container and is refused unless
    /// this is set and the workspace is trusted.
    pub allow_initialize_command: bool,
    pub readiness_commands: Vec<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvironmentSettings {
    /// Child processes of testry itself.
    Host,
    /// A disposable Dev Container owned by the run.
    DevcontainerCli(DevcontainerSettings),
}

impl EnvironmentSettings {
    pub fn provider(&self) -> ProviderKind {
        match self {
            Self::Host => ProviderKind::Host,
            Self::DevcontainerCli(_) => ProviderKind::DevcontainerCli,
        }
    }
}

/// Unvalidated input for [`Settings::try_new`].
#[derive(Debug, Clone)]
pub struct SettingsParts {
    /// Path of the configuration file inside the workspace.
    pub config_path: RepoPath,
    /// Digest of the configuration file bytes.
    pub config_digest: Fingerprint,
    pub exclude: Vec<String>,
    pub non_test_inputs: Vec<String>,
    pub global_inputs: Vec<String>,
    pub budget: ResourceBudget,
    pub default_environment: EnvironmentName,
    pub environments: BTreeMap<EnvironmentName, EnvironmentSettings>,
    pub default_timeout: Duration,
    pub termination_grace: Duration,
    pub cleanup_timeout: Duration,
    pub fail_fast: bool,
    pub max_log_bytes_per_stream: u64,
    pub local_default_mode: Mode,
    pub ci_default_mode: Mode,
    pub fail_on_unexpected_skips: bool,
    pub fail_on_flaky: bool,
    pub storage_directory: RepoPath,
    pub history_retention_days: u32,
    pub suites: Vec<SuiteDefinition>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SettingsIssue {
    #[error(
        "at least one test suite is required; `command` suites are gates and do not count as \
         test verification"
    )]
    NoTestSuite,
    #[error("suite id `{0}` is defined more than once")]
    DuplicateSuite(SuiteId),
    #[error(
        "suite ids `{first}` and `{second}` differ only by case and would share an artifact directory"
    )]
    ArtifactPathCollision { first: SuiteId, second: SuiteId },
    #[error("suite `{suite}`: {source}")]
    SuiteExceedsBudget {
        suite: SuiteId,
        source: ResourceError,
    },
    #[error("execution.default_environment `{0}` is not defined under [environments]")]
    UnknownDefaultEnvironment(EnvironmentName),
    #[error("policy.ci_default_mode must be full or shadow; affected alone is not a quality gate")]
    CiDefaultModeIsNotAGate,
    #[error("{field} must be greater than zero")]
    NotPositive { field: &'static str },
    #[error("{field}: {source}")]
    Pattern {
        field: &'static str,
        source: PatternError,
    },
    #[error("environments.{environment}: docker_context must not be empty")]
    EmptyDockerContext { environment: EnvironmentName },
    #[error(
        "environments.{environment}: readiness_commands[{index}] is empty or has an empty argument"
    )]
    InvalidReadinessCommand {
        environment: EnvironmentName,
        index: usize,
    },
    #[error("storage.directory must not be `.git` or inside it")]
    StorageInsideGit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    config_path: RepoPath,
    config_digest: Fingerprint,
    exclude: PatternSet,
    non_test_inputs: PatternSet,
    global_inputs: PatternSet,
    declared_inputs: PatternSet,
    budget: ResourceBudget,
    default_environment: EnvironmentName,
    environments: BTreeMap<EnvironmentName, EnvironmentSettings>,
    default_timeout: Duration,
    termination_grace: Duration,
    cleanup_timeout: Duration,
    fail_fast: bool,
    max_log_bytes_per_stream: u64,
    local_default_mode: Mode,
    ci_default_mode: Mode,
    fail_on_unexpected_skips: bool,
    fail_on_flaky: bool,
    storage_directory: RepoPath,
    history_retention_days: u32,
    suites: Vec<SuiteDefinition>,
}

impl Settings {
    pub fn try_new(parts: SettingsParts) -> Result<Self, Vec<SettingsIssue>> {
        let mut issues = Vec::new();

        check_suites(&parts, &mut issues);
        check_environments(&parts, &mut issues);
        if parts.ci_default_mode == Mode::Affected {
            issues.push(SettingsIssue::CiDefaultModeIsNotAGate);
        }
        for (field, is_zero) in [
            (
                "execution.default_timeout_seconds",
                parts.default_timeout.is_zero(),
            ),
            (
                "execution.termination_grace_seconds",
                parts.termination_grace.is_zero(),
            ),
            (
                "execution.cleanup_timeout_seconds",
                parts.cleanup_timeout.is_zero(),
            ),
            (
                "execution.max_log_bytes_per_stream",
                parts.max_log_bytes_per_stream == 0,
            ),
            (
                "storage.history_retention_days",
                parts.history_retention_days == 0,
            ),
        ] {
            if is_zero {
                issues.push(SettingsIssue::NotPositive { field });
            }
        }
        if parts.storage_directory.segments().next() == Some(".git") {
            issues.push(SettingsIssue::StorageInsideGit);
        }

        // Git metadata and testry's own state are never inputs, whatever the
        // configuration says.
        let mut exclude = parts.exclude.clone();
        exclude.push(".git/**".to_owned());
        exclude.push(format!(
            "{}/**",
            escape_glob(parts.storage_directory.as_str())
        ));
        let exclude = compile("workspace.exclude", &exclude, &mut issues);

        // The configuration file decides what is verified, so a change to it
        // is always a global input.
        let mut global_inputs = parts.global_inputs.clone();
        global_inputs.push(escape_glob(parts.config_path.as_str()));
        let mut declared_inputs = global_inputs.clone();
        for suite in &parts.suites {
            declared_inputs.extend(suite.inputs().sources().iter().cloned());
        }
        let global_inputs = compile("analysis.global_inputs", &global_inputs, &mut issues);
        let declared_inputs = compile("inputs", &declared_inputs, &mut issues);
        let non_test_inputs = compile(
            "workspace.non_test_inputs",
            &parts.non_test_inputs,
            &mut issues,
        );

        if !issues.is_empty() {
            return Err(issues);
        }
        Ok(Self {
            config_path: parts.config_path,
            config_digest: parts.config_digest,
            exclude,
            non_test_inputs,
            global_inputs,
            declared_inputs,
            budget: parts.budget,
            default_environment: parts.default_environment,
            environments: parts.environments,
            default_timeout: parts.default_timeout,
            termination_grace: parts.termination_grace,
            cleanup_timeout: parts.cleanup_timeout,
            fail_fast: parts.fail_fast,
            max_log_bytes_per_stream: parts.max_log_bytes_per_stream,
            local_default_mode: parts.local_default_mode,
            ci_default_mode: parts.ci_default_mode,
            fail_on_unexpected_skips: parts.fail_on_unexpected_skips,
            fail_on_flaky: parts.fail_on_flaky,
            storage_directory: parts.storage_directory,
            history_retention_days: parts.history_retention_days,
            suites: parts.suites,
        })
    }

    pub fn config_path(&self) -> &RepoPath {
        &self.config_path
    }

    pub fn config_digest(&self) -> &Fingerprint {
        &self.config_digest
    }

    /// Runtime outputs that are not inputs. Always contains `.git/**` and the
    /// storage directory.
    pub fn exclude(&self) -> &PatternSet {
        &self.exclude
    }

    pub fn non_test_inputs(&self) -> &PatternSet {
        &self.non_test_inputs
    }

    /// Always contains the configuration file itself.
    pub fn global_inputs(&self) -> &PatternSet {
        &self.global_inputs
    }

    /// `global_inputs` plus the `inputs` of every suite.
    pub fn declared_inputs(&self) -> &PatternSet {
        &self.declared_inputs
    }

    pub fn budget(&self) -> &ResourceBudget {
        &self.budget
    }

    pub fn default_environment(&self) -> &EnvironmentName {
        &self.default_environment
    }

    pub fn environments(&self) -> &BTreeMap<EnvironmentName, EnvironmentSettings> {
        &self.environments
    }

    pub fn default_timeout(&self) -> Duration {
        self.default_timeout
    }

    pub fn termination_grace(&self) -> Duration {
        self.termination_grace
    }

    pub fn cleanup_timeout(&self) -> Duration {
        self.cleanup_timeout
    }

    pub fn fail_fast(&self) -> bool {
        self.fail_fast
    }

    pub fn max_log_bytes_per_stream(&self) -> u64 {
        self.max_log_bytes_per_stream
    }

    pub fn local_default_mode(&self) -> Mode {
        self.local_default_mode
    }

    pub fn ci_default_mode(&self) -> Mode {
        self.ci_default_mode
    }

    pub fn fail_on_unexpected_skips(&self) -> bool {
        self.fail_on_unexpected_skips
    }

    pub fn fail_on_flaky(&self) -> bool {
        self.fail_on_flaky
    }

    pub fn storage_directory(&self) -> &RepoPath {
        &self.storage_directory
    }

    pub fn history_retention_days(&self) -> u32 {
        self.history_retention_days
    }

    pub fn suites(&self) -> &[SuiteDefinition] {
        &self.suites
    }

    /// The resolver profiles of all configured suites.
    pub fn resolver_profiles(&self) -> BTreeSet<ResolverProfile> {
        self.suites
            .iter()
            .filter_map(|suite| ResolverProfile::for_runner(suite.runner()))
            .collect()
    }
}

fn check_suites(parts: &SettingsParts, issues: &mut Vec<SettingsIssue>) {
    if !parts
        .suites
        .iter()
        .any(|suite| suite.runner().is_test_runner())
    {
        issues.push(SettingsIssue::NoTestSuite);
    }
    for (index, suite) in parts.suites.iter().enumerate() {
        for earlier in parts.suites.iter().take(index) {
            if earlier.id() == suite.id() {
                issues.push(SettingsIssue::DuplicateSuite(suite.id().clone()));
            } else if earlier
                .id()
                .as_str()
                .eq_ignore_ascii_case(suite.id().as_str())
            {
                issues.push(SettingsIssue::ArtifactPathCollision {
                    first: earlier.id().clone(),
                    second: suite.id().clone(),
                });
            }
        }
        if let Err(source) = parts.budget.admit(suite.resources()) {
            issues.push(SettingsIssue::SuiteExceedsBudget {
                suite: suite.id().clone(),
                source,
            });
        }
    }
}

fn check_environments(parts: &SettingsParts, issues: &mut Vec<SettingsIssue>) {
    if !parts.environments.contains_key(&parts.default_environment) {
        issues.push(SettingsIssue::UnknownDefaultEnvironment(
            parts.default_environment.clone(),
        ));
    }
    for (name, environment) in &parts.environments {
        let EnvironmentSettings::DevcontainerCli(devcontainer) = environment else {
            continue;
        };
        if devcontainer.docker_context.trim().is_empty() {
            issues.push(SettingsIssue::EmptyDockerContext {
                environment: name.clone(),
            });
        }
        for (index, command) in devcontainer.readiness_commands.iter().enumerate() {
            let invalid = command.is_empty()
                || command
                    .iter()
                    .any(|argument| argument.is_empty() || argument.contains('\0'));
            if invalid {
                issues.push(SettingsIssue::InvalidReadinessCommand {
                    environment: name.clone(),
                    index,
                });
            }
        }
    }
}

fn compile(field: &'static str, sources: &[String], issues: &mut Vec<SettingsIssue>) -> PatternSet {
    match PatternSet::try_new(sources) {
        Ok(patterns) => patterns,
        Err(source) => {
            issues.push(SettingsIssue::Pattern { field, source });
            PatternSet::empty()
        }
    }
}

/// Escapes glob metacharacters so a literal path can be used as a pattern.
fn escape_glob(literal: &str) -> String {
    let mut escaped = String::with_capacity(literal.len());
    for character in literal.chars() {
        if matches!(character, '*' | '?' | '[' | ']' | '{' | '}' | '\\' | '!') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    //! Builders for settings in tests.

    use testry_domain::ids::RunnerProject;
    use testry_domain::paths::RepoDir;
    use testry_domain::resources::ResourceRequest;
    use testry_domain::suite::{RunnerKind, SelectionStrategy, SuiteDefinitionParts};

    use super::*;

    #[allow(clippy::unwrap_used, clippy::expect_used)]
    pub fn suite_parts(id: &str, runner: RunnerKind, include: &[&str]) -> SuiteDefinitionParts {
        SuiteDefinitionParts {
            id: SuiteId::try_new(id).unwrap(),
            runner,
            cwd: RepoDir::root(),
            command: vec!["bun".to_owned(), "test".to_owned()],
            project: matches!(runner, RunnerKind::Vitest | RunnerKind::StorybookVitest)
                .then(|| RunnerProject::try_new(id).unwrap()),
            include: PatternSet::try_new(include).unwrap(),
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

    #[allow(clippy::unwrap_used, clippy::expect_used)]
    pub fn suite(id: &str, runner: RunnerKind, include: &[&str]) -> SuiteDefinition {
        SuiteDefinition::try_new(suite_parts(id, runner, include)).unwrap()
    }

    #[allow(clippy::unwrap_used, clippy::expect_used)]
    pub fn settings_parts(suites: Vec<SuiteDefinition>) -> SettingsParts {
        let local = EnvironmentName::try_new("local").unwrap();
        SettingsParts {
            config_path: RepoPath::try_new("testry.toml").unwrap(),
            config_digest: Fingerprint::of_bytes(b"config"),
            exclude: vec!["**/node_modules/**".to_owned()],
            non_test_inputs: Vec::new(),
            global_inputs: vec!["**/package.json".to_owned(), "**/bun.lock".to_owned()],
            budget: ResourceBudget::try_new(4, 4096, 2).unwrap(),
            default_environment: local.clone(),
            environments: BTreeMap::from([(local, EnvironmentSettings::Host)]),
            default_timeout: Duration::from_secs(600),
            termination_grace: Duration::from_secs(5),
            cleanup_timeout: Duration::from_secs(30),
            fail_fast: false,
            max_log_bytes_per_stream: 16 * 1024 * 1024,
            local_default_mode: Mode::Affected,
            ci_default_mode: Mode::Full,
            fail_on_unexpected_skips: true,
            fail_on_flaky: true,
            storage_directory: RepoPath::try_new(".testry").unwrap(),
            history_retention_days: 14,
            suites,
        }
    }

    #[allow(clippy::unwrap_used, clippy::expect_used)]
    pub fn settings(suites: Vec<SuiteDefinition>) -> Settings {
        Settings::try_new(settings_parts(suites)).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use testry_domain::resources::ResourceRequest;
    use testry_domain::suite::RunnerKind;

    use super::test_support::{settings_parts, suite, suite_parts};
    use super::*;

    fn path(raw: &str) -> RepoPath {
        RepoPath::try_new(raw).unwrap()
    }

    fn issues(parts: SettingsParts) -> Vec<SettingsIssue> {
        Settings::try_new(parts).unwrap_err()
    }

    #[test]
    fn accepts_a_minimal_configuration() {
        let settings = Settings::try_new(settings_parts(vec![suite(
            "api",
            RunnerKind::Bun,
            &["**/*.test.ts"],
        )]))
        .unwrap();
        assert_eq!(settings.suites().len(), 1);
        assert_eq!(
            settings.resolver_profiles(),
            BTreeSet::from([ResolverProfile::Bun])
        );
    }

    #[test]
    fn command_suites_alone_are_not_test_verification() {
        let parts = settings_parts(vec![suite("typecheck", RunnerKind::Command, &[])]);
        assert_eq!(issues(parts), vec![SettingsIssue::NoTestSuite]);
        assert_eq!(
            issues(settings_parts(Vec::new())),
            vec![SettingsIssue::NoTestSuite]
        );
    }

    #[test]
    fn duplicate_and_case_colliding_suite_ids_are_rejected() {
        let duplicated = settings_parts(vec![
            suite("api", RunnerKind::Bun, &["**/*.test.ts"]),
            suite("api", RunnerKind::Bun, &["**/*.spec.ts"]),
        ]);
        assert!(matches!(
            issues(duplicated).as_slice(),
            [SettingsIssue::DuplicateSuite(_)]
        ));

        let colliding = settings_parts(vec![
            suite("api", RunnerKind::Bun, &["**/*.test.ts"]),
            suite("API", RunnerKind::Bun, &["**/*.spec.ts"]),
        ]);
        assert!(matches!(
            issues(colliding).as_slice(),
            [SettingsIssue::ArtifactPathCollision { .. }]
        ));
    }

    /// R01: a suite that can never fit the budget is a configuration error.
    #[test]
    fn r01_suite_larger_than_the_budget_is_rejected() {
        let mut big = suite_parts("api", RunnerKind::Bun, &["**/*.test.ts"]);
        big.resources = ResourceRequest::try_new(8, 512, []).unwrap();
        let parts = settings_parts(vec![SuiteDefinition::try_new(big).unwrap()]);
        assert!(matches!(
            issues(parts).as_slice(),
            [SettingsIssue::SuiteExceedsBudget { .. }]
        ));
    }

    #[test]
    fn default_environment_must_exist() {
        let mut parts = settings_parts(vec![suite("api", RunnerKind::Bun, &["**/*.test.ts"])]);
        parts.default_environment = EnvironmentName::try_new("colima").unwrap();
        assert!(matches!(
            issues(parts).as_slice(),
            [SettingsIssue::UnknownDefaultEnvironment(_)]
        ));
    }

    /// P07: affected can never be the CI default.
    #[test]
    fn p07_ci_default_mode_must_be_a_gate() {
        let mut parts = settings_parts(vec![suite("api", RunnerKind::Bun, &["**/*.test.ts"])]);
        parts.ci_default_mode = Mode::Affected;
        assert_eq!(issues(parts), vec![SettingsIssue::CiDefaultModeIsNotAGate]);
    }

    #[test]
    fn zero_durations_and_limits_are_rejected() {
        let mut parts = settings_parts(vec![suite("api", RunnerKind::Bun, &["**/*.test.ts"])]);
        parts.default_timeout = Duration::ZERO;
        parts.cleanup_timeout = Duration::ZERO;
        parts.max_log_bytes_per_stream = 0;
        assert_eq!(issues(parts).len(), 3);
    }

    #[test]
    fn invalid_patterns_are_reported_with_their_key() {
        let mut parts = settings_parts(vec![suite("api", RunnerKind::Bun, &["**/*.test.ts"])]);
        parts.global_inputs = vec!["../outside/**".to_owned()];
        parts.exclude = vec!["/abs/**".to_owned()];
        let found = issues(parts);
        assert!(found.iter().any(|issue| matches!(
            issue,
            SettingsIssue::Pattern {
                field: "analysis.global_inputs",
                ..
            }
        )));
        assert!(found.iter().any(|issue| matches!(
            issue,
            SettingsIssue::Pattern {
                field: "workspace.exclude",
                ..
            }
        )));
    }

    #[test]
    fn storage_and_git_are_always_excluded_and_config_is_always_global() {
        let mut parts = settings_parts(vec![suite("api", RunnerKind::Bun, &["**/*.test.ts"])]);
        parts.exclude = Vec::new();
        parts.global_inputs = Vec::new();
        parts.config_path = path("config/testry [ci].toml");
        let settings = Settings::try_new(parts).unwrap();
        assert!(
            settings
                .exclude()
                .is_match(&path(".testry/runs/x/result.json"))
        );
        assert!(settings.exclude().is_match(&path(".git/HEAD")));
        assert!(!settings.exclude().is_match(&path("src/a.ts")));
        assert!(
            settings
                .global_inputs()
                .is_match(&path("config/testry [ci].toml"))
        );
        assert!(
            !settings
                .global_inputs()
                .is_match(&path("config/testry c.toml"))
        );
    }

    #[test]
    fn storage_must_not_live_inside_git() {
        let mut parts = settings_parts(vec![suite("api", RunnerKind::Bun, &["**/*.test.ts"])]);
        parts.storage_directory = path(".git/testry");
        assert!(issues(parts).contains(&SettingsIssue::StorageInsideGit));
    }

    #[test]
    fn devcontainer_environments_are_checked() {
        let mut parts = settings_parts(vec![suite("api", RunnerKind::Bun, &["**/*.test.ts"])]);
        parts.environments.insert(
            EnvironmentName::try_new("colima").unwrap(),
            EnvironmentSettings::DevcontainerCli(DevcontainerSettings {
                docker_context: " ".to_owned(),
                config: path(".devcontainer/devcontainer.json"),
                allow_initialize_command: false,
                readiness_commands: vec![
                    vec!["bun".to_owned(), "--version".to_owned()],
                    Vec::new(),
                ],
            }),
        );
        let found = issues(parts);
        assert!(
            found
                .iter()
                .any(|issue| matches!(issue, SettingsIssue::EmptyDockerContext { .. }))
        );
        assert!(found.iter().any(|issue| matches!(
            issue,
            SettingsIssue::InvalidReadinessCommand { index: 1, .. }
        )));
    }

    #[test]
    fn declared_inputs_combine_global_and_suite_inputs() {
        let mut api = suite_parts("api", RunnerKind::Bun, &["**/*.test.ts"]);
        api.inputs = PatternSet::try_new(["apps/api/**"]).unwrap();
        let settings =
            Settings::try_new(settings_parts(vec![SuiteDefinition::try_new(api).unwrap()]))
                .unwrap();
        assert!(
            settings
                .declared_inputs()
                .is_match(&path("apps/api/.env.local"))
        );
        assert!(settings.declared_inputs().is_match(&path("package.json")));
        assert!(!settings.declared_inputs().is_match(&path("apps/web/x.ts")));
    }
}
