//! In-memory fakes of the ports, for tests only (feature `test-support`).
//!
//! The fakes record how they were called, so a test can assert that a use
//! case did or did not touch a port. Fakes of the execution ports are in
//! [`execution`].

pub mod execution;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use testry_domain::graph::DependencyGraph;
use testry_domain::ids::RunId;
use testry_domain::inventory::TestInventory;
use testry_domain::snapshot::SourceSnapshot;
use testry_domain::suite::SuiteDefinition;

use crate::catalog::collect_static_inventory;
use crate::ports::execution::{EventSink, RunEvent, RunRecord, RunResult, RunStore};
use crate::ports::planning::{
    AnalysisProfile, DependencyAnalyzer, SnapshotRequest, SnapshotSide, SourceRepository,
    TestCatalog,
};
use crate::ports::{Cancellation, NeverCancelled, OperationContext, PortError, PortFuture};

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A context that is never cancelled and has no deadline.
#[allow(clippy::unwrap_used)]
pub fn context() -> OperationContext {
    OperationContext {
        run_id: RunId::try_new("00000000-0000-4000-8000-000000000000").unwrap(),
        cancellation: Arc::new(NeverCancelled),
        deadline: None,
    }
}

/// A cancellation source that is already cancelled.
#[derive(Debug, Clone, Copy, Default)]
pub struct AlreadyCancelled;

impl Cancellation for AlreadyCancelled {
    fn is_cancelled(&self) -> bool {
        true
    }

    fn cancelled<'a>(&'a self) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(std::future::ready(()))
    }
}

pub fn cancelled_context() -> OperationContext {
    OperationContext {
        cancellation: Arc::new(AlreadyCancelled),
        ..context()
    }
}

/// Returns canned snapshots or errors and records every request. With several
/// responses, each call returns the next one and the last one repeats, which
/// lets a test change the inputs between plan and execution.
pub struct FakeSourceRepository {
    responses: Vec<Result<SourceSnapshot, PortError>>,
    requests: Mutex<Vec<SnapshotRequest>>,
}

impl FakeSourceRepository {
    pub fn returning(snapshot: SourceSnapshot) -> Self {
        Self::sequence(vec![snapshot])
    }

    pub fn sequence(snapshots: Vec<SourceSnapshot>) -> Self {
        Self {
            responses: snapshots.into_iter().map(Ok).collect(),
            requests: Mutex::new(Vec::new()),
        }
    }

    pub fn failing(error: PortError) -> Self {
        Self {
            responses: vec![Err(error)],
            requests: Mutex::new(Vec::new()),
        }
    }

    pub fn requests(&self) -> Vec<SnapshotRequest> {
        lock(&self.requests).clone()
    }
}

impl SourceRepository for FakeSourceRepository {
    fn snapshot<'a>(
        &'a self,
        request: &'a SnapshotRequest,
        ctx: &'a OperationContext,
    ) -> PortFuture<'a, SourceSnapshot> {
        let mut requests = lock(&self.requests);
        let index = requests.len().min(self.responses.len().saturating_sub(1));
        requests.push(request.clone());
        let response = if ctx.cancellation.is_cancelled() {
            Err(PortError::Cancelled)
        } else {
            self.responses.get(index).cloned().unwrap_or_else(|| {
                Err(PortError::Configuration {
                    message: "no canned snapshot".to_owned(),
                })
            })
        };
        Box::pin(async move { response })
    }
}

/// Returns a canned graph per snapshot side and records every profile.
pub struct FakeDependencyAnalyzer {
    graphs: BTreeMap<SnapshotSide, Result<DependencyGraph, PortError>>,
    calls: Mutex<Vec<AnalysisProfile>>,
}

impl FakeDependencyAnalyzer {
    pub fn returning(baseline: DependencyGraph, current: DependencyGraph) -> Self {
        Self {
            graphs: BTreeMap::from([
                (SnapshotSide::Baseline, Ok(baseline)),
                (SnapshotSide::Current, Ok(current)),
            ]),
            calls: Mutex::new(Vec::new()),
        }
    }

