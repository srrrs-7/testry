//! Loading and validating `testry.toml` (spec section 15).
//!
//! `testry.toml` is the only configuration source. Unknown keys, unknown enum
//! values and a different `schema_version` are errors. Parsing is split from
//! file access so the rules can be tested without a file system.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use testry_application::settings::{
    DevcontainerSettings, EnvironmentSettings, Settings, SettingsParts,
};
use testry_domain::fingerprint::Fingerprint;
use testry_domain::ids::{EnvironmentName, ExclusiveKey, RunnerProject, SuiteId};
use testry_domain::paths::{RepoDir, RepoPath};
use testry_domain::pattern::PatternSet;
use testry_domain::plan::Mode;
use testry_domain::resources::{ResourceBudget, ResourceRequest};
use testry_domain::suite::{
    KnownSkip, RunnerKind, SelectionStrategy, SkipStatus, SuiteDefinition, SuiteDefinitionParts,
};

pub const CONFIG_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_CONFIG_FILE: &str = "testry.toml";

/// Upper bound for the configuration file. A larger file is not read.
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// One problem in the configuration, addressed by its key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigIssue {
    pub key: String,
    pub message: String,
}

impl fmt::Display for ConfigIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.key.is_empty() {
            f.write_str(&self.message)
        } else {
            write!(f, "{}: {}", self.key, self.message)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read configuration {path}: {message}")]
    Read { path: String, message: String },
    #[error("configuration {path} is larger than {limit} bytes")]
    TooLarge { path: String, limit: u64 },
    #[error("configuration is not valid: {0}")]
    Parse(String),
    #[error(
        "configuration schema_version {found} is not supported; this build reads version {CONFIG_SCHEMA_VERSION}"
    )]
    UnsupportedSchemaVersion { found: i64 },
    #[error("configuration is invalid:\n{}", format_issues(.0))]
    Invalid(Vec<ConfigIssue>),
    #[error("workspace.root {root:?} must be a relative path")]
    AbsoluteWorkspaceRoot { root: String },
    #[error("workspace root {path} cannot be resolved: {message}")]
    WorkspaceRoot { path: String, message: String },
    #[error("configuration file {config} is not inside the workspace root {root}")]
    OutsideWorkspace { config: String, root: String },
    #[error("configuration path is not usable: {0}")]
    ConfigPath(String),
}

fn format_issues(issues: &[ConfigIssue]) -> String {
    issues
        .iter()
        .map(|issue| format!("  - {issue}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A configuration that was read from disk.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub settings: Settings,
    /// Absolute, canonical path of the workspace.
    pub workspace_root: PathBuf,
}

/// Reads and validates the configuration file.
pub fn load(config_file: &Path) -> Result<LoadedConfig, ConfigError> {
    let shown = config_file.display().to_string();
    let read_error = |error: std::io::Error| ConfigError::Read {
        path: shown.clone(),
        message: error.kind().to_string(),
    };
    let metadata = std::fs::metadata(config_file).map_err(read_error)?;
    if metadata.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::TooLarge {
            path: shown,
            limit: MAX_CONFIG_BYTES,
        });
    }
    let bytes = std::fs::read(config_file).map_err(read_error)?;
    let text = String::from_utf8(bytes)
        .map_err(|_| ConfigError::Parse("the file is not valid UTF-8".to_owned()))?;

    let raw = parse_raw(&text)?;
    let config_file = std::fs::canonicalize(config_file).map_err(read_error)?;
    let config_directory = config_file
        .parent()
        .ok_or_else(|| ConfigError::ConfigPath("the file has no parent directory".to_owned()))?;

    if Path::new(&raw.workspace.root).is_absolute() {
        return Err(ConfigError::AbsoluteWorkspaceRoot {
            root: raw.workspace.root.clone(),
        });
    }
    let joined = config_directory.join(&raw.workspace.root);
    let workspace_root =
        std::fs::canonicalize(&joined).map_err(|error| ConfigError::WorkspaceRoot {
            path: joined.display().to_string(),
            message: error.kind().to_string(),
        })?;
    let relative =
        config_file
            .strip_prefix(&workspace_root)
            .map_err(|_| ConfigError::OutsideWorkspace {
                config: config_file.display().to_string(),
                root: workspace_root.display().to_string(),
            })?;
    let relative = relative
        .to_str()
        .ok_or_else(|| ConfigError::ConfigPath("the path is not valid UTF-8".to_owned()))?;
    let config_path =
        RepoPath::try_new(relative).map_err(|error| ConfigError::ConfigPath(error.to_string()))?;

    let settings = validate(raw, config_path, Fingerprint::of_bytes(text.as_bytes()))?;
    Ok(LoadedConfig {
        settings,
        workspace_root,
    })
}

