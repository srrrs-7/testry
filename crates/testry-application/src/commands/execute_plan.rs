//! The execution use case: plan, prepare the environment, verify the runner
//! capabilities, run the suites within the resource budget, clean up and
//! aggregate (spec sections 8 to 11 and 13).
//!
//! Order of events, and what each step guarantees:
//!
//! 1. The request is validated and planned. Nothing is executed for this.
//! 2. Workspace trust is checked before the first command runs.
//! 3. The environment is prepared. A failure here is an environment failure,
//!    never a test failure, and no test starts.
//! 4. Each runner version is probed. A runner whose file filter is not
//!    verified for that version runs its whole suite.
//! 5. The inputs are fingerprinted again. If they differ from the plan, the
//!    run is invalidated before any test runs.
//! 6. Suites run under the scheduler. After a cancellation or a fail-fast
//!    failure no new suite starts.
//! 7. The inputs are fingerprinted once more. A change invalidates the run.
//! 8. The environment is released under its own deadline, also after a
//!    cancellation. A failed cleanup prevents a passed run.
//! 9. The terminal result is stored. Without it a run counts as interrupted.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use testry_domain::environment::ProviderKind;
use testry_domain::ids::{EnvironmentLeaseId, TaskId};
use testry_domain::inventory::TestInventory;
use testry_domain::plan::{ExecutionPlan, SuitePlan};
use testry_domain::results::{RunOutcome, TaskOutcome};
use testry_domain::selection::Selection;
use testry_domain::suite::{RunnerKind, SuiteDefinition};

use super::create_plan::{PlanningError, apply_runner_capabilities, ensure_plan_is_current};
use super::plan_workspace::{PlanRequest, PlanWorkspace};
use crate::policy::{ResultPolicy, apply_result_policy};
use crate::ports::execution::{
    AdditionalFiles, Anomaly, ArtifactStore, CleanupContext, CleanupReport, CommandSpec,
    EnvironmentLease, EnvironmentRequest, EventSink, ExecutionDescriptor, InputValidation,
    LeaseOwnership, OutputPolicy, ReleaseDisposition, ReportBundle, ReportContent, ReportFormat,
    RunEvent, RunEventPayload, RunMetrics, RunRecord, RunResult, RunStore, RunnerAdapter,
    RunnerCapabilities, RuntimeProvider, SuiteRunResult, TaskArtifacts, ToolVersion, TrustApproval,
};
use crate::ports::{OperationContext, PortError};
use crate::results::{
    achieved_assurance, aggregate_run_outcome, anomaly, exit_code_of, is_failure, shadow_gaps,
    unstarted_suite_result,
};
use crate::scheduler::{NotStarted, Scheduled, ScheduledTask, SchedulingPolicy, run_scheduled};

/// How long a runner may take to print its version.
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const VERSION_PROBE_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRequest {
    pub plan: PlanRequest,
    pub provider: ProviderKind,
    /// argv arrays that must succeed in the environment before any test.
    pub readiness_commands: Vec<Vec<String>>,
    pub lease_id: EnvironmentLeaseId,
    /// The trust decision for this workspace, if there is one. It comes from
    /// outside the repository.
    pub trust: Option<TrustApproval>,
}

#[derive(Debug, Clone)]
pub struct RunReport {
    pub plan: ExecutionPlan,
    pub result: RunResult,
}

pub struct ExecutePlan<'a> {
    pub planning: PlanWorkspace<'a>,
    pub runners: BTreeMap<RunnerKind, &'a dyn RunnerAdapter>,
    pub runtime: &'a dyn RuntimeProvider,
    pub artifacts: &'a dyn ArtifactStore,
    pub events: &'a dyn EventSink,
    pub store: &'a dyn RunStore,
}

/// One suite that will run.
struct Work<'a> {
    suite: &'a SuiteDefinition,
    plan: SuitePlan,
    capabilities: RunnerCapabilities,
    artifacts: TaskArtifacts,
}

