//! Logical resource reservations (spec section 10).
//!
//! Reservations are scheduling budgets, not operating system limits. The
//! ledger acquires all resources of a task in one step, so two tasks can never
//! deadlock by taking resources in different orders.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::ids::{ExclusiveKey, TaskId};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResourceError {
    #[error("cpu_slots must be at least 1")]
    ZeroCpu,
    #[error("memory must be at least 1 MiB")]
    ZeroMemory,
    #[error("max_parallel_suites must be at least 1")]
    ZeroParallelism,
    #[error(
        "request needs {requested_cpu} cpu slot(s) and {requested_memory_mib} MiB but the whole \
         budget is {budget_cpu} cpu slot(s) and {budget_memory_mib} MiB; it could never be scheduled"
    )]
    ExceedsBudget {
        requested_cpu: u32,
        requested_memory_mib: u64,
        budget_cpu: u32,
        budget_memory_mib: u64,
    },
    #[error("task {0} already holds a reservation")]
    AlreadyReserved(TaskId),
    #[error("task {0} holds no reservation to release")]
    NotReserved(TaskId),
}

/// What one task reserves while it runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ResourceRequestDocument")]
pub struct ResourceRequest {
    cpu_slots: u32,
    memory_mib: u64,
    exclusive: BTreeSet<ExclusiveKey>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResourceRequestDocument {
    cpu_slots: u32,
    memory_mib: u64,
    exclusive: BTreeSet<ExclusiveKey>,
}

impl TryFrom<ResourceRequestDocument> for ResourceRequest {
    type Error = ResourceError;

    fn try_from(document: ResourceRequestDocument) -> Result<Self, Self::Error> {
        Self::try_new(document.cpu_slots, document.memory_mib, document.exclusive)
    }
}

impl ResourceRequest {
    pub fn try_new(
        cpu_slots: u32,
        memory_mib: u64,
        exclusive: impl IntoIterator<Item = ExclusiveKey>,
    ) -> Result<Self, ResourceError> {
        if cpu_slots == 0 {
            return Err(ResourceError::ZeroCpu);
        }
        if memory_mib == 0 {
            return Err(ResourceError::ZeroMemory);
        }
        Ok(Self {
            cpu_slots,
            memory_mib,
            exclusive: exclusive.into_iter().collect(),
        })
    }

    pub fn cpu_slots(&self) -> u32 {
        self.cpu_slots
    }

    pub fn memory_mib(&self) -> u64 {
        self.memory_mib
    }

    pub fn exclusive(&self) -> &BTreeSet<ExclusiveKey> {
        &self.exclusive
    }
}

/// The total logical budget of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ResourceBudgetDocument")]
pub struct ResourceBudget {
    cpu_slots: u32,
    memory_mib: u64,
    max_parallel_suites: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResourceBudgetDocument {
    cpu_slots: u32,
    memory_mib: u64,
    max_parallel_suites: u32,
}

impl TryFrom<ResourceBudgetDocument> for ResourceBudget {
    type Error = ResourceError;

    fn try_from(document: ResourceBudgetDocument) -> Result<Self, Self::Error> {
        Self::try_new(
            document.cpu_slots,
            document.memory_mib,
            document.max_parallel_suites,
        )
    }
}

impl ResourceBudget {
    pub fn try_new(
        cpu_slots: u32,
        memory_mib: u64,
        max_parallel_suites: u32,
    ) -> Result<Self, ResourceError> {
        if cpu_slots == 0 {
            return Err(ResourceError::ZeroCpu);
        }
        if memory_mib == 0 {
            return Err(ResourceError::ZeroMemory);
        }
        if max_parallel_suites == 0 {
            return Err(ResourceError::ZeroParallelism);
        }
        Ok(Self {
            cpu_slots,
            memory_mib,
            max_parallel_suites,
        })
    }

    pub fn cpu_slots(&self) -> u32 {
        self.cpu_slots
    }

    pub fn memory_mib(&self) -> u64 {
        self.memory_mib
    }

    pub fn max_parallel_suites(&self) -> u32 {
        self.max_parallel_suites
    }

