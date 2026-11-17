//! Execution environments and their lease life cycle (spec section 11).

use serde::{Deserialize, Serialize};

/// How commands are executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// Child processes of the testry process itself. Under the container-first
    /// policy (ADR-0002) testry runs inside a container, so this is the
    /// in-container process provider.
    Host,
    /// A disposable Dev Container owned by the run, driven by the official
    /// Dev Container CLI.
    DevcontainerCli,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::DevcontainerCli => "devcontainer_cli",
        }
    }
}

/// Life cycle of an environment lease.
///
/// ```text
/// Requested -> Preparing -> Ready <-> Running -> Cleaning -> Released
///                   \          \         \          \
///                    +----------+---------+----------+----> Failed
/// ```
///
/// `Running -> Ready` exists because tasks inside one lease run one after
/// another in v0.1. Every non-terminal state may enter `Cleaning`: owned
/// resources are removed no matter how far preparation got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseState {
    Requested,
    Preparing,
    Ready,
    Running,
    Cleaning,
    Released,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("environment lease cannot move from {from:?} to {to:?}")]
pub struct LeaseTransitionError {
    pub from: LeaseState,
    pub to: LeaseState,
}

impl LeaseState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Released | Self::Failed)
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        use LeaseState::{Cleaning, Failed, Preparing, Ready, Released, Requested, Running};
        matches!(
            (self, next),
            (Requested, Preparing)
                | (Preparing, Ready | Cleaning | Failed)
                | (Ready, Running | Cleaning | Failed)
                | (Running, Ready | Cleaning | Failed)
                | (Cleaning, Released | Failed)
        )
    }

    pub fn transition(self, next: Self) -> Result<Self, LeaseTransitionError> {
        if self.can_transition_to(next) {
            Ok(next)
        } else {
            Err(LeaseTransitionError {
                from: self,
                to: next,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LeaseState::{Cleaning, Failed, Preparing, Ready, Released, Requested, Running};
    use super::*;

    const ALL: [LeaseState; 7] = [
        Requested, Preparing, Ready, Running, Cleaning, Released, Failed,
    ];

    #[test]
    fn happy_path_is_allowed() {
        let mut state = Requested;
        for next in [
            Preparing, Ready, Running, Ready, Running, Cleaning, Released,
        ] {
            state = state.transition(next).unwrap();
        }
        assert!(state.is_terminal());
    }

    #[test]
    fn terminal_states_never_leave() {
        for terminal in [Released, Failed] {
            for next in ALL {
                assert!(
                    !terminal.can_transition_to(next),
                    "{terminal:?} -> {next:?}"
                );
            }
        }
    }

    #[test]
    fn a_task_cannot_start_before_the_environment_is_ready() {
        assert_eq!(
            Requested.transition(Running),
            Err(LeaseTransitionError {
                from: Requested,
                to: Running
            })
        );
        assert!(!Preparing.can_transition_to(Running));
        assert!(!Cleaning.can_transition_to(Running));
    }

    #[test]
    fn release_is_only_reachable_through_cleaning() {
        for state in ALL {
            assert_eq!(
                state.can_transition_to(Released),
                state == Cleaning,
                "{state:?}"
            );
        }
    }

    #[test]
    fn a_lease_is_not_handed_out_twice() {
        assert!(!Ready.can_transition_to(Preparing));
        assert!(!Running.can_transition_to(Running));
        assert!(!Ready.can_transition_to(Ready));
    }
}