#[derive(Default)]
struct Progress {
    suites: Vec<SuiteRunResult>,
    environment: Option<ExecutionDescriptor>,
    input_validation: Option<InputValidation>,
    cleanup: CleanupReport,
    cancelled: bool,
    environment_ready: Duration,
    verification: Duration,
    cleanup_time: Duration,
    first_failure: Option<Duration>,
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn report_file_name(runner: RunnerKind) -> &'static str {
    match runner {
        RunnerKind::Bun => "report.xml",
        RunnerKind::Vitest | RunnerKind::StorybookVitest | RunnerKind::Playwright => "report.json",
        RunnerKind::Command => "report.none",
    }
}

fn selection_kind(selection: &Selection) -> &'static str {
    match selection {
        Selection::All => "all",
        Selection::Files { .. } => "files",
        Selection::Omit { .. } => "omit",
    }
}

impl ExecutePlan<'_> {
    pub async fn execute(
        &self,
        workspace_root: &Path,
        request: &RunRequest,
        ctx: &OperationContext,
    ) -> Result<RunReport, PortError> {
        let started = Instant::now();
        let settings = self.planning.settings;

        let planned = self
            .planning
            .execute(workspace_root, &request.plan, ctx)
            .await?;
        let plan = planned.plan;
        let inventory = planned.inventory;

        // Nothing below runs a command before these two checks passed.
        self.ensure_runners_exist(&plan)?;
        if plan.has_work() {
            ensure_trusted(request.trust.as_ref(), settings.config_digest())?;
        }

        self.emit(ctx, RunEventPayload::RunStarted { mode: plan.mode() })
            .await?;
        self.emit(
            ctx,
            RunEventPayload::PlanCreated {
                plan_id: *plan.id(),
            },
        )
        .await?;
        self.store
            .create(&RunRecord {
                run_id: ctx.run_id.clone(),
                plan_id: *plan.id(),
                plan: plan.clone(),
                started_at: SystemTime::now(),
                mode: plan.mode(),
                scope: plan.body().scope.clone(),
                workspace_id: plan.body().workspace_id.clone(),
                lease_id: request.lease_id.clone(),
                provider: request.provider,
            })
            .await?;

        let mut progress = Progress::default();
        let error = if plan.has_work() {
            self.run(
                workspace_root,
                request,
                ctx,
                &plan,
                &inventory,
                started,
                &mut progress,
            )
            .await
            .err()
        } else {
            None
        };

        let cancelled = progress.cancelled
            || ctx.cancellation.is_cancelled()
            || matches!(error, Some(PortError::Cancelled));
        let input_validation = match (&error, progress.input_validation) {
            (Some(PortError::StaleInputs { .. }), _) => InputValidation::Changed,
            (_, Some(validation)) => validation,
            (_, None) => InputValidation::NotChecked,
        };
        let mut outcome = aggregate_run_outcome(
            &progress.suites,
            cancelled,
            input_validation,
            &progress.cleanup,
        );
        let mut exit_code = exit_code_of(outcome);
        // An error that is neither a cancellation nor changed inputs means the
        // run did not complete. It can never be reported as passed or no work.
        let fatal = error
            .as_ref()
            .filter(|error| !matches!(error, PortError::Cancelled | PortError::StaleInputs { .. }));
        if let Some(fatal) = fatal
            && !cancelled
        {
            outcome = RunOutcome::InfrastructureError;
            exit_code = fatal.exit_condition().exit_code();
        }

        let result = RunResult {
            run_id: ctx.run_id.clone(),
            plan_id: *plan.id(),
            mode: plan.mode(),
            scope: plan.body().scope.clone(),
            assurance: achieved_assurance(
                &plan,
                outcome,
                input_validation,
                &progress.cleanup,
                &progress.suites,
            ),
            outcome,
            input_validation,
            environment: progress.environment,
            shadow_gaps: shadow_gaps(&plan, &progress.suites),
            metrics: RunMetrics {
                planning_ms: millis(planned.timings.total),
                environment_ready_ms: millis(progress.environment_ready),
                first_failure_ms: progress.first_failure.map(millis),
                verification_ms: millis(progress.verification),
                cleanup_ms: millis(progress.cleanup_time),
                total_ms: millis(started.elapsed()),
            },
            suites: progress.suites,
            cleanup: progress.cleanup,
            exit_code,
        };
        self.store.finish(&result).await?;
        self.emit(ctx, RunEventPayload::RunFinished { outcome, exit_code })
            .await?;

        match fatal {
            Some(fatal) if !cancelled => Err(fatal.clone()),
            _ => Ok(RunReport { plan, result }),
        }
    }

    async fn emit(
        &self,
        ctx: &OperationContext,
        payload: RunEventPayload,
    ) -> Result<(), PortError> {
        self.events
            .append(RunEvent {
                run_id: ctx.run_id.clone(),
                payload,
            })
            .await
    }

    /// A suite whose runner has no adapter is refused. It is never skipped
    /// and never run by another adapter.
    fn ensure_runners_exist(&self, plan: &ExecutionPlan) -> Result<(), PortError> {
        for suite in plan.suites() {
            if suite.effective_selection.runs_anything()
                && !self.runners.contains_key(&suite.runner)
            {
                return Err(PortError::UnsupportedCapability {
                    capability: format!("runner:{}", suite.runner.as_str()),
                    message: format!(
                        "suite {} uses the {} runner, which this build cannot execute yet",
                        suite.suite_id,
                        suite.runner.as_str()
                    ),
                });
            }
        }
        Ok(())
    }

    /// Steps 3 to 8. The environment is released whatever happens in between.
    #[allow(clippy::too_many_arguments)]
    async fn run(
        &self,
        workspace_root: &Path,
        request: &RunRequest,
        ctx: &OperationContext,
        plan: &ExecutionPlan,
        inventory: &TestInventory,
        started: Instant,
        progress: &mut Progress,
    ) -> Result<(), PortError> {
        let settings = self.planning.settings;
        let ownership = LeaseOwnership {
            workspace_id: plan.body().workspace_id.clone(),
            run_id: ctx.run_id.clone(),
            lease_id: request.lease_id.clone(),
        };

        self.emit(
            ctx,
            RunEventPayload::EnvironmentPreparing {
                provider: request.provider,
            },
        )
        .await?;
        let preparing = Instant::now();
        let lease = self
            .runtime
            .prepare(
                &EnvironmentRequest {
                    provider: request.provider,
                    workspace_root: workspace_root.to_path_buf(),
                    config_fingerprint: *settings.config_digest(),
                    ownership: ownership.clone(),
                    readiness_commands: request.readiness_commands.clone(),
                    trust: request.trust.clone(),
                },
                ctx,
            )
            .await?;
        progress.environment_ready = preparing.elapsed();
        progress.environment = Some(lease.descriptor.clone());
        self.emit(
            ctx,
            RunEventPayload::EnvironmentReady {
                provider: request.provider,
            },
        )
        .await?;

        let verifying = Instant::now();
        let outcome = self
            .verify(
                workspace_root,
                request,
                ctx,
                plan,
                inventory,
                &lease,
                started,
                progress,
            )
            .await;
        progress.verification = verifying.elapsed();
        progress.cancelled |= ctx.cancellation.is_cancelled();

        // Cleanup has its own deadline; a cancelled run still cleans up.
        let disposition = match (&outcome, progress.cancelled) {
            (_, true) => ReleaseDisposition::Cancelled,
            (Err(_), false) => ReleaseDisposition::Failed,
            (Ok(()), false) => ReleaseDisposition::Completed,
        };
        self.emit(ctx, RunEventPayload::CleanupStarted).await?;
        let cleaning = Instant::now();
        let cleanup = CleanupContext {
            deadline: Instant::now() + settings.cleanup_timeout(),
            ownership,
            max_retries: 2,
        };
        progress.cleanup = match self.runtime.release(lease, disposition, &cleanup).await {
            Ok(report) => report,
            Err(error) => CleanupReport {
                failures: vec![error.to_string()],
                ..CleanupReport::default()
            },
        };
        progress.cleanup_time = cleaning.elapsed();
        self.emit(
            ctx,
            RunEventPayload::CleanupFinished {
                complete: progress.cleanup.is_complete(),
            },
        )
        .await?;
        outcome
    }

    /// Steps 4 to 7, inside a prepared environment.
    #[allow(clippy::too_many_arguments)]
    async fn verify(
        &self,
        workspace_root: &Path,
        request: &RunRequest,
        ctx: &OperationContext,
        plan: &ExecutionPlan,
        inventory: &TestInventory,
        lease: &EnvironmentLease,
        started: Instant,
        progress: &mut Progress,
    ) -> Result<(), PortError> {
        let settings = self.planning.settings;
        let mut descriptor = lease.descriptor.clone();

        let mut work = Vec::new();
        for suite_plan in plan
            .suites()
            .iter()
            .filter(|suite| suite.effective_selection.runs_anything())
        {
            let suite = settings
                .suites()
                .iter()
                .find(|suite| suite.id() == &suite_plan.suite_id)
                .ok_or_else(|| PlanningError::UnknownSuite(suite_plan.suite_id.clone()))?;
            let adapter = self.adapter(suite.runner())?;
            let task = TaskId::for_suite(suite.id());
            let artifacts = self
                .artifacts
                .task_artifacts(&task, report_file_name(suite.runner()))
                .await?;

            let version = self
                .probe_version(adapter, suite, &artifacts, lease, ctx)
                .await?;
            if let Some(version) = &version {
                descriptor
                    .tool_versions
                    .insert(version.tool.clone(), version.version.clone());
            }
            let capabilities = adapter.capabilities(version.as_ref());
            if !capabilities.full_run {
                return Err(PortError::UnsupportedCapability {
                    capability: format!("runner:{}", suite.runner().as_str()),
                    message: format!(
                        "the installed runner of suite {} cannot be used",
                        suite.id()
                    ),
                });
            }

            let mut suite_plan = suite_plan.clone();
            let before = selection_kind(&suite_plan.effective_selection);
            apply_runner_capabilities(&mut suite_plan, capabilities.file_selection);
            let after = selection_kind(&suite_plan.effective_selection);
            if before != after {
                let reason_code = if version.is_some() {
                    "unsupported_runner_version"
                } else {
                    "runner_version_unknown"
                };
                self.emit(
                    ctx,
                    RunEventPayload::SelectionExpanded {
                        suite_id: suite.id().clone(),
                        from: before,
                        to: after,
                        reason_code: reason_code.to_owned(),
                    },
                )
                .await?;
            }
            work.push(Work {
                suite,
                plan: suite_plan,
                capabilities,
                artifacts,
            });
        }
        progress.environment = Some(descriptor.clone());

        // A suite command that contradicts its adapter (it sets the report,
        // or narrows the run) is a configuration error of the whole request.
        // It is found here, before the first suite starts, and not when the
        // scheduler happens to reach that suite.
        for item in &work {
            self.adapter(item.suite.runner())?.prepare(
                item.suite,
                &item.plan.effective_selection,
                &descriptor,
                &item.artifacts.locations,
            )?;
        }

        // The plan must still describe the inputs that are about to be tested.
        let before = self
            .planning
            .snapshot(workspace_root, &request.plan, ctx)
            .await?;
        ensure_plan_is_current(plan, &before, settings.config_digest())?;

        let tasks = work
            .iter()
            .map(|work| ScheduledTask {
                id: TaskId::for_suite(work.suite.id()),
                resources: work.suite.resources().clone(),
            })
            .collect();
        let policy = SchedulingPolicy {
            budget: *settings.budget(),
            // Tasks inside one Dev Container lease are serial in v0.1, so a
            // cancellation can stop the whole environment safely.
            serial: lease.provider == ProviderKind::DevcontainerCli,
            fail_fast: settings.fail_fast(),
        };
        let first_failure: Mutex<Option<Duration>> = Mutex::new(None);
        let scheduled = run_scheduled(
            tasks,
            policy,
            ctx.cancellation.as_ref(),
            |task| {
                let work = work
                    .iter()
                    .find(|work| TaskId::for_suite(work.suite.id()) == task);
                let (descriptor, first_failure) = (&descriptor, &first_failure);
                async move {
                    let Some(work) = work else {
                        return Err(PortError::Storage {
                            message: format!("task {task} has no suite"),
                        });
                    };
                    let result = self
                        .run_task(work, lease, descriptor, inventory, ctx)
                        .await?;
                    if is_failure(result.result.outcome) {
                        let mut first = first_failure
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        first.get_or_insert(started.elapsed());
                    }
                    Ok(result)
                }
            },
            |result: &Result<SuiteRunResult, PortError>| match result {
                Ok(suite) => is_failure(suite.result.outcome),
                Err(_) => true,
            },
        )
        .await
        .map_err(PlanningError::Scheduling)?;

        let mut first_error = None;
        for (_, entry) in scheduled {
            match entry {
                Scheduled::Completed(Ok(suite)) => progress.suites.push(suite),
                Scheduled::Completed(Err(error)) => {
                    first_error.get_or_insert(error);
                }
                Scheduled::NotStarted(NotStarted::Cancelled) => progress.cancelled = true,
                Scheduled::NotStarted(NotStarted::FailFast) => {}
            }
        }
        progress.first_failure = *first_failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        progress.cancelled |= ctx.cancellation.is_cancelled();
        if let Some(error) = first_error {
            return Err(error);
        }

        // Inputs that changed while the tests ran invalidate what they said.
        if !progress.cancelled {
            let after = self
                .planning
                .snapshot(workspace_root, &request.plan, ctx)
                .await?;
            progress.input_validation = Some(if after.id() == &plan.body().snapshot_id {
                InputValidation::Unchanged
            } else {
                InputValidation::Changed
            });
        }
        Ok(())
    }

    fn adapter(&self, runner: RunnerKind) -> Result<&dyn RunnerAdapter, PortError> {
        self.runners
            .get(&runner)
            .copied()
            .ok_or_else(|| PortError::UnsupportedCapability {
                capability: format!("runner:{}", runner.as_str()),
                message: "this build has no adapter for the runner".to_owned(),
            })
    }

    /// Asks the runner for its version. An unreadable version is `None`: the
    /// adapter then treats the version as unverified.
    async fn probe_version(
        &self,
        adapter: &dyn RunnerAdapter,
        suite: &SuiteDefinition,
        artifacts: &TaskArtifacts,
        lease: &EnvironmentLease,
        ctx: &OperationContext,
    ) -> Result<Option<ToolVersion>, PortError> {
        let Some(argv) = adapter.version_command(suite) else {
            return Ok(None);
        };
        let Some((program, args)) = argv.split_first() else {
            return Ok(None);
        };
        let command = CommandSpec {
            program: program.clone(),
            args: args.to_vec(),
            cwd: suite.cwd().clone(),
            env: BTreeMap::new(),
            timeout: VERSION_PROBE_TIMEOUT.min(suite.timeout()),
            output: OutputPolicy {
                stdout: artifacts.probe_stdout.clone(),
                stderr: artifacts.probe_stderr.clone(),
                max_bytes_per_stream: VERSION_PROBE_BYTES,
            },
        };
        match self
            .runtime
            .execute(lease, &command, self.events, ctx)
            .await
        {
            Ok(process) if process.exit.is_success() => {
                let output = self
                    .artifacts
                    .read_text(&artifacts.probe_stdout, VERSION_PROBE_BYTES)
                    .await?;
                Ok(adapter.parse_version(&output))
            }
            Ok(_) | Err(PortError::Spawn { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    async fn run_task(
        &self,
        work: &Work<'_>,
        lease: &EnvironmentLease,
        descriptor: &ExecutionDescriptor,
        inventory: &TestInventory,
        ctx: &OperationContext,
    ) -> Result<SuiteRunResult, PortError> {
        let settings = self.planning.settings;
        let suite = work.suite;
        let adapter = self.adapter(suite.runner())?;
        let task = TaskId::for_suite(suite.id());
        self.emit(
            ctx,
            RunEventPayload::TaskStarted {
                task_id: task.clone(),
            },
        )
        .await?;

        let mut invocation = adapter.prepare(
            suite,
            &work.plan.effective_selection,
            descriptor,
            &work.artifacts.locations,
        )?;
        if invocation.effective_selection == Selection::All
            && invocation.expected_files.is_none()
            && work.capabilities.file_attribution
        {
            // The whole suite runs: every registered test file must show up
            // in the report. Files beyond the registration are tolerated.
            invocation.expected_files = Some(inventory.files_of(suite.id()).cloned().collect());
            invocation.additional_files = AdditionalFiles::Expected;
        }

        let Some((program, args)) = invocation.argv.split_first() else {
            return Err(PortError::Configuration {
                message: format!("suite {} has an empty command", suite.id()),
            });
        };
        let command = CommandSpec {
            program: program.clone(),
            args: args.to_vec(),
            cwd: invocation.cwd.clone(),
            env: invocation.env.clone(),
            timeout: suite.timeout(),
            output: OutputPolicy {
                stdout: work.artifacts.stdout.clone(),
                stderr: work.artifacts.stderr.clone(),
                max_bytes_per_stream: settings.max_log_bytes_per_stream(),
            },
        };

        let mut result = match self
            .runtime
            .execute(lease, &command, self.events, ctx)
            .await
        {
            Ok(process) => {
                let reports = if invocation.report_path.is_some() {
                    self.artifacts.read_report(&invocation).await?
                } else {
                    ReportBundle {
                        format: ReportFormat::None,
                        content: ReportContent::Missing,
                    }
                };
                adapter.normalize(&invocation, &process, &reports)?
            }
            Err(PortError::Spawn { program, message }) => unstarted_suite_result(
                &invocation,
                TaskOutcome::InfrastructureError,
                Anomaly {
                    code: anomaly::SPAWN_FAILED,
                    detail: format!("`{program}` could not be started: {message}"),
                },
            ),
            Err(PortError::Cancelled) => unstarted_suite_result(
                &invocation,
                TaskOutcome::Cancelled,
                Anomaly {
                    code: "cancelled",
                    detail: "the run was cancelled".to_owned(),
                },
            ),
            Err(error) => return Err(error),
        };

        let findings = apply_result_policy(
            &mut result,
            suite,
            ResultPolicy {
                fail_on_unexpected_skips: settings.fail_on_unexpected_skips(),
                fail_on_flaky: settings.fail_on_flaky(),
            },
        );
        self.emit(
            ctx,
            RunEventPayload::TaskFinished {
                task_id: task,
                outcome: result.outcome,
            },
        )
        .await?;

        Ok(SuiteRunResult {
            result,
            effective_selection: invocation.effective_selection.clone(),
            runner: suite.runner(),
            findings,
            report: invocation
                .report_path
                .is_some()
                .then(|| work.artifacts.report.clone()),
        })
    }
}

/// The first command of a workspace only runs after someone approved exactly
/// this configuration from outside the repository.
fn ensure_trusted(
    trust: Option<&TrustApproval>,
    config_digest: &testry_domain::fingerprint::Fingerprint,
) -> Result<(), PortError> {
    match trust {
        Some(approval) if &approval.approved_config_digest == config_digest => Ok(()),
        Some(_) => Err(PortError::TrustRequired {
            message: "the configuration changed since this workspace was trusted; review the \
                      commands it runs and approve it again"
                .to_owned(),
        }),
        None => Err(PortError::TrustRequired {
            message: "this workspace is not trusted yet; review the commands its configuration \
                      runs and approve it"
                .to_owned(),
        }),
    }
}
