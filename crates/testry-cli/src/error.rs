//! The error shape of the command line: a machine-readable code, an exit
//! condition and a message that never contains secrets or raw environment
//! variables.

use serde::Serialize;
use testry_application::ports::PortError;
use testry_domain::results::ExitCondition;

use crate::config::ConfigError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    pub code: &'static str,
    pub condition: ExitCondition,
    pub message: String,
}

#[derive(Serialize)]
struct ErrorDocument<'a> {
    schema_version: u32,
    error: ErrorBody<'a>,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    code: &'a str,
    message: &'a str,
}

impl CliError {
    pub fn configuration(message: impl Into<String>) -> Self {
        Self {
            code: "configuration",
            condition: ExitCondition::Configuration,
            message: message.into(),
        }
    }

    pub fn storage(message: impl Into<String>) -> Self {
        Self {
            code: "storage",
            condition: ExitCondition::Infrastructure,
            message: message.into(),
        }
    }

    /// Something the configuration contract can express but this tool does
    /// not do, by decision. It fails loudly; it never reports success.
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self {
            code: "unsupported_capability",
            condition: ExitCondition::Configuration,
            message: message.into(),
        }
    }

    pub fn exit_code(&self) -> u8 {
        self.condition.exit_code()
    }

    /// The single JSON object printed on stdout for `--format json`.
    pub fn to_json(&self) -> String {
        let document = ErrorDocument {
            schema_version: 1,
            error: ErrorBody {
                code: self.code,
                message: &self.message,
            },
        };
        serde_json::to_string(&document).unwrap_or_else(|_| {
            r#"{"schema_version":1,"error":{"code":"internal","message":"error could not be serialized"}}"#
                .to_owned()
        })
    }
}

impl From<ConfigError> for CliError {
    fn from(error: ConfigError) -> Self {
        Self::configuration(error.to_string())
    }
}

impl From<PortError> for CliError {
    fn from(error: PortError) -> Self {
        Self {
            code: error.code(),
            condition: error.exit_condition(),
            message: error.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_not_supported_is_never_a_success() {
        let error = CliError::unsupported("the Dev Container provider is not implemented");
        assert_eq!(error.exit_code(), 2);
        assert_eq!(error.code, "unsupported_capability");
    }

    #[test]
    fn json_form_has_a_code_and_a_message() {
        let error = CliError::configuration("bad \"value\"");
        let value: serde_json::Value = serde_json::from_str(&error.to_json()).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["error"]["code"], "configuration");
        assert_eq!(value["error"]["message"], "bad \"value\"");
    }

    #[test]
    fn port_errors_keep_code_and_exit_condition() {
        let error: CliError = PortError::StaleInputs {
            message: "changed".to_owned(),
        }
        .into();
        assert_eq!(error.code, "stale_inputs");
        assert_eq!(error.exit_code(), 3);
        let cancelled: CliError = PortError::Cancelled.into();
        assert_eq!(cancelled.exit_code(), 130);
    }
}