/// Parses and validates configuration text. `config_path` is where the file
/// lives inside the workspace.
#[cfg(test)]
pub fn parse(text: &str, config_path: RepoPath) -> Result<Settings, ConfigError> {
    let raw = parse_raw(text)?;
    validate(raw, config_path, Fingerprint::of_bytes(text.as_bytes()))
}

fn parse_raw(text: &str) -> Result<RawConfig, ConfigError> {
    // The version is checked first so that a newer file is reported as such
    // rather than as a list of unknown keys.
    let table: toml::Table =
        toml::from_str(text).map_err(|error| ConfigError::Parse(error.to_string()))?;
    match table.get("schema_version") {
        Some(toml::Value::Integer(found)) if *found == i64::from(CONFIG_SCHEMA_VERSION) => {}
        Some(toml::Value::Integer(found)) => {
            return Err(ConfigError::UnsupportedSchemaVersion { found: *found });
        }
        Some(_) => {
            return Err(ConfigError::Parse(
                "schema_version must be an integer".to_owned(),
            ));
        }
        None => return Err(ConfigError::Parse("schema_version is required".to_owned())),
    }
    toml::from_str(text).map_err(|error| ConfigError::Parse(error.to_string()))
}

// ------------------------------------------------------------ raw document --

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[allow(dead_code)]
    schema_version: u32,
    #[serde(default)]
    workspace: RawWorkspace,
    analysis: RawAnalysis,
    execution: RawExecution,
    #[serde(default)]
    policy: RawPolicy,
    #[serde(default)]
    storage: RawStorage,
    environments: BTreeMap<String, RawEnvironment>,
    #[serde(default)]
    suites: Vec<RawSuite>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWorkspace {
    #[serde(default = "default_root")]
    root: String,
    #[serde(default = "default_exclude")]
    exclude: Vec<String>,
    #[serde(default)]
    non_test_inputs: Vec<String>,
}

impl Default for RawWorkspace {
    fn default() -> Self {
        Self {
            root: default_root(),
            exclude: default_exclude(),
            non_test_inputs: Vec::new(),
        }
    }
}

fn default_root() -> String {
    ".".to_owned()
}

