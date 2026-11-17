//! IO ports (spec section 5).
//!
//! Ports sit at the boundary to external systems only: Git, the parser and
//! resolver, test runners, execution environments, storage. Adapters implement
//! them; the application never depends on a concrete adapter. Pure logic such
//! as graph traversal and selection stays in plain functions.
//!
//! Ports use boxed futures so they are object safe without committing the
//! application to a particular async trait mechanism.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use testry_domain::ids::RunId;

pub mod error;
pub mod execution;
pub mod planning;

pub use error::{PortError, SourceUnavailableReason};

pub type PortFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, PortError>> + Send + 'a>>;

pub trait Cancellation: Send + Sync {
    fn is_cancelled(&self) -> bool;
    /// Completes once cancellation was requested.
    fn cancelled<'a>(&'a self) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
}

/// Carried through every port call.
#[derive(Clone)]
pub struct OperationContext {
    /// Correlates everything done for one invocation. Read-only commands such
    /// as `plan` get a fresh ID as well.
    pub run_id: RunId,
    pub cancellation: Arc<dyn Cancellation>,
    pub deadline: Option<Instant>,
}

impl OperationContext {
    /// Fails fast when the operation was cancelled or ran out of time. Long
    /// adapter operations call this between steps.
    pub fn ensure_active(&self, operation: &str) -> Result<(), PortError> {
        if self.cancellation.is_cancelled() {
            return Err(PortError::Cancelled);
        }
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(PortError::Timeout {
                operation: operation.to_owned(),
            });
        }
        Ok(())
    }
}

/// A cancellation source that never fires, for operations without one.
#[derive(Debug, Clone, Copy, Default)]
pub struct NeverCancelled;

impl Cancellation for NeverCancelled {
    fn is_cancelled(&self) -> bool {
        false
    }

    fn cancelled<'a>(&'a self) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(std::future::pending())
    }
}
