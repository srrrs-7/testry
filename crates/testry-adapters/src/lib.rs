//! Adapters of testry: the implementations of the application ports.
//!
//! Present: the Git snapshot (`source`), the Oxc based dependency analysis
//! (`analysis`), the static test catalog (`catalog`), plan documents
//! (`storage`), process control (`process`), runner report parsers
//! (`reports`) and the Bun, Vitest and command adapters (`runners`).
#![forbid(unsafe_code)]

pub mod analysis;
pub mod catalog;
pub mod process;
pub mod reports;
pub mod runners;
pub mod runtime;
pub mod source;
pub mod storage;
