//! Pure domain model of testry.
//!
//! Value objects, the dependency graph model, selection rules and state
//! transitions live here. This crate performs no IO and must not depend on an
//! async runtime, the CLI, the parser, containers or the database
//! (spec section 3). Invariants are enforced when values are constructed, so
//! the forbidden states of spec section 4 cannot be represented.
#![forbid(unsafe_code)]

pub mod environment;
pub mod fingerprint;
pub mod graph;
pub mod ids;
pub mod inventory;
pub mod paths;
pub mod pattern;
pub mod plan;
pub mod planning;
pub mod resources;
pub mod results;
pub mod selection;
pub mod snapshot;
pub mod suite;
