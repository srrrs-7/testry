//! Application layer of testry: use cases, IO ports, planning, scheduling and
//! result aggregation.
//!
//! This crate depends on `testry-domain` only. Adapters implement the traits
//! in [`ports`]; nothing here knows a concrete adapter (spec section 3).
#![forbid(unsafe_code)]

pub mod catalog;
pub mod commands;
pub mod policy;
pub mod ports;
pub mod queries;
pub mod results;
pub mod scheduler;
pub mod settings;

#[cfg(any(test, feature = "test-support"))]
pub mod testing;