    pub fn failing(error: PortError) -> Self {
        Self {
            graphs: BTreeMap::from([
                (SnapshotSide::Baseline, Err(error.clone())),
                (SnapshotSide::Current, Err(error)),
            ]),
            calls: Mutex::new(Vec::new()),
        }
    }

    pub fn calls(&self) -> Vec<AnalysisProfile> {
        lock(&self.calls).clone()
    }
}

impl DependencyAnalyzer for FakeDependencyAnalyzer {
    fn analyze<'a>(
        &'a self,
        _snapshot: &'a SourceSnapshot,
        profile: &'a AnalysisProfile,
        _ctx: &'a OperationContext,
    ) -> PortFuture<'a, DependencyGraph> {
        lock(&self.calls).push(*profile);
        let response = self.graphs.get(&profile.side).cloned().unwrap_or_else(|| {
            Err(PortError::Configuration {
                message: "no canned graph for this side".to_owned(),
            })
        });
        Box::pin(async move { response })
    }
}

/// The static, include-pattern based catalog. It performs no IO, so tests use
/// the real collection logic.
#[derive(Debug, Clone, Copy, Default)]
pub struct StaticCatalog;

impl TestCatalog for StaticCatalog {
    fn collect<'a>(
        &'a self,
        snapshot: &'a SourceSnapshot,
        suites: &'a [SuiteDefinition],
        _ctx: &'a OperationContext,
    ) -> PortFuture<'a, TestInventory> {
        Box::pin(async move { Ok(collect_static_inventory(snapshot, suites)) })
    }
}

/// Keeps events in memory in the order they were appended.
#[derive(Default)]
pub struct RecordingEventSink {
    events: Mutex<Vec<RunEvent>>,
}

impl RecordingEventSink {
    pub fn events(&self) -> Vec<RunEvent> {
        lock(&self.events).clone()
    }
}

impl EventSink for RecordingEventSink {
    fn append<'a>(&'a self, event: RunEvent) -> PortFuture<'a, ()> {
        lock(&self.events).push(event);
        Box::pin(async { Ok(()) })
    }
}

/// A run store that only lives as long as the test.
#[derive(Default)]
pub struct InMemoryRunStore {
    records: Mutex<BTreeMap<RunId, RunRecord>>,
    results: Mutex<BTreeMap<RunId, RunResult>>,
}

impl InMemoryRunStore {
    /// The terminal results that were stored.
    pub fn results(&self) -> Vec<RunResult> {
        lock(&self.results).values().cloned().collect()
    }

    /// Runs that were created but never finished.
    pub fn unfinished(&self) -> Vec<RunId> {
        let results = lock(&self.results);
        lock(&self.records)
            .keys()
            .filter(|id| !results.contains_key(*id))
            .cloned()
            .collect()
    }
}

impl RunStore for InMemoryRunStore {
    fn create<'a>(&'a self, record: &'a RunRecord) -> PortFuture<'a, ()> {
        let mut records = lock(&self.records);
        let outcome = if records.contains_key(&record.run_id) {
            Err(PortError::Storage {
                message: format!("run {} already exists", record.run_id),
            })
        } else {
            records.insert(record.run_id.clone(), record.clone());
            Ok(())
        };
        Box::pin(async move { outcome })
    }

    fn finish<'a>(&'a self, result: &'a RunResult) -> PortFuture<'a, ()> {
        let known = lock(&self.records).contains_key(&result.run_id);
        let outcome = if known {
            lock(&self.results).insert(result.run_id.clone(), result.clone());
            Ok(())
        } else {
            Err(PortError::Storage {
                message: format!("run {} was never created", result.run_id),
            })
        };
        Box::pin(async move { outcome })
    }
}
