//! Typed port errors (spec section 5).
//!
//! Errors are classified by variant, never by parsing message text. A test
//! assertion failure is not an error; it is a `TaskOutcome::Failed`.

use testry_domain::results::ExitCondition;

use crate::commands::create_plan::PlanningError;

/// Why the source of a workspace could not be read. The distinction decides
/// the exit code: a missing base is a usage problem, a crashing `git` is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceUnavailableReason {
    /// The workspace root is not the top level of a Git work tree.
    NotARepository,
    /// The requested base ref does not resolve to a commit in this clone.
    BaseNotFound,
    /// The base and `HEAD` have no common ancestor (for example a shallow
    /// clone).
    NoMergeBase,
    /// `HEAD` does not point to a commit yet.
    UnbornHead,
    /// The index contains unmerged entries.
    UnmergedIndex,
    /// A path is not valid UTF-8.
    NonUtf8Path,
    /// A path reported by Git is not a valid repository-relative path.
    InvalidPath,
    /// `git` could not be started.
    GitUnavailable,
    /// `git` failed or printed something unexpected.
    GitFailed,
    /// A file of the workspace could not be read.
    ReadFailed,
}

impl SourceUnavailableReason {
    pub fn code(self) -> &'static str {
        match self {
            Self::NotARepository => "not_a_repository",
            Self::BaseNotFound => "base_not_found",
            Self::NoMergeBase => "no_merge_base",
            Self::UnbornHead => "unborn_head",
            Self::UnmergedIndex => "unmerged_index",
            Self::NonUtf8Path => "non_utf8_path",
            Self::InvalidPath => "invalid_path",
            Self::GitUnavailable => "git_unavailable",
            Self::GitFailed => "git_failed",
            Self::ReadFailed => "read_failed",
        }
    }

    /// Whether the user can fix this by changing the request or the
    /// repository state (exit code 2) rather than the infrastructure (3).
    fn is_usage_problem(self) -> bool {
        !matches!(
            self,
            Self::GitUnavailable | Self::GitFailed | Self::ReadFailed
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PortError {
    #[error("configuration error: {message}")]
    Configuration { message: String },
    #[error("workspace trust is required: {message}")]
    TrustRequired { message: String },
    #[error("source unavailable ({}): {message}", reason.code())]
    SourceUnavailable {
        reason: SourceUnavailableReason,
        message: String,
    },
    #[error("unsupported capability `{capability}`: {message}")]
    UnsupportedCapability { capability: String, message: String },
    #[error("planning failed: {0}")]
    Planning(#[from] PlanningError),
    #[error("could not start `{program}`: {message}")]
    Spawn { program: String, message: String },
    #[error("timed out: {operation}")]
    Timeout { operation: String },
    #[error("cancelled")]
    Cancelled,
    #[error("invalid report: {message}")]
    InvalidReport { message: String },
    #[error("storage error: {message}")]
    Storage { message: String },
    #[error("inputs changed: {message}")]
    StaleInputs { message: String },
    #[error("the environment is not ready: {message}")]
    EnvironmentNotReady { message: String },
    #[error("the workspace is in use: {message}")]
    WorkspaceBusy { message: String },
    #[error("cleanup failed: {message}")]
    CleanupFailed {
        message: String,
        residual_resources: Vec<String>,
    },
}

impl PortError {
    /// Stable machine-readable code for JSON output.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Configuration { .. } => "configuration",
            Self::TrustRequired { .. } => "trust_required",
            Self::SourceUnavailable { .. } => "source_unavailable",
            Self::UnsupportedCapability { .. } => "unsupported_capability",
            Self::Planning(_) => "planning",
            Self::Spawn { .. } => "spawn",
            Self::Timeout { .. } => "timeout",
            Self::Cancelled => "cancelled",
            Self::InvalidReport { .. } => "invalid_report",
            Self::Storage { .. } => "storage",
            Self::StaleInputs { .. } => "stale_inputs",
            Self::EnvironmentNotReady { .. } => "environment_not_ready",
            Self::WorkspaceBusy { .. } => "workspace_busy",
            Self::CleanupFailed { .. } => "cleanup_failed",
        }
    }

    pub fn exit_condition(&self) -> ExitCondition {
        match self {
            Self::Configuration { .. }
            | Self::TrustRequired { .. }
            | Self::UnsupportedCapability { .. }
            | Self::Planning(_) => ExitCondition::Configuration,
            Self::SourceUnavailable { reason, .. } => {
                if reason.is_usage_problem() {
                    ExitCondition::Configuration
                } else {
                    ExitCondition::Infrastructure
                }
            }
            Self::Spawn { .. }
            | Self::Timeout { .. }
            | Self::InvalidReport { .. }
            | Self::Storage { .. }
            | Self::StaleInputs { .. }
            | Self::EnvironmentNotReady { .. }
            | Self::WorkspaceBusy { .. }
            | Self::CleanupFailed { .. } => ExitCondition::Infrastructure,
            Self::Cancelled => ExitCondition::UserCancelled,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(reason: SourceUnavailableReason) -> PortError {
        PortError::SourceUnavailable {
            reason,
            message: "m".to_owned(),
        }
    }

    #[test]
    fn exit_codes_follow_the_cli_contract() {
        let message = || "m".to_owned();
        let cases = [
            (PortError::Configuration { message: message() }, 2),
            (PortError::TrustRequired { message: message() }, 2),
            (
                PortError::UnsupportedCapability {
                    capability: "c".to_owned(),
                    message: message(),
                },
                2,
            ),
            (
                PortError::Planning(PlanningError::CiRequiresFullVerification),
                2,
            ),
            (source(SourceUnavailableReason::BaseNotFound), 2),
            (source(SourceUnavailableReason::NoMergeBase), 2),
            (source(SourceUnavailableReason::UnmergedIndex), 2),
            (source(SourceUnavailableReason::NonUtf8Path), 2),
            (source(SourceUnavailableReason::GitFailed), 3),
            (source(SourceUnavailableReason::GitUnavailable), 3),
            (
                PortError::Spawn {
                    program: "bun".to_owned(),
                    message: message(),
                },
                3,
            ),
            (
                PortError::Timeout {
                    operation: "o".to_owned(),
                },
                3,
            ),
            (PortError::InvalidReport { message: message() }, 3),
            (PortError::Storage { message: message() }, 3),
            (PortError::StaleInputs { message: message() }, 3),
            (
                PortError::CleanupFailed {
                    message: message(),
                    residual_resources: Vec::new(),
                },
                3,
            ),
            (PortError::Cancelled, 130),
        ];
        for (error, expected) in cases {
            assert_eq!(error.exit_condition().exit_code(), expected, "{error}");
        }
    }

    #[test]
    fn every_error_has_a_distinct_machine_code() {
        let message = || "m".to_owned();
        let errors = [
            PortError::Configuration { message: message() },
            PortError::TrustRequired { message: message() },
            source(SourceUnavailableReason::GitFailed),
            PortError::UnsupportedCapability {
                capability: "c".to_owned(),
                message: message(),
            },
            PortError::Planning(PlanningError::CiRejectsScopeLimit),
            PortError::Spawn {
                program: "p".to_owned(),
                message: message(),
            },
            PortError::Timeout {
                operation: "o".to_owned(),
            },
            PortError::Cancelled,
            PortError::InvalidReport { message: message() },
            PortError::Storage { message: message() },
            PortError::StaleInputs { message: message() },
            PortError::CleanupFailed {
                message: message(),
                residual_resources: Vec::new(),
            },
        ];
        let codes: std::collections::BTreeSet<&str> = errors.iter().map(PortError::code).collect();
        assert_eq!(codes.len(), errors.len());
    }
}
