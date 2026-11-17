//! The `host` provider: commands run as child processes of testry itself
//! (spec sections 10 and 11).
//!
//! Under the container-first policy of this project (ADR-0002) testry runs
//! inside a container, so "host" means the container testry is in. The
//! provider refuses to start anything when no container marker is present,
//! unless that is explicitly allowed.
//!
//! This provider offers no isolation: resource reservations are scheduling
//! budgets, and a process that leaves its process group is not contained.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use testry_application::ports::execution::{
    CleanupContext, CleanupReport, CommandSpec, EnvironmentLease, EnvironmentRequest, EventSink,
    ExecutionDescriptor, ProcessNotes, ProcessResult, ReleaseDisposition, RuntimeProvider,
    WorkspaceMapping,
};
use testry_application::ports::{OperationContext, PortError, PortFuture};
use testry_domain::environment::{LeaseState, ProviderKind};

use crate::process::unix::{ProcessSpec, run};

/// Environment variables a command may inherit from testry. Everything else,
/// in particular credentials, is dropped. `CI` is inherited because runner
/// configurations commonly depend on it (retries, reuse of a running server);
/// dropping it would make a run in CI behave like a local one.
pub const INHERITED_VARIABLES: [&str; 9] = [
    "PATH", "HOME", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE", "TZ", "TERM", "CI",
];

/// Variables every command gets, whatever the caller has set. The allowlist
/// above would drop an opt-out of usage telemetry the user has set, so the
/// common opt-out is always given. An adapter may add tool-specific ones.
pub const DEFAULT_VARIABLES: [(&str, &str); 1] = [("DO_NOT_TRACK", "1")];

/// Whether the process is inside a container, judged by the usual markers.
pub fn container_marker_present() -> bool {
    Path::new("/.dockerenv").exists() || Path::new("/run/.containerenv").exists()
}

pub struct HostRuntime {
    /// The artifact directory of the run. Log artifacts are relative to it.
    artifact_root: PathBuf,
    grace: Duration,
    environment: BTreeMap<String, String>,
    in_container: bool,
    allow_outside_container: bool,
    /// Process groups that could not be confirmed gone.
    residual: Mutex<Vec<String>>,
}

impl HostRuntime {
    /// `environment` is the complete base environment of every command;
    /// build it with [`Self::inherited_environment`].
    pub fn new(
        artifact_root: PathBuf,
        grace: Duration,
        environment: BTreeMap<String, String>,
        in_container: bool,
        allow_outside_container: bool,
    ) -> Self {
        Self {
            artifact_root,
            grace,
            environment,
            in_container,
            allow_outside_container,
            residual: Mutex::new(Vec::new()),
        }
    }

    /// Picks the allowlisted variables out of an environment.
    pub fn inherited_environment(
        variables: impl IntoIterator<Item = (String, String)>,
    ) -> BTreeMap<String, String> {
        variables
            .into_iter()
            .filter(|(name, _)| INHERITED_VARIABLES.contains(&name.as_str()))
            .collect()
    }

    fn ensure_parent(&self, relative: &Path) -> Result<PathBuf, PortError> {
        let path = self.artifact_root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| PortError::Storage {
                message: format!("cannot create {}: {}", parent.display(), error.kind()),
            })?;
        }
        Ok(path)
    }

    async fn run_command(
        &self,
        owner: &str,
        workspace_root: &Path,
        command: &CommandSpec,
        ctx: &OperationContext,
    ) -> Result<ProcessResult, PortError> {
        let mut env: BTreeMap<String, String> = DEFAULT_VARIABLES
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        env.extend(self.environment.clone());
        env.extend(command.env.clone());
        let cwd = match command.cwd.as_path() {
            Some(directory) => workspace_root.join(directory.as_str()),
            None => workspace_root.to_path_buf(),
        };
        let spec = ProcessSpec {
            program: command.program.clone(),
            args: command.args.clone(),
            owner: owner.to_owned(),
            cwd,
            env,
            timeout: command.timeout,
            grace: self.grace,
            stdout_path: self.ensure_parent(&command.output.stdout.relative_path)?,
            stderr_path: self.ensure_parent(&command.output.stderr.relative_path)?,
            max_bytes_per_stream: command.output.max_bytes_per_stream,
        };
        let outcome = run(&spec, ctx.cancellation.as_ref()).await?;
        if outcome.residual_group {
            let mut residual = self
                .residual
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            residual.push(format!("process group of `{}`", command.program));
        }
        Ok(ProcessResult {
            exit: outcome.exit,
            started_at: outcome.started_at,
            finished_at: outcome.finished_at,
            duration: outcome.duration,
            stdout: command.output.stdout.clone(),
            stderr: command.output.stderr.clone(),
            notes: ProcessNotes {
                stdout_truncated: outcome.stdout_truncated,
                stderr_truncated: outcome.stderr_truncated,
                stopped_survivors: outcome.stopped_survivors,
                residual_processes: outcome.residual_group,
                output_incomplete: outcome.output_incomplete,
            },
        })
    }
}