    /// Rejects a request that could never be satisfied, so the scheduler fails
    /// at planning time instead of waiting forever.
    pub fn admit(&self, request: &ResourceRequest) -> Result<(), ResourceError> {
        if request.cpu_slots > self.cpu_slots || request.memory_mib > self.memory_mib {
            return Err(ResourceError::ExceedsBudget {
                requested_cpu: request.cpu_slots,
                requested_memory_mib: request.memory_mib,
                budget_cpu: self.cpu_slots,
                budget_memory_mib: self.memory_mib,
            });
        }
        Ok(())
    }
}

/// Tracks which task holds which reservation. All resources of a task are
/// taken and returned together.
#[derive(Debug, Clone)]
pub struct ResourceLedger {
    budget: ResourceBudget,
    held: BTreeMap<TaskId, ResourceRequest>,
}

impl ResourceLedger {
    pub fn new(budget: ResourceBudget) -> Self {
        Self {
            budget,
            held: BTreeMap::new(),
        }
    }

    pub fn budget(&self) -> &ResourceBudget {
        &self.budget
    }

    pub fn running(&self) -> usize {
        self.held.len()
    }

    /// Whether `request` fits next to the current reservations.
    pub fn can_reserve(&self, request: &ResourceRequest) -> bool {
        let running = u32::try_from(self.held.len()).unwrap_or(u32::MAX);
        if running >= self.budget.max_parallel_suites {
            return false;
        }
        let cpu: u64 = self
            .held
            .values()
            .map(|held| u64::from(held.cpu_slots))
            .sum();
        let memory: u64 = self
            .held
            .values()
            .fold(0u64, |sum, held| sum.saturating_add(held.memory_mib));
        if cpu + u64::from(request.cpu_slots) > u64::from(self.budget.cpu_slots) {
            return false;
        }
        if memory.saturating_add(request.memory_mib) > self.budget.memory_mib {
            return false;
        }
        !self
            .held
            .values()
            .any(|held| !held.exclusive.is_disjoint(&request.exclusive))
    }

    /// Reserves everything `request` needs, or nothing. `Ok(false)` means the
    /// task has to wait; an error means the caller violated the protocol.
    pub fn try_reserve(
        &mut self,
        task: &TaskId,
        request: &ResourceRequest,
    ) -> Result<bool, ResourceError> {
        self.budget.admit(request)?;
        if self.held.contains_key(task) {
            return Err(ResourceError::AlreadyReserved(task.clone()));
        }
        if !self.can_reserve(request) {
            return Ok(false);
        }
        self.held.insert(task.clone(), request.clone());
        Ok(true)
    }

