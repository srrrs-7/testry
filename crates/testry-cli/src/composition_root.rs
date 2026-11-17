//! Wires the concrete adapters to the application ports. This is the only
//! place that knows both sides.

use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use testry_adapters::analysis::oxc::OxcDependencyAnalyzer;
use testry_adapters::catalog::static_files::StaticTestCatalog;
use testry_adapters::runners::RunnerRegistry;
use testry_adapters::runtime::host::{HostRuntime, container_marker_present};
use testry_adapters::source::git::GitSourceRepository;
use testry_adapters::source::reader::GitWorkspaceReader;
use testry_adapters::storage::history::HistoryDatabase;
use testry_adapters::storage::lock::WorkspaceLock;
use testry_adapters::storage::plan_files::PlanFileStore;
use testry_adapters::storage::run_files::{RunDirectory, reconcile_history};
use testry_adapters::storage::trust::{
    TRUSTED_DIGEST_VARIABLE, TrustStore, approval_from_environment,
};
use testry_application::commands::execute_plan::ExecutePlan;
use testry_application::commands::plan_workspace::PlanWorkspace;
use testry_application::ports::execution::TrustApproval;
use testry_application::ports::{Cancellation, NeverCancelled, OperationContext};
use testry_application::settings::Settings;
use testry_domain::ids::{EnvironmentLeaseId, RunId};
use tokio::sync::Notify;

use crate::error::CliError;

/// Accepts execution although no container marker is present (ADR-0002).
pub const ALLOW_OUTSIDE_CONTAINER_VARIABLE: &str = "TESTRY_ALLOW_EXECUTION_OUTSIDE_CONTAINER";

/// The adapters needed to create a plan for one workspace.
pub struct Planning {
    pub source: GitSourceRepository,
    pub analyzer: OxcDependencyAnalyzer,
    pub catalog: StaticTestCatalog,
    pub plans: PlanFileStore,
}

impl Planning {
    pub fn new(workspace_root: &Path, settings: &Settings) -> Self {
        Self {
            source: GitSourceRepository::new(),
            analyzer: OxcDependencyAnalyzer::new(Arc::new(GitWorkspaceReader::new(workspace_root))),
            catalog: StaticTestCatalog,
            plans: PlanFileStore::new(workspace_root, settings.storage_directory()),
        }
    }

    pub fn use_case<'a>(&'a self, settings: &'a Settings) -> PlanWorkspace<'a> {
        PlanWorkspace {
            settings,
            source: &self.source,
            analyzer: &self.analyzer,
            catalog: &self.catalog,
        }
    }
}

fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub fn new_run_id() -> Result<RunId, CliError> {
    RunId::try_new(new_uuid())
        .map_err(|error| CliError::storage(format!("could not create a run id: {error}")))
}

pub fn new_lease_id() -> Result<EnvironmentLeaseId, CliError> {
    EnvironmentLeaseId::try_new(new_uuid())
        .map_err(|error| CliError::storage(format!("could not create a lease id: {error}")))
}

/// A context for one invocation that cannot be cancelled. Every invocation
/// gets its own ID; a plan never contains it.
pub fn operation_context() -> Result<OperationContext, CliError> {
    Ok(OperationContext {
        run_id: new_run_id()?,
        cancellation: Arc::new(NeverCancelled),
        deadline: None,
    })
}

/// Cancellation driven by SIGINT and SIGTERM.
#[derive(Default)]
pub struct SignalCancellation {
    cancelled: AtomicBool,
    notify: Notify,
}

impl SignalCancellation {
    /// Starts listening. The first signal requests cancellation: no new suite
    /// starts, running process groups are stopped, cleanup still runs.
    pub fn install() -> Arc<Self> {
        let cancellation = Arc::new(Self::default());
        let listener = Arc::clone(&cancellation);
        tokio::spawn(async move {
            let interrupt = tokio::signal::ctrl_c();
            let terminate = async {
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                    Ok(mut signal) => {
                        signal.recv().await;
                    }
                    Err(_) => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                _ = interrupt => {}
                () = terminate => {}
            }
            listener.cancelled.store(true, Ordering::SeqCst);
            listener.notify.notify_waiters();
            eprintln!("testry: cancellation requested; stopping running suites and cleaning up");
        });
        cancellation
    }
}

impl Cancellation for SignalCancellation {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    fn cancelled<'a>(&'a self) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            loop {
                // Register before checking, so a notification between the
                // check and the wait is not lost.
                let notified = self.notify.notified();
                if self.is_cancelled() {
                    return;
                }
                notified.await;
            }
        })
    }
}

/// The adapters of one run: its artifact directory, the runtime and the
/// runner adapters. The workspace lock is held as long as this value lives.
pub struct Execution {
    pub planning: Planning,
    pub run: RunDirectory,
    pub runtime: HostRuntime,
    pub runners: RunnerRegistry,
    _lock: WorkspaceLock,
}

impl Execution {
    pub fn new(
        workspace_root: &Path,
        settings: &Settings,
        run_id: &RunId,
        echo_events: bool,
    ) -> Result<Self, CliError> {
        // The lock comes first: nothing of a second run is created while
        // another run is active in this workspace.
        let lock = WorkspaceLock::acquire(workspace_root, settings.storage_directory(), run_id)?;

        // With the lock held no other run is active, so every run the index
        // still calls running stopped without its terminal record. This is
        // start-up work on the calling thread; no task runs yet.
        let history = HistoryDatabase::open(workspace_root, settings.storage_directory())?;
        let repaired =
            reconcile_history(workspace_root, settings.storage_directory(), &history, None)?;
        if !repaired.interrupted.is_empty() {
            eprintln!(
                "testry: {} earlier run(s) stopped without a result and are recorded as \
                 interrupted: {}. `testry gc --dry-run` shows what they left behind",
                repaired.interrupted.len(),
                repaired.interrupted.join(", ")
            );
        }

        let run = RunDirectory::create(
            workspace_root,
            settings.storage_directory(),
            run_id,
            echo_events,
        )?
        .with_history(history);
        let allow_outside =
            std::env::var(ALLOW_OUTSIDE_CONTAINER_VARIABLE).is_ok_and(|value| value == "1");
        let runtime = HostRuntime::new(
            run.root().to_path_buf(),
            settings.termination_grace(),
            HostRuntime::inherited_environment(std::env::vars()),
            container_marker_present(),
            allow_outside,
        );
        Ok(Self {
            planning: Planning::new(workspace_root, settings),
            run,
            runtime,
            runners: RunnerRegistry::new(),
            _lock: lock,
        })
    }

    pub fn use_case<'a>(&'a self, settings: &'a Settings) -> ExecutePlan<'a> {
        ExecutePlan {
            planning: self.planning.use_case(settings),
            runners: self.runners.adapters(),
            runtime: &self.runtime,
            artifacts: &self.run,
            events: &self.run,
            store: &self.run,
        }
    }
}

/// The trust decision for a workspace: a digest pinned by a trusted job
/// definition through the environment, or the approval stored for the user.
pub fn trust_approval(workspace_root: &Path) -> Result<Option<TrustApproval>, CliError> {
    let pinned = std::env::var(TRUSTED_DIGEST_VARIABLE).ok();
    if let Some(approval) = approval_from_environment(pinned.as_deref())? {
        return Ok(Some(approval));
    }
    Ok(trust_store()?.approval(workspace_root)?)
}

pub fn trust_store() -> Result<TrustStore, CliError> {
    Ok(TrustStore::locate(|name| std::env::var(name).ok())?)
}