fn default_exclude() -> Vec<String> {
    vec!["**/node_modules/**".to_owned()]
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawLanguage {
    #[default]
    TypescriptJavascript,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawGranularity {
    #[default]
    File,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawOxc {
    #[default]
    Oxc,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawUnknownDependency {
    #[default]
    AllEnabledSuites,
}

/// The single-valued keys document the v0.1 behaviour. Any other value is an
/// unknown enum value and therefore an error.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAnalysis {
    #[serde(default)]
    #[allow(dead_code)]
    language: RawLanguage,
    #[serde(default)]
    #[allow(dead_code)]
    granularity: RawGranularity,
    #[serde(default)]
    #[allow(dead_code)]
    parser: RawOxc,
    #[serde(default)]
    #[allow(dead_code)]
    resolver: RawOxc,
    #[serde(default)]
    #[allow(dead_code)]
    unknown_dependency: RawUnknownDependency,
    /// Required: there is no hidden default for what widens to a full run.
    global_inputs: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawExecution {
    default_environment: String,
    /// Required: a budget is a statement about the machine, not a default.
    cpu_slots: u32,
    memory_budget_mib: u64,
    #[serde(default = "default_max_parallel_suites")]
    max_parallel_suites: u32,
    #[serde(default = "default_timeout_seconds")]
    default_timeout_seconds: u64,
    #[serde(default = "default_termination_grace_seconds")]
    termination_grace_seconds: u64,
    #[serde(default = "default_cleanup_timeout_seconds")]
    cleanup_timeout_seconds: u64,
    #[serde(default)]
    fail_fast: bool,
    #[serde(default = "default_max_log_bytes")]
    max_log_bytes_per_stream: u64,
}

fn default_max_parallel_suites() -> u32 {
    1
}

fn default_timeout_seconds() -> u64 {
    600
}

fn default_termination_grace_seconds() -> u64 {
    5
}

fn default_cleanup_timeout_seconds() -> u64 {
    30
}

fn default_max_log_bytes() -> u64 {
    16 * 1024 * 1024
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawMode {
    Affected,
    Full,
    Shadow,
}

impl From<RawMode> for Mode {
    fn from(mode: RawMode) -> Self {
        match mode {
            RawMode::Affected => Self::Affected,
            RawMode::Full => Self::Full,
            RawMode::Shadow => Self::Shadow,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPolicy {
    #[serde(default = "default_local_mode")]
    local_default_mode: RawMode,
    #[serde(default = "default_ci_mode")]
    ci_default_mode: RawMode,
    #[serde(default)]
    allow_result_cache: bool,
    #[serde(default)]
    allow_ai_test_exclusion: bool,
    #[serde(default = "default_true")]
    fail_on_unexpected_skips: bool,
    #[serde(default = "default_true")]
    fail_on_flaky: bool,
}

impl Default for RawPolicy {
    fn default() -> Self {
        Self {
            local_default_mode: default_local_mode(),
            ci_default_mode: default_ci_mode(),
            allow_result_cache: false,
            allow_ai_test_exclusion: false,
            fail_on_unexpected_skips: true,
            fail_on_flaky: true,
        }
    }
}

fn default_local_mode() -> RawMode {
    RawMode::Affected
}

fn default_ci_mode() -> RawMode {
    RawMode::Full
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStorage {
    #[serde(default = "default_storage_directory")]
    directory: String,
    #[serde(default = "default_retention_days")]
    history_retention_days: u32,
}

impl Default for RawStorage {
    fn default() -> Self {
        Self {
            directory: default_storage_directory(),
            history_retention_days: default_retention_days(),
        }
    }
}

fn default_storage_directory() -> String {
    ".testry".to_owned()
}

fn default_retention_days() -> u32 {
    14
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawNetwork {
    #[default]
    Inherit,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawLifecycle {
    #[default]
    Disposable,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case", deny_unknown_fields)]
enum RawEnvironment {
    Host {
        #[serde(default)]
        #[allow(dead_code)]
        network: RawNetwork,
    },
    DevcontainerCli {
        docker_context: String,
        config: String,
        #[serde(default)]
        #[allow(dead_code)]
        lifecycle: RawLifecycle,
        #[serde(default)]
        allow_initialize_command: bool,
        #[serde(default)]
        readiness_commands: Vec<Vec<String>>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawResources {
    cpu_slots: u32,
    memory_mib: u64,
    #[serde(default)]
    exclusive: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawKnownSkip {
    #[serde(default)]
    project: Option<String>,
    file: String,
    title_path: Vec<String>,
    expected_status: SkipStatus,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSuite {
    id: String,
    runner: RunnerKind,
    #[serde(default = "default_root")]
    cwd: String,
    command: Vec<String>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    inputs: Vec<String>,
    /// Required: whether a suite may be narrowed is an explicit decision.
    selection: SelectionStrategy,
    #[serde(default)]
    always_run: bool,
    #[serde(default)]
    workers: Option<u32>,
    #[serde(default)]
    timeout_seconds: Option<u64>,
    resources: RawResources,
    #[serde(default)]
    known_skips: Vec<RawKnownSkip>,
}

// -------------------------------------------------------------- validation --

struct Issues(Vec<ConfigIssue>);

impl Issues {
    fn push(&mut self, key: impl Into<String>, message: impl fmt::Display) {
        self.0.push(ConfigIssue {
            key: key.into(),
            message: message.to_string(),
        });
    }

    /// Records the error of `result` under `key` and returns the value.
    fn take<T, E: fmt::Display>(
        &mut self,
        key: impl Into<String>,
        result: Result<T, E>,
    ) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(error) => {
                self.push(key, error);
                None
            }
        }
    }
}

fn validate(
    raw: RawConfig,
    config_path: RepoPath,
    config_digest: Fingerprint,
) -> Result<Settings, ConfigError> {
    let mut issues = Issues(Vec::new());

    // v0.1 never reuses test results and never lets a model exclude tests.
    if raw.policy.allow_result_cache {
        issues.push(
            "policy.allow_result_cache",
            "must be false: v0.1 never reuses test results",
        );
    }
    if raw.policy.allow_ai_test_exclusion {
        issues.push(
            "policy.allow_ai_test_exclusion",
            "must be false: v0.1 never lets a model exclude tests",
        );
    }

    let budget = issues.take(
        "execution",
        ResourceBudget::try_new(
            raw.execution.cpu_slots,
            raw.execution.memory_budget_mib,
            raw.execution.max_parallel_suites,
        ),
    );
    let default_environment = issues.take(
        "execution.default_environment",
        EnvironmentName::try_new(raw.execution.default_environment.clone()),
    );
    let storage_directory = issues.take(
        "storage.directory",
        RepoPath::try_new(&raw.storage.directory),
    );

    let mut environments = BTreeMap::new();
    for (name, environment) in raw.environments {
        let key = format!("environments.{name}");
        let Some(name) = issues.take(key.clone(), EnvironmentName::try_new(name)) else {
            continue;
        };
        let settings = match environment {
            RawEnvironment::Host { .. } => EnvironmentSettings::Host,
            RawEnvironment::DevcontainerCli {
                docker_context,
                config,
                allow_initialize_command,
                readiness_commands,
                ..
            } => {
                let Some(config) = issues.take(format!("{key}.config"), RepoPath::try_new(config))
                else {
                    continue;
                };
                EnvironmentSettings::DevcontainerCli(DevcontainerSettings {
                    docker_context,
                    config,
                    allow_initialize_command,
                    readiness_commands,
                })
            }
        };
        environments.insert(name, settings);
    }

    if raw.suites.is_empty() {
        issues.push("suites", "at least one [[suites]] entry is required");
    }
    let default_timeout = raw.execution.default_timeout_seconds;
    let mut suites = Vec::new();
    for (index, suite) in raw.suites.into_iter().enumerate() {
        if let Some(suite) = convert_suite(index, suite, default_timeout, &mut issues) {
            suites.push(suite);
        }
    }

    let (Some(budget), Some(default_environment), Some(storage_directory)) =
        (budget, default_environment, storage_directory)
    else {
        return Err(ConfigError::Invalid(issues.0));
    };

    let parts = SettingsParts {
        config_path,
        config_digest,
        exclude: raw.workspace.exclude,
        non_test_inputs: raw.workspace.non_test_inputs,
        global_inputs: raw.analysis.global_inputs,
        budget,
        default_environment,
        environments,
        default_timeout: Duration::from_secs(raw.execution.default_timeout_seconds),
        termination_grace: Duration::from_secs(raw.execution.termination_grace_seconds),
        cleanup_timeout: Duration::from_secs(raw.execution.cleanup_timeout_seconds),
        fail_fast: raw.execution.fail_fast,
        max_log_bytes_per_stream: raw.execution.max_log_bytes_per_stream,
        local_default_mode: raw.policy.local_default_mode.into(),
        ci_default_mode: raw.policy.ci_default_mode.into(),
        fail_on_unexpected_skips: raw.policy.fail_on_unexpected_skips,
        fail_on_flaky: raw.policy.fail_on_flaky,
        storage_directory,
        history_retention_days: raw.storage.history_retention_days,
        suites,
    };
    match Settings::try_new(parts) {
        Ok(settings) if issues.0.is_empty() => Ok(settings),
        Ok(_) => Err(ConfigError::Invalid(issues.0)),
        Err(settings_issues) => {
            for issue in settings_issues {
                issues.push("", issue);
            }
            Err(ConfigError::Invalid(issues.0))
        }
    }
}

fn convert_suite(
    index: usize,
    raw: RawSuite,
    default_timeout_seconds: u64,
    issues: &mut Issues,
) -> Option<SuiteDefinition> {
    let key = if raw.id.is_empty() {
        format!("suites[{index}]")
    } else {
        format!("suites[{index}] ({})", raw.id)
    };
    let before = issues.0.len();

    let id = issues.take(format!("{key}.id"), SuiteId::try_new(raw.id));
    let cwd = issues.take(format!("{key}.cwd"), RepoDir::try_new(&raw.cwd));
    let project = match raw.project {
        Some(project) => issues
            .take(format!("{key}.project"), RunnerProject::try_new(project))
            .map(Some),
        None => Some(None),
    };
    let include = issues.take(format!("{key}.include"), PatternSet::try_new(&raw.include));
    let inputs = issues.take(format!("{key}.inputs"), PatternSet::try_new(&raw.inputs));

    let mut exclusive = Vec::new();
    for name in raw.resources.exclusive {
        if let Some(exclusive_key) = issues.take(
            format!("{key}.resources.exclusive"),
            ExclusiveKey::try_new(name),
        ) {
            exclusive.push(exclusive_key);
        }
    }
    let resources = issues.take(
        format!("{key}.resources"),
        ResourceRequest::try_new(raw.resources.cpu_slots, raw.resources.memory_mib, exclusive),
    );

    let mut known_skips = Vec::new();
    for (skip_index, skip) in raw.known_skips.into_iter().enumerate() {
        let skip_key = format!("{key}.known_skips[{skip_index}]");
        let file = issues.take(format!("{skip_key}.file"), RepoPath::try_new(skip.file));
        let project = match skip.project {
            Some(project) => issues
                .take(
                    format!("{skip_key}.project"),
                    RunnerProject::try_new(project),
                )
                .map(Some),
            None => Some(None),
        };
        if let (Some(file), Some(project)) = (file, project) {
            known_skips.push(KnownSkip {
                project,
                file,
                title_path: skip.title_path,
                expected_status: skip.expected_status,
            });
        }
    }

    if issues.0.len() != before {
        return None;
    }
    let parts = SuiteDefinitionParts {
        id: id?,
        runner: raw.runner,
        cwd: cwd?,
        command: raw.command,
        project: project?,
        include: include?,
        inputs: inputs?,
        selection: raw.selection,
        always_run: raw.always_run,
        workers: raw.workers,
        timeout: Duration::from_secs(raw.timeout_seconds.unwrap_or(default_timeout_seconds)),
        resources: resources?,
        known_skips,
    };
    issues.take(key, SuiteDefinition::try_new(parts))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sample configuration of spec section 15, with the names of this
    /// project (`testry.toml`, `.testry`).
    pub(crate) const SAMPLE: &str = include_str!("../tests/fixtures/sample.testry.toml");

    fn config_path() -> RepoPath {
        RepoPath::try_new(DEFAULT_CONFIG_FILE).unwrap()
    }

    fn parse_text(text: &str) -> Result<Settings, ConfigError> {
        parse(text, config_path())
    }

    fn issues_of(text: &str) -> Vec<ConfigIssue> {
        match parse_text(text) {
            Err(ConfigError::Invalid(issues)) => issues,
            other => panic!("expected validation issues, got {other:?}"),
        }
    }

    /// Replaces one line of the sample. Fails the test when the line is absent,
    /// so a sample change cannot silently turn a negative test into a no-op.
    fn sample_with(from: &str, to: &str) -> String {
        assert!(SAMPLE.contains(from), "sample does not contain {from:?}");
        SAMPLE.replacen(from, to, 1)
    }

    fn has_issue(issues: &[ConfigIssue], key_part: &str, message_part: &str) -> bool {
        issues
            .iter()
            .any(|issue| issue.key.contains(key_part) && issue.message.contains(message_part))
    }

    #[test]
    fn the_spec_sample_is_valid() {
        let settings = parse_text(SAMPLE).unwrap();
        let ids: Vec<&str> = settings
            .suites()
            .iter()
            .map(|suite| suite.id().as_str())
            .collect();
        assert_eq!(
            ids,
            ["api-unit", "web-unit", "storybook", "e2e", "typecheck"]
        );
        assert_eq!(settings.budget().cpu_slots(), 4);
        assert_eq!(settings.budget().memory_mib(), 4096);
        assert_eq!(settings.budget().max_parallel_suites(), 2);
        assert_eq!(settings.local_default_mode(), Mode::Affected);
        assert_eq!(settings.ci_default_mode(), Mode::Full);
        assert_eq!(settings.environments().len(), 2);
        assert_eq!(settings.storage_directory().as_str(), ".testry");
        let e2e = &settings.suites()[3];
        assert_eq!(e2e.resources().exclusive().len(), 2);
        assert_eq!(e2e.timeout(), Duration::from_secs(600));
        assert!(settings.suites()[4].always_run());
        assert!(
            settings
                .global_inputs()
                .is_match(&RepoPath::try_new("apps/api/bun.lock").unwrap())
        );
        assert!(settings.global_inputs().is_match(&config_path()));
    }

    #[test]
    fn unknown_keys_are_rejected_at_every_level() {
        for (from, to) in [
            ("schema_version = 1", "schema_version = 1\nsurprise = true"),
            ("[workspace]", "[workspace]\nsurprise = true"),
            ("[analysis]", "[analysis]\nsurprise = true"),
            ("[execution]", "[execution]\nsurprise = true"),
            ("[policy]", "[policy]\nsurprise = true"),
            ("[storage]", "[storage]\nsurprise = true"),
            (
                "[environments.local]",
                "[environments.local]\nsurprise = true",
            ),
            ("id = \"api-unit\"", "id = \"api-unit\"\nsurprise = true"),
            (
                "cpu_slots = 1\nmemory_mib = 512",
                "cpu_slots = 1\nsurprise = 1\nmemory_mib = 512",
            ),
        ] {
            let error = parse_text(&sample_with(from, to)).unwrap_err();
            assert!(
                matches!(&error, ConfigError::Parse(message) if message.contains("surprise")),
                "{from}: {error}"
            );
        }
    }

    #[test]
    fn unknown_enum_values_are_rejected() {
        for (from, to) in [
            (
                "language = \"typescript_javascript\"",
                "language = \"python\"",
            ),
            ("granularity = \"file\"", "granularity = \"function\""),
            ("parser = \"oxc\"", "parser = \"swc\""),
            (
                "unknown_dependency = \"all_enabled_suites\"",
                "unknown_dependency = \"ignore\"",
            ),
            (
                "local_default_mode = \"affected\"",
                "local_default_mode = \"safe\"",
            ),
            ("runner = \"bun\"", "runner = \"jest\""),
            ("selection = \"file_graph\"", "selection = \"test_case\""),
            ("provider = \"host\"", "provider = \"kiri\""),
            ("network = \"inherit\"", "network = \"none\""),
            ("lifecycle = \"disposable\"", "lifecycle = \"warm\""),
        ] {
            let error = parse_text(&sample_with(from, to)).unwrap_err();
            assert!(matches!(error, ConfigError::Parse(_)), "{from}: {error}");
        }
    }

    #[test]
    fn other_schema_versions_are_rejected_before_anything_else() {
        let newer = sample_with(
            "schema_version = 1",
            "schema_version = 2\nfuture_key = true",
        );
        assert_eq!(
            parse_text(&newer).unwrap_err(),
            ConfigError::UnsupportedSchemaVersion { found: 2 }
        );
        let missing = sample_with("schema_version = 1", "");
        assert!(matches!(
            parse_text(&missing).unwrap_err(),
            ConfigError::Parse(_)
        ));
        let text = sample_with("schema_version = 1", "schema_version = \"1\"");
        assert!(matches!(
            parse_text(&text).unwrap_err(),
            ConfigError::Parse(_)
        ));
    }

    #[test]
    fn result_cache_and_ai_exclusion_cannot_be_enabled() {
        let cache = issues_of(&sample_with(
            "allow_result_cache = false",
            "allow_result_cache = true",
        ));
        assert!(has_issue(
            &cache,
            "policy.allow_result_cache",
            "must be false"
        ));
        let ai = issues_of(&sample_with(
            "allow_ai_test_exclusion = false",
            "allow_ai_test_exclusion = true",
        ));
        assert!(has_issue(
            &ai,
            "policy.allow_ai_test_exclusion",
            "must be false"
        ));
    }

    #[test]
    fn duplicate_suite_ids_are_rejected() {
        let issues = issues_of(&sample_with("id = \"web-unit\"", "id = \"api-unit\""));
        assert!(has_issue(&issues, "", "defined more than once"));
    }

    #[test]
    fn path_traversal_is_rejected_everywhere() {
        for (from, to, key) in [
            (
                "include = [\"apps/api/**/*.test.ts\", \"apps/api/**/*.spec.ts\"]",
                "include = [\"../outside/**/*.test.ts\"]",
                "include",
            ),
            (
                "inputs = [\"apps/api/**\", \"packages/**\"]",
                "inputs = [\"/etc/**\"]",
                "inputs",
            ),
            (
                "directory = \".testry\"",
                "directory = \"../state\"",
                "storage.directory",
            ),
            (
                "config = \".devcontainer/devcontainer.json\"",
                "config = \"../../devcontainer.json\"",
                "environments.colima.config",
            ),
            (
                "non_test_inputs = []",
                "non_test_inputs = [\"../docs/**\"]",
                "non_test_inputs",
            ),
        ] {
            let issues = issues_of(&sample_with(from, to));
            assert!(
                issues.iter().any(|issue| issue.to_string().contains(key)),
                "{key}: {issues:?}"
            );
        }
        let cwd = issues_of(&sample_with(
            "id = \"api-unit\"\nrunner = \"bun\"\ncwd = \".\"",
            "id = \"api-unit\"\nrunner = \"bun\"\ncwd = \"../elsewhere\"",
        ));
        assert!(has_issue(&cwd, "cwd", ".."));
    }

    #[test]
    fn budgets_and_reservations_must_be_positive_and_fit() {
        let zero = issues_of(&sample_with("cpu_slots = 4", "cpu_slots = 0"));
        assert!(has_issue(&zero, "execution", "cpu_slots"));

        let negative = parse_text(&sample_with("cpu_slots = 4", "cpu_slots = -1")).unwrap_err();
        assert!(matches!(negative, ConfigError::Parse(_)));

        let too_big = issues_of(&sample_with("memory_mib = 1536", "memory_mib = 999999"));
        assert!(has_issue(&too_big, "", "could never be scheduled"));

        let timeout = issues_of(&sample_with("timeout_seconds = 120", "timeout_seconds = 0"));
        assert!(has_issue(&timeout, "api-unit", "timeout"));
    }

    #[test]
    fn suite_rules_are_reported_with_the_suite_key() {
        let file_graph = issues_of(&sample_with(
            "inputs = [\"apps/**\", \"packages/**\", \"tests/e2e/**\"]\nselection = \"whole_suite\"",
            "inputs = [\"apps/**\", \"packages/**\", \"tests/e2e/**\"]\nselection = \"file_graph\"",
        ));
        assert!(has_issue(&file_graph, "e2e", "file_graph"));

        let launcher = issues_of(&sample_with(
            "command = [\"bun\", \"test\"]",
            "command = [\"bunx\", \"vitest\"]",
        ));
        assert!(has_issue(&launcher, "api-unit", "download"));

        let workers = issues_of(&sample_with(
            "project = \"unit\"\ninclude = [\"apps/web/**/*.test.ts\", \"apps/web/**/*.test.tsx\"]\ninputs = [\"apps/web/**\", \"packages/**\"]\nselection = \"file_graph\"\nalways_run = false\nworkers = 1",
            "project = \"unit\"\ninclude = [\"apps/web/**/*.test.ts\", \"apps/web/**/*.test.tsx\"]\ninputs = [\"apps/web/**\", \"packages/**\"]\nselection = \"file_graph\"\nalways_run = false\nworkers = 8",
        ));
        assert!(has_issue(&workers, "web-unit", "workers"));
    }

    #[test]
    fn a_configuration_needs_a_real_test_suite() {
        let only_gate = r#"
schema_version = 1
[analysis]
global_inputs = []
[execution]
default_environment = "local"
cpu_slots = 2
memory_budget_mib = 1024
[environments.local]
provider = "host"
[[suites]]
id = "typecheck"
runner = "command"
command = ["bun", "run", "typecheck"]
selection = "whole_suite"
[suites.resources]
cpu_slots = 1
memory_mib = 256
"#;
        assert!(has_issue(
            &issues_of(only_gate),
            "",
            "at least one test suite"
        ));

        let no_suites = only_gate.split("[[suites]]").next().unwrap();
        assert!(has_issue(&issues_of(no_suites), "suites", "at least one"));
    }

    #[test]
    fn minimal_configuration_gets_documented_defaults() {
        let minimal = r#"
schema_version = 1
[analysis]
global_inputs = ["**/package.json"]
[execution]
default_environment = "local"
cpu_slots = 2
memory_budget_mib = 1024
[environments.local]
provider = "host"
[[suites]]
id = "unit"
runner = "bun"
command = ["bun", "test"]
include = ["**/*.test.ts"]
selection = "file_graph"
[suites.resources]
cpu_slots = 1
memory_mib = 256
"#;
        let settings = parse_text(minimal).unwrap();
        assert_eq!(settings.budget().max_parallel_suites(), 1);
        assert_eq!(settings.default_timeout(), Duration::from_secs(600));
        assert_eq!(settings.suites()[0].timeout(), Duration::from_secs(600));
        assert_eq!(settings.storage_directory().as_str(), ".testry");
        assert!(settings.non_test_inputs().is_empty());
        assert!(settings.fail_on_flaky() && settings.fail_on_unexpected_skips());
        assert!(
            settings
                .exclude()
                .is_match(&RepoPath::try_new("node_modules/x/index.js").unwrap())
        );
        assert!(settings.suites()[0].known_skips().is_empty());
    }

    #[test]
    fn required_keys_have_no_hidden_default() {
        for line in [
            "global_inputs = [",
            "default_environment = \"local\"",
            "cpu_slots = 4",
            "memory_budget_mib = 4096",
            "selection = \"file_graph\"",
        ] {
            assert!(SAMPLE.contains(line), "sample does not contain {line:?}");
            let without: String = if line.ends_with('[') {
                // Drop the whole multi-line array.
                let start = SAMPLE.find(line).unwrap();
                let end = start + SAMPLE[start..].find("]\n").unwrap() + 2;
                format!("{}{}", &SAMPLE[..start], &SAMPLE[end..])
            } else {
                SAMPLE.replacen(line, "", 1)
            };
            let error = parse_text(&without).unwrap_err();
            assert!(matches!(error, ConfigError::Parse(_)), "{line}: {error}");
        }
    }

    #[test]
    fn known_skips_are_exact_records() {
        let with_skip = sample_with(
            "timeout_seconds = 120",
            "timeout_seconds = 120\nknown_skips = [{ file = \"apps/api/a.test.ts\", title_path = [\"suite\", \"case\"], expected_status = \"todo\" }]",
        );
        let settings = parse_text(&with_skip).unwrap();
        assert_eq!(settings.suites()[0].known_skips().len(), 1);

        let wildcard = sample_with(
            "timeout_seconds = 120",
            "timeout_seconds = 120\nknown_skips = [{ file = \"apps/api/a.test.ts\", title_path = [\"suite\"], expected_status = \"any\" }]",
        );
        assert!(matches!(
            parse_text(&wildcard).unwrap_err(),
            ConfigError::Parse(_)
        ));

        let traversal = sample_with(
            "timeout_seconds = 120",
            "timeout_seconds = 120\nknown_skips = [{ file = \"../a.test.ts\", title_path = [\"suite\"], expected_status = \"skipped\" }]",
        );
        assert!(has_issue(
            &issues_of(&traversal),
            "known_skips[0].file",
            ".."
        ));
    }

    #[test]
    fn load_resolves_the_workspace_root_and_config_path() {
        let directory = tempfile::tempdir().unwrap();
        let nested = directory.path().join("config");
        std::fs::create_dir(&nested).unwrap();
        let file = nested.join("testry.toml");
        std::fs::write(&file, sample_with("root = \".\"", "root = \"..\"")).unwrap();

        let loaded = load(&file).unwrap();
        assert_eq!(
            loaded.workspace_root,
            std::fs::canonicalize(directory.path()).unwrap()
        );
        assert_eq!(loaded.settings.config_path().as_str(), "config/testry.toml");
        assert!(
            loaded
                .settings
                .global_inputs()
                .is_match(&RepoPath::try_new("config/testry.toml").unwrap())
        );
    }

    #[test]
    fn load_reports_missing_files_and_escaping_roots() {
        let directory = tempfile::tempdir().unwrap();
        let missing = load(&directory.path().join("testry.toml")).unwrap_err();
        assert!(matches!(missing, ConfigError::Read { .. }));

        let file = directory.path().join("testry.toml");
        std::fs::write(&file, sample_with("root = \".\"", "root = \"/\"")).unwrap();
        assert!(matches!(
            load(&file).unwrap_err(),
            ConfigError::AbsoluteWorkspaceRoot { .. }
        ));

        let inner = directory.path().join("inner");
        std::fs::create_dir(&inner).unwrap();
        std::fs::write(&file, sample_with("root = \".\"", "root = \"inner\"")).unwrap();
        assert!(matches!(
            load(&file).unwrap_err(),
            ConfigError::OutsideWorkspace { .. }
        ));
    }

    #[test]
    fn the_digest_follows_the_file_bytes() {
        let a = parse_text(SAMPLE).unwrap();
        let b = parse_text(&format!("{SAMPLE}\n# a comment\n")).unwrap();
        assert_ne!(a.config_digest(), b.config_digest());
    }
}