    /// Returns a reservation. Releasing what was never reserved is an error.
    pub fn release(&mut self, task: &TaskId) -> Result<(), ResourceError> {
        match self.held.remove(task) {
            Some(_) => Ok(()),
            None => Err(ResourceError::NotReserved(task.clone())),
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn key(name: &str) -> ExclusiveKey {
        ExclusiveKey::try_new(name).unwrap()
    }

    fn task(name: &str) -> TaskId {
        TaskId::try_new(name).unwrap()
    }

    fn request(cpu: u32, memory: u64, exclusive: &[&str]) -> ResourceRequest {
        ResourceRequest::try_new(cpu, memory, exclusive.iter().map(|name| key(name))).unwrap()
    }

    #[test]
    fn rejects_zero_reservations() {
        assert_eq!(
            ResourceRequest::try_new(0, 512, []),
            Err(ResourceError::ZeroCpu)
        );
        assert_eq!(
            ResourceRequest::try_new(1, 0, []),
            Err(ResourceError::ZeroMemory)
        );
        assert_eq!(
            ResourceBudget::try_new(0, 1, 1),
            Err(ResourceError::ZeroCpu)
        );
        assert_eq!(
            ResourceBudget::try_new(1, 0, 1),
            Err(ResourceError::ZeroMemory)
        );
        assert_eq!(
            ResourceBudget::try_new(1, 1, 0),
            Err(ResourceError::ZeroParallelism)
        );
    }

    /// R01: a task larger than the whole budget is an immediate error.
    #[test]
    fn r01_request_larger_than_budget_is_rejected_immediately() {
        let budget = ResourceBudget::try_new(4, 4096, 2).unwrap();
        assert!(matches!(
            budget.admit(&request(5, 512, &[])),
            Err(ResourceError::ExceedsBudget { .. })
        ));
        assert!(matches!(
            budget.admit(&request(1, 8192, &[])),
            Err(ResourceError::ExceedsBudget { .. })
        ));
        let mut ledger = ResourceLedger::new(budget);
        assert!(matches!(
            ledger.try_reserve(&task("big"), &request(5, 512, &[])),
            Err(ResourceError::ExceedsBudget { .. })
        ));
    }

    /// R02: suites sharing an exclusive key never run together.
    #[test]
    fn r02_exclusive_keys_are_never_shared() {
        let mut ledger = ResourceLedger::new(ResourceBudget::try_new(8, 8192, 4).unwrap());
        assert!(
            ledger
                .try_reserve(&task("e2e"), &request(1, 512, &["test-database"]))
                .unwrap()
        );
        assert!(
            !ledger
                .try_reserve(&task("api"), &request(1, 512, &["test-database"]))
                .unwrap()
        );
        assert!(
            ledger
                .try_reserve(&task("unit"), &request(1, 512, &[]))
                .unwrap()
        );
        ledger.release(&task("e2e")).unwrap();
        assert!(
            ledger
                .try_reserve(&task("api"), &request(1, 512, &["test-database"]))
                .unwrap()
        );
    }

    #[test]
    fn reservations_respect_cpu_memory_and_parallelism() {
        let mut ledger = ResourceLedger::new(ResourceBudget::try_new(4, 2048, 2).unwrap());
        assert!(
            ledger
                .try_reserve(&task("a"), &request(3, 1024, &[]))
                .unwrap()
        );
        assert!(
            !ledger
                .try_reserve(&task("b"), &request(2, 512, &[]))
                .unwrap()
        );
        assert!(
            !ledger
                .try_reserve(&task("c"), &request(1, 2048, &[]))
                .unwrap()
        );
        assert!(
            ledger
                .try_reserve(&task("d"), &request(1, 1024, &[]))
                .unwrap()
        );
        assert!(
            !ledger
                .try_reserve(&task("e"), &request(1, 1, &[]))
                .is_ok_and(|ok| ok)
        );
        assert_eq!(ledger.running(), 2);
    }

    #[test]
    fn double_reserve_and_unknown_release_are_errors() {
        let mut ledger = ResourceLedger::new(ResourceBudget::try_new(4, 2048, 4).unwrap());
        assert!(ledger.try_reserve(&task("a"), &request(1, 1, &[])).unwrap());
        assert_eq!(
            ledger.try_reserve(&task("a"), &request(1, 1, &[])),
            Err(ResourceError::AlreadyReserved(task("a")))
        );
        assert_eq!(
            ledger.release(&task("zzz")),
            Err(ResourceError::NotReserved(task("zzz")))
        );
        ledger.release(&task("a")).unwrap();
        assert_eq!(
            ledger.release(&task("a")),
            Err(ResourceError::NotReserved(task("a")))
        );
    }

    proptest! {
        /// R03: whatever sequence of reservations is attempted, the sum of held
        /// reservations never exceeds the budget and exclusive keys stay unique.
        #[test]
        fn held_reservations_never_exceed_the_budget(
            requests in proptest::collection::vec((1u32..6, 1u64..3000, 0usize..3), 1..24)
        ) {
            let budget = ResourceBudget::try_new(4, 4096, 3).unwrap();
            let mut ledger = ResourceLedger::new(budget);
            let keys = ["db", "browser"];
            for (index, (cpu, memory, key_index)) in requests.into_iter().enumerate() {
                let exclusive: Vec<&str> = keys.get(key_index).copied().into_iter().collect();
                let request = request(cpu, memory, &exclusive);
                let name = task(&format!("t{index}"));
                let _ = ledger.try_reserve(&name, &request);
                let cpu_total: u32 = ledger.held.values().map(ResourceRequest::cpu_slots).sum();
                let memory_total: u64 = ledger.held.values().map(ResourceRequest::memory_mib).sum();
                prop_assert!(cpu_total <= budget.cpu_slots());
                prop_assert!(memory_total <= budget.memory_mib());
                prop_assert!(ledger.running() <= budget.max_parallel_suites() as usize);
                let mut seen = BTreeSet::new();
                for held in ledger.held.values() {
                    for key in held.exclusive() {
                        prop_assert!(seen.insert(key.clone()), "exclusive key held twice");
                    }
                }
                if index % 3 == 2 {
                    let first = ledger.held.keys().next().cloned();
                    if let Some(first) = first {
                        ledger.release(&first).unwrap();
                    }
                }
            }
        }
    }
}
