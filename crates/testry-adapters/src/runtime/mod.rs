//! Execution environments.
//!
//! There is one provider: `host`, child processes of testry itself inside
//! the container testry runs in. A Dev Container provider is not implemented
//! by decision (ADR-0006).

pub mod host;
