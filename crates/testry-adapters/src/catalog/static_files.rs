//! The static, include-pattern based test catalog (spec section 5).
//!
//! It matches the files of the snapshot against each suite's `include`
//! patterns. No runner configuration and no test code is executed; what the
//! runner itself would discover stays marked as unverified in the inventory.

use testry_application::catalog::collect_static_inventory;
use testry_application::ports::planning::TestCatalog;
use testry_application::ports::{OperationContext, PortFuture};
use testry_domain::inventory::TestInventory;
use testry_domain::snapshot::SourceSnapshot;
use testry_domain::suite::SuiteDefinition;

#[derive(Debug, Clone, Copy, Default)]
pub struct StaticTestCatalog;

impl TestCatalog for StaticTestCatalog {
    fn collect<'a>(
        &'a self,
        snapshot: &'a SourceSnapshot,
        suites: &'a [SuiteDefinition],
        ctx: &'a OperationContext,
    ) -> PortFuture<'a, TestInventory> {
        Box::pin(async move {
            ctx.ensure_active("collecting the test inventory")?;
            Ok(collect_static_inventory(snapshot, suites))
        })
    }
}
