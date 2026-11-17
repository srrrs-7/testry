//! Persistent state: plans, run artifacts and the workspace lock below
//! `storage.directory`, and workspace trust in the user's state directory.
//!
//! The SQLite history belongs to milestone M5; until then a run is its
//! directory of JSON documents.

pub mod gc;
pub mod history;
pub mod lock;
pub mod plan_files;
mod private;
pub mod run_files;
#[cfg(test)]
pub(crate) mod test_support;
pub mod time;
pub mod trust;