impl RuntimeProvider for HostRuntime {
    fn prepare<'a>(
        &'a self,
        request: &'a EnvironmentRequest,
        ctx: &'a OperationContext,
    ) -> PortFuture<'a, EnvironmentLease> {
        Box::pin(async move {
            ctx.ensure_active("preparing the environment")?;
            if request.provider != ProviderKind::Host {
                return Err(PortError::UnsupportedCapability {
                    capability: format!("provider:{}", request.provider.as_str()),
                    message: "this build can only execute with the host provider".to_owned(),
                });
            }
            if !self.in_container && !self.allow_outside_container {
                return Err(PortError::UnsupportedCapability {
                    capability: "execution_outside_container".to_owned(),
                    message: "testry does not appear to run inside a container, and this project \
                              runs tests in containers only. Run testry inside the project's \
                              container, or set TESTRY_ALLOW_EXECUTION_OUTSIDE_CONTAINER=1 to accept \
                              unverified execution on this machine"
                        .to_owned(),
                });
            }
            let workspace_root =
                std::fs::canonicalize(&request.workspace_root).map_err(|error| {
                    PortError::EnvironmentNotReady {
                        message: format!(
                            "the workspace {} cannot be resolved: {}",
                            request.workspace_root.display(),
                            error.kind()
                        ),
                    }
                })?;
            Ok(EnvironmentLease {
                lease_id: request.ownership.lease_id.clone(),
                owner_run_id: request.ownership.run_id.clone(),
                provider: ProviderKind::Host,
                // The host provider creates nothing that outlives its commands.
                owned_resources: Vec::new(),
                descriptor: ExecutionDescriptor {
                    provider: ProviderKind::Host,
                    os: std::env::consts::OS.to_owned(),
                    arch: std::env::consts::ARCH.to_owned(),
                    tool_versions: BTreeMap::new(),
                    workspace: WorkspaceMapping {
                        local_root: workspace_root.clone(),
                        execution_root: workspace_root,
                    },
                    image: None,
                    capabilities: BTreeSet::from(["process_group".to_owned()]),
                },
                state: LeaseState::Ready,
            })
        })
    }

    fn execute<'a>(
        &'a self,
        lease: &'a EnvironmentLease,
        command: &'a CommandSpec,
        _events: &'a dyn EventSink,
        ctx: &'a OperationContext,
    ) -> PortFuture<'a, ProcessResult> {
        Box::pin(self.run_command(
            lease.owner_run_id.as_str(),
            &lease.descriptor.workspace.execution_root,
            command,
            ctx,
        ))
    }

    fn release<'a>(
        &'a self,
        _lease: EnvironmentLease,
        _disposition: ReleaseDisposition,
        _cleanup: &'a CleanupContext,
    ) -> PortFuture<'a, CleanupReport> {
        // Each command already stopped its process group. What could not be
        // confirmed gone is reported, so the run is not a complete success.
        let residual = self
            .residual
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Box::pin(async move {
            Ok(CleanupReport {
                residual_resources: residual,
                ..CleanupReport::default()
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use testry_application::ports::execution::LeaseOwnership;
    use testry_application::testing::{cancelled_context, context};
    use testry_domain::fingerprint::Fingerprint;
    use testry_domain::ids::{EnvironmentLeaseId, RunId, WorkspaceId};

    use super::*;

    fn runtime(in_container: bool, allow_outside_container: bool) -> HostRuntime {
        HostRuntime::new(
            std::env::temp_dir(),
            Duration::from_secs(1),
            BTreeMap::new(),
            in_container,
            allow_outside_container,
        )
    }

    fn request(provider: ProviderKind) -> EnvironmentRequest {
        EnvironmentRequest {
            provider,
            workspace_root: std::env::temp_dir(),
            config_fingerprint: Fingerprint::of_bytes(b"config"),
            ownership: LeaseOwnership {
                workspace_id: WorkspaceId::try_new("ws-fixture").unwrap(),
                run_id: RunId::try_new("00000000-0000-4000-8000-000000000000").unwrap(),
                lease_id: EnvironmentLeaseId::try_new("11111111-1111-4111-8111-111111111111")
                    .unwrap(),
            },
            readiness_commands: Vec::new(),
            trust: None,
        }
    }

    /// ADR-0002: without a container marker nothing is prepared, so no
    /// command of the configuration can start.
    #[tokio::test]
    async fn execution_outside_a_container_is_refused_by_default() {
        let error = runtime(false, false)
            .prepare(&request(ProviderKind::Host), &context())
            .await
            .unwrap_err();
        match error {
            PortError::UnsupportedCapability {
                capability,
                message,
            } => {
                assert_eq!(capability, "execution_outside_container");
                assert!(
                    message.contains("TESTRY_ALLOW_EXECUTION_OUTSIDE_CONTAINER=1"),
                    "{message}"
                );
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn execution_outside_a_container_needs_the_explicit_opt_out() {
        let lease = runtime(false, true)
            .prepare(&request(ProviderKind::Host), &context())
            .await
            .unwrap();
        assert_eq!(lease.state, LeaseState::Ready);
    }

    #[tokio::test]
    async fn a_lease_inside_a_container_owns_nothing_and_maps_the_workspace_to_itself() {
        let lease = runtime(true, false)
            .prepare(&request(ProviderKind::Host), &context())
            .await
            .unwrap();
        assert_eq!(lease.provider, ProviderKind::Host);
        assert_eq!(lease.state, LeaseState::Ready);
        assert!(lease.owned_resources.is_empty());
        assert_eq!(
            lease.descriptor.workspace.local_root,
            lease.descriptor.workspace.execution_root
        );
        assert!(lease.descriptor.workspace.execution_root.is_absolute());
        assert!(
            lease.descriptor.tool_versions.is_empty(),
            "versions are probed, never assumed"
        );
    }

    #[tokio::test]
    async fn another_provider_is_not_silently_replaced_by_the_host() {
        let error = runtime(true, false)
            .prepare(&request(ProviderKind::DevcontainerCli), &context())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, PortError::UnsupportedCapability { capability, .. } if capability == "provider:devcontainer_cli"),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn a_cancelled_run_prepares_nothing() {
        let error = runtime(true, false)
            .prepare(&request(ProviderKind::Host), &cancelled_context())
            .await
            .unwrap_err();
        assert!(matches!(error, PortError::Cancelled), "{error:?}");
    }

    #[tokio::test]
    async fn a_missing_workspace_is_an_environment_problem() {
        let mut missing = request(ProviderKind::Host);
        missing.workspace_root = std::env::temp_dir().join("testry-no-such-workspace-7f3a");
        let error = runtime(true, false)
            .prepare(&missing, &context())
            .await
            .unwrap_err();
        assert!(
            matches!(error, PortError::EnvironmentNotReady { .. }),
            "{error:?}"
        );
    }

    /// Credentials and other variables of the caller never reach a runner.
    #[test]
    fn only_allowlisted_variables_are_inherited() {
        let inherited = HostRuntime::inherited_environment([
            ("PATH".to_owned(), "/usr/bin".to_owned()),
            ("HOME".to_owned(), "/home/dev".to_owned()),
            ("AWS_SECRET_ACCESS_KEY".to_owned(), "secret".to_owned()),
            ("GITHUB_TOKEN".to_owned(), "secret".to_owned()),
            ("CI".to_owned(), "true".to_owned()),
            ("NODE_OPTIONS".to_owned(), "--require ./x.js".to_owned()),
            (
                "path".to_owned(),
                "lowercase is another variable".to_owned(),
            ),
        ]);
        assert_eq!(
            inherited,
            BTreeMap::from([
                ("CI".to_owned(), "true".to_owned()),
                ("HOME".to_owned(), "/home/dev".to_owned()),
                ("PATH".to_owned(), "/usr/bin".to_owned()),
            ])
        );
    }
}
