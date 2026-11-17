//! Resource-budgeted task scheduling (spec section 10).
//!
//! - A task reserves all of its resources in one step through the ledger, so
//!   tasks cannot deadlock by taking resources in different orders.
//! - A task that could never fit the budget is rejected before anything runs.
//! - Tasks are considered in their given order. A task that does not fit yet
//!   waits; later tasks that fit may start. No history is consulted, so the
//!   order of consideration is the same for the same plan.
//! - After a cancellation, or after a failure under `fail_fast`, no new task
//!   starts. Running tasks are left to finish (the runtime stops cancelled
//!   processes); tasks that never started are reported as such.

use std::collections::VecDeque;
use std::future::Future;

use futures_util::stream::{FuturesUnordered, StreamExt};
use testry_domain::ids::TaskId;
use testry_domain::resources::{ResourceBudget, ResourceError, ResourceLedger, ResourceRequest};

use crate::ports::Cancellation;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledTask {
    pub id: TaskId,
    pub resources: ResourceRequest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotStarted {
    /// The run was cancelled before the task could start.
    Cancelled,
    /// An earlier task failed and `fail_fast` is set.
    FailFast,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scheduled<T> {
    Completed(T),
    NotStarted(NotStarted),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulingPolicy {
    pub budget: ResourceBudget,
    /// Run one task at a time regardless of the budget. Tasks inside one
    /// Dev Container lease are serial in v0.1.
    pub serial: bool,
    pub fail_fast: bool,
}

/// Runs `tasks` and returns one entry per task, in the given order.
///
/// `run` starts a task; `is_failure` tells whether a finished task should
/// stop further launches under `fail_fast`.
pub async fn run_scheduled<T, F, Fut>(
    tasks: Vec<ScheduledTask>,
    policy: SchedulingPolicy,
    cancellation: &dyn Cancellation,
    run: F,
    is_failure: impl Fn(&T) -> bool,
) -> Result<Vec<(TaskId, Scheduled<T>)>, ResourceError>
where
    F: Fn(TaskId) -> Fut,
    Fut: Future<Output = T>,
{
    // Fail before anything starts instead of waiting forever later.
    for task in &tasks {
        policy.budget.admit(&task.resources)?;
    }
    let budget = if policy.serial {
        ResourceBudget::try_new(policy.budget.cpu_slots(), policy.budget.memory_mib(), 1)?
    } else {
        policy.budget
    };

    let order: Vec<TaskId> = tasks.iter().map(|task| task.id.clone()).collect();
    let mut ledger = ResourceLedger::new(budget);
    let mut pending: VecDeque<ScheduledTask> = tasks.into();
    let mut running = FuturesUnordered::new();
    let mut finished: Vec<(TaskId, Scheduled<T>)> = Vec::with_capacity(order.len());
    let mut stop: Option<NotStarted> = None;

    loop {
        if stop.is_none() && cancellation.is_cancelled() {
            stop = Some(NotStarted::Cancelled);
        }
        match stop {
            Some(reason) => {
                finished.extend(
                    pending
                        .drain(..)
                        .map(|task| (task.id, Scheduled::NotStarted(reason))),
                );
            }
            None => {
                // First fit in the given order.
                let mut waiting = VecDeque::with_capacity(pending.len());
                while let Some(task) = pending.pop_front() {
                    if ledger.try_reserve(&task.id, &task.resources)? {
                        let id = task.id.clone();
                        let future = run(task.id);
                        running.push(async move { (id, future.await) });
                    } else {
                        waiting.push_back(task);
                    }
                }
                pending = waiting;
            }
        }

        if running.is_empty() {
            // Every task fits an empty ledger, so nothing can be left waiting.
            finished.extend(
                pending
                    .drain(..)
                    .map(|task| (task.id, Scheduled::NotStarted(NotStarted::Cancelled))),
            );
            break;
        }

        tokio_free_select(
            &mut running,
            cancellation,
            stop.is_none(),
            |completed| match completed {
                Some((id, output)) => {
                    // The reservation was made above; a failure here is a bug in
                    // this function, not in the caller.
                    let _ = ledger.release(&id);
                    if stop.is_none() && policy.fail_fast && is_failure(&output) {
                        stop = Some(NotStarted::FailFast);
                    }
                    finished.push((id, Scheduled::Completed(output)));
                }
                None => stop = Some(NotStarted::Cancelled),
            },
        )
        .await;
    }

    finished.sort_by_key(|(id, _)| order.iter().position(|candidate| candidate == id));
    Ok(finished)
}

/// Waits for the next finished task, or for cancellation when `watch` is set.
/// Passes `Some(result)` or `None` (cancelled) to `on_event`.
async fn tokio_free_select<S, T>(
    running: &mut S,
    cancellation: &dyn Cancellation,
    watch: bool,
    on_event: impl FnOnce(Option<T>),
) where
    S: futures_util::Stream<Item = T> + Unpin,
{
    use futures_util::future::{Either, select};
    if !watch {
        if let Some(result) = running.next().await {
            on_event(Some(result));
        }
        return;
    }
    let cancelled = cancellation.cancelled();
    match select(running.next(), cancelled).await {
        Either::Left((Some(result), _)) => on_event(Some(result)),
        Either::Left((None, _)) => {}
        Either::Right(((), _)) => on_event(None),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use testry_domain::ids::ExclusiveKey;

    use super::*;
    use crate::ports::NeverCancelled;

    fn task(name: &str, cpu: u32, memory: u64, exclusive: &[&str]) -> ScheduledTask {
        ScheduledTask {
            id: TaskId::try_new(name).unwrap(),
            resources: ResourceRequest::try_new(
                cpu,
                memory,
                exclusive
                    .iter()
                    .map(|key| ExclusiveKey::try_new(*key).unwrap()),
            )
            .unwrap(),
        }
    }

    fn policy(cpu: u32, memory: u64, parallel: u32) -> SchedulingPolicy {
        SchedulingPolicy {
            budget: ResourceBudget::try_new(cpu, memory, parallel).unwrap(),
            serial: false,
            fail_fast: false,
        }
    }

    /// Records what runs concurrently.
    #[derive(Default)]
    struct Probe {
        active: Mutex<BTreeMap<String, ResourceRequest>>,
        max_parallel: Mutex<usize>,
        max_cpu: Mutex<u32>,
        max_memory: Mutex<u64>,
        overlaps: Mutex<Vec<String>>,
        started: Mutex<Vec<String>>,
    }

    impl Probe {
        async fn run(&self, tasks: &[ScheduledTask], id: TaskId, millis: u64) -> String {
            let name = id.as_str().to_owned();
            let resources = tasks
                .iter()
                .find(|task| task.id == id)
                .unwrap()
                .resources
                .clone();
            {
                let mut active = self.active.lock().unwrap();
                for (other, held) in active.iter() {
                    if !held.exclusive().is_disjoint(resources.exclusive()) {
                        self.overlaps
                            .lock()
                            .unwrap()
                            .push(format!("{other}+{name}"));
                    }
                }
                active.insert(name.clone(), resources);
                let parallel = active.len();
                let cpu: u32 = active.values().map(ResourceRequest::cpu_slots).sum();
                let memory: u64 = active.values().map(ResourceRequest::memory_mib).sum();
                let mut max_parallel = self.max_parallel.lock().unwrap();
                *max_parallel = (*max_parallel).max(parallel);
                let mut max_cpu = self.max_cpu.lock().unwrap();
                *max_cpu = (*max_cpu).max(cpu);
                let mut max_memory = self.max_memory.lock().unwrap();
                *max_memory = (*max_memory).max(memory);
                self.started.lock().unwrap().push(name.clone());
            }
            tokio::time::sleep(Duration::from_millis(millis)).await;
            self.active.lock().unwrap().remove(&name);
            name
        }
    }

    fn completed<T: Clone>(results: &[(TaskId, Scheduled<T>)]) -> Vec<T> {
        results
            .iter()
            .filter_map(|(_, result)| match result {
                Scheduled::Completed(output) => Some(output.clone()),
                Scheduled::NotStarted(_) => None,
            })
            .collect()
    }

    /// R01: a task larger than the whole budget fails at once; nothing starts.
    #[tokio::test]
    async fn r01_oversized_task_is_rejected_before_anything_starts() {
        let tasks = vec![task("small", 1, 100, &[]), task("huge", 9, 100, &[])];
        let started = AtomicBool::new(false);
        let result = run_scheduled(
            tasks,
            policy(4, 1000, 2),
            &NeverCancelled,
            |_| {
                started.store(true, Ordering::SeqCst);
                async {}
            },
            |()| false,
        )
        .await;
        assert!(matches!(result, Err(ResourceError::ExceedsBudget { .. })));
        assert!(!started.load(Ordering::SeqCst));
    }

    /// R02: suites sharing an exclusive key never run at the same time.
    #[tokio::test]
    async fn r02_exclusive_keys_serialize_tasks() {
        let tasks = vec![
            task("e2e", 1, 100, &["test-database", "browser-budget"]),
            task("api", 1, 100, &["test-database"]),
            task("storybook", 1, 100, &["browser-budget"]),
            task("unit", 1, 100, &[]),
        ];
        let probe = Probe::default();
        let results = run_scheduled(
            tasks.clone(),
            policy(8, 8000, 4),
            &NeverCancelled,
            |id| probe.run(&tasks, id, 20),
            |_| false,
        )
        .await
        .unwrap();
        assert_eq!(completed(&results).len(), 4);
        assert!(
            probe.overlaps.lock().unwrap().is_empty(),
            "{:?}",
            probe.overlaps.lock().unwrap()
        );
        // Tasks without a shared key did run together.
        assert!(*probe.max_parallel.lock().unwrap() >= 2);
    }

    /// R03: the sum of reservations never exceeds the budget.
    #[tokio::test]
    async fn r03_reservations_stay_within_the_budget() {
        let tasks: Vec<ScheduledTask> = (0..8)
            .map(|index| task(&format!("t{index}"), 1 + index % 3, 300, &[]))
            .collect();
        let probe = Probe::default();
        let results = run_scheduled(
            tasks.clone(),
            policy(4, 1000, 3),
            &NeverCancelled,
            |id| probe.run(&tasks, id, 10),
            |_| false,
        )
        .await
        .unwrap();
        assert_eq!(completed(&results).len(), 8);
        assert!(*probe.max_cpu.lock().unwrap() <= 4);
        assert!(*probe.max_memory.lock().unwrap() <= 1000);
        assert!(*probe.max_parallel.lock().unwrap() <= 3);
    }

    #[tokio::test]
    async fn results_keep_the_given_order_whatever_finishes_first() {
        let tasks = vec![
            task("slow", 1, 1, &[]),
            task("fast", 1, 1, &[]),
            task("medium", 1, 1, &[]),
        ];
        let results = run_scheduled(
            tasks,
            policy(4, 100, 4),
            &NeverCancelled,
            |id| async move {
                let millis = match id.as_str() {
                    "slow" => 60,
                    "medium" => 30,
                    _ => 1,
                };
                tokio::time::sleep(Duration::from_millis(millis)).await;
                id.as_str().to_owned()
            },
            |_| false,
        )
        .await
        .unwrap();
        assert_eq!(completed(&results), ["slow", "fast", "medium"]);
    }

    #[tokio::test]
    async fn serial_mode_runs_one_task_at_a_time_in_order() {
        let tasks = vec![
            task("a", 1, 1, &[]),
            task("b", 1, 1, &[]),
            task("c", 1, 1, &[]),
        ];
        let probe = Probe::default();
        let mut serial = policy(8, 100, 8);
        serial.serial = true;
        run_scheduled(
            tasks.clone(),
            serial,
            &NeverCancelled,
            |id| probe.run(&tasks, id, 5),
            |_| false,
        )
        .await
        .unwrap();
        assert_eq!(*probe.max_parallel.lock().unwrap(), 1);
        assert_eq!(*probe.started.lock().unwrap(), ["a", "b", "c"]);
    }

    #[tokio::test]
    async fn fail_fast_stops_new_launches_but_keeps_results() {
        let tasks = vec![
            task("a", 1, 1, &[]),
            task("b", 1, 1, &[]),
            task("c", 1, 1, &[]),
        ];
        let mut fail_fast = policy(1, 100, 1);
        fail_fast.fail_fast = true;
        let results = run_scheduled(
            tasks.clone(),
            fail_fast,
            &NeverCancelled,
            |id| async move { id.as_str() != "a" },
            |passed| !*passed,
        )
        .await
        .unwrap();
        assert_eq!(results[0].1, Scheduled::Completed(false));
        assert_eq!(results[1].1, Scheduled::NotStarted(NotStarted::FailFast));
        assert_eq!(results[2].1, Scheduled::NotStarted(NotStarted::FailFast));

        // Without fail_fast every task runs and every result is kept.
        let results = run_scheduled(
            tasks,
            policy(1, 100, 1),
            &NeverCancelled,
            |id| async move { id.as_str() != "a" },
            |passed| !*passed,
        )
        .await
        .unwrap();
        assert_eq!(completed(&results), [false, true, true]);
    }

    /// A cancellation source a test can trigger.
    #[derive(Default)]
    struct Trigger {
        cancelled: AtomicBool,
        notify: tokio::sync::Notify,
    }

    impl Trigger {
        fn fire(&self) {
            self.cancelled.store(true, Ordering::SeqCst);
            self.notify.notify_waiters();
        }
    }

    impl Cancellation for Trigger {
        fn is_cancelled(&self) -> bool {
            self.cancelled.load(Ordering::SeqCst)
        }

        fn cancelled<'a>(&'a self) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
            Box::pin(async move {
                loop {
                    let notified = self.notify.notified();
                    if self.is_cancelled() {
                        return;
                    }
                    notified.await;
                }
            })
        }
    }

    /// R06: after a cancellation no new task starts; running tasks finish.
    #[tokio::test]
    async fn r06_cancellation_stops_new_launches() {
        let tasks = vec![
            task("first", 1, 1, &[]),
            task("second", 1, 1, &[]),
            task("third", 1, 1, &[]),
        ];
        let trigger = Trigger::default();
        let started = Mutex::new(Vec::new());
        let results = run_scheduled(
            tasks,
            policy(1, 100, 1),
            &trigger,
            |id| {
                started.lock().unwrap().push(id.as_str().to_owned());
                let trigger = &trigger;
                async move {
                    // The running task observes the cancellation itself.
                    trigger.fire();
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    "finished"
                }
            },
            |_| false,
        )
        .await
        .unwrap();
        assert_eq!(*started.lock().unwrap(), ["first"]);
        assert_eq!(results[0].1, Scheduled::Completed("finished"));
        assert_eq!(results[1].1, Scheduled::NotStarted(NotStarted::Cancelled));
        assert_eq!(results[2].1, Scheduled::NotStarted(NotStarted::Cancelled));
    }

    #[tokio::test]
    async fn a_run_cancelled_up_front_starts_nothing() {
        let trigger = Trigger::default();
        trigger.fire();
        let results = run_scheduled(
            vec![task("a", 1, 1, &[])],
            policy(1, 100, 1),
            &trigger,
            |_| async { unreachable!("nothing may start") },
            |()| false,
        )
        .await
        .unwrap();
        assert_eq!(results[0].1, Scheduled::NotStarted(NotStarted::Cancelled));
    }

    #[tokio::test]
    async fn no_tasks_is_an_empty_result() {
        let results = run_scheduled(
            Vec::new(),
            policy(1, 100, 1),
            &NeverCancelled,
            |_| async {},
            |()| false,
        )
        .await
        .unwrap();
        assert!(results.is_empty());
    }
}
