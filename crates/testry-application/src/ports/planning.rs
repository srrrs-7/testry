//! Ports used to create a plan. None of them executes project code: a plan is
//! built from Git and file reads only.

use std::collections::BTreeSet;
use std::path::PathBuf;

use testry_domain::graph::DependencyGraph;
use testry_domain::ids::SuiteId;
use testry_domain::inventory::TestInventory;
use testry_domain::pattern::PatternSet;
use testry_domain::plan::Mode;
use testry_domain::snapshot::SourceSnapshot;
use testry_domain::suite::{RunnerKind, SuiteDefinition};

use super::{OperationContext, PortFuture};

#[derive(Debug, Clone)]
pub struct SnapshotRequest {
    /// Absolute path of the workspace. It must be the top level of a Git work
    /// tree.
    pub workspace_root: PathBuf,
    pub mode: Mode,
    /// The ref to diff against. `None` means `HEAD`, which makes the diff the
    /// uncommitted changes. Ignored in full mode.
    pub base_ref: Option<String>,
    /// Whether staged, unstaged and untracked changes are part of the diff.
    pub include_dirty: bool,
    /// The explicit `--suite` scope. Adapters must never use it to drop
    /// changes or files from the snapshot.
    pub scope: Option<BTreeSet<SuiteId>>,
    /// Runtime outputs that are not inputs (`workspace.exclude`).
    pub exclude: PatternSet,
    /// `global_inputs` and all suite `inputs`. Git-ignored files that match
    /// are still fingerprinted.
    pub declared_inputs: PatternSet,
}

pub trait SourceRepository: Send + Sync {
    /// Captures the baseline, the change set and the current files.
    ///
    /// Must not modify the work tree or the index, must not fetch, and must
    /// fail instead of treating an unavailable base as an empty diff.
    fn snapshot<'a>(
        &'a self,
        request: &'a SnapshotRequest,
        ctx: &'a OperationContext,
    ) -> PortFuture<'a, SourceSnapshot>;
}

/// Which tree of the snapshot is analyzed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SnapshotSide {
    /// The baseline commit, read from the Git object database.
    Baseline,
    /// The working tree.
    Current,
}

/// A module resolution rule set that mirrors one runner family. Resolution
/// differs per runner (export conditions, extension order), so graphs are
/// built per profile and combined as a union.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ResolverProfile {
    Bun,
    Vite,
    Node,
}

impl ResolverProfile {
    pub fn name(self) -> &'static str {
        match self {
            Self::Bun => "bun",
            Self::Vite => "vite",
            Self::Node => "node",
        }
    }

    /// The profile a runner resolves modules with. `command` suites run an
    /// arbitrary command, so no profile can be attributed to them.
    pub fn for_runner(runner: RunnerKind) -> Option<Self> {
        match runner {
            RunnerKind::Bun => Some(Self::Bun),
            RunnerKind::Vitest | RunnerKind::StorybookVitest => Some(Self::Vite),
            RunnerKind::Playwright => Some(Self::Node),
            RunnerKind::Command => None,
        }
    }
}

/// Resource limits of the static analysis. Hitting one yields a graph
/// diagnostic, never a silently smaller graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnalysisLimits {
    /// Source files larger than this are not parsed.
    pub max_source_bytes: u64,
}

impl Default for AnalysisLimits {
    fn default() -> Self {
        Self {
            max_source_bytes: 4 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnalysisProfile {
    pub side: SnapshotSide,
    pub resolver: ResolverProfile,
    pub limits: AnalysisLimits,
}

pub trait DependencyAnalyzer: Send + Sync {
    /// Builds the dependency graph of one side of the snapshot under one
    /// resolver profile.
    ///
    /// Whatever cannot be established (unresolved or dynamic imports, parse
    /// failures, unsupported mechanisms) must be reported as a
    /// `GraphDiagnostic`. It must never be dropped or stored as "no
    /// dependency".
    fn analyze<'a>(
        &'a self,
        snapshot: &'a SourceSnapshot,
        profile: &'a AnalysisProfile,
        ctx: &'a OperationContext,
    ) -> PortFuture<'a, DependencyGraph>;
}

pub trait TestCatalog: Send + Sync {
    /// Lists the test files of each suite. In v0.1 this uses file enumeration
    /// and static configuration only; no runner configuration or test code is
    /// executed.
    fn collect<'a>(
        &'a self,
        snapshot: &'a SourceSnapshot,
        suites: &'a [SuiteDefinition],
        ctx: &'a OperationContext,
    ) -> PortFuture<'a, TestInventory>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runners_map_to_resolver_profiles() {
        assert_eq!(
            ResolverProfile::for_runner(RunnerKind::Bun),
            Some(ResolverProfile::Bun)
        );
        assert_eq!(
            ResolverProfile::for_runner(RunnerKind::Vitest),
            Some(ResolverProfile::Vite)
        );
        assert_eq!(
            ResolverProfile::for_runner(RunnerKind::StorybookVitest),
            Some(ResolverProfile::Vite)
        );
        assert_eq!(
            ResolverProfile::for_runner(RunnerKind::Playwright),
            Some(ResolverProfile::Node)
        );
        assert_eq!(ResolverProfile::for_runner(RunnerKind::Command), None);
    }
}
