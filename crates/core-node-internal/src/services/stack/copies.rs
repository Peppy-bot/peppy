//! A copy is one member of a launcher's repeatable component: the instances
//! one of its options deploys, under the name the operator gave them. A
//! launch starts the copies its launcher file names, `peppy stack join` adds
//! one to the running stack, and `peppy stack remove` takes one off. Every
//! change here runs against the launch this daemon coordinates and writes it
//! back, whatever the outcome.

mod changed_slots;
pub(in crate::services::stack) mod join;
mod live;
mod plan;
pub(in crate::services::stack) mod remove;

use super::action::StackChangeContext;
use super::state::{ActiveLaunch, active_launch};
use super::{ChangeResult, stack_list_on};
use crate::services::fleet::FleetBook;

/// Runs one change over the active launch and writes the record back,
/// whatever the outcome. A change that succeeds then runs `committed` on the
/// book of the fleet, under the lock of the record: a client that reads the
/// record finds the book in step with it, so no client takes the lease of a
/// copy that a lease holder joined before the join leases it.
async fn change_active_launch(
    ctx: &StackChangeContext,
    change: impl AsyncFnOnce(&mut ActiveLaunch) -> ChangeResult<()>,
    committed: impl FnOnce(&mut FleetBook),
) -> ChangeResult<()> {
    let mut active = active_launch(ctx)?;
    let outcome = change(&mut active).await;
    let mut record = ctx.slice_ownership.active.lock();
    *record = Some(active);
    if outcome.is_ok() {
        ctx.slice_ownership.fleet.with(committed);
    }
    drop(record);
    outcome
}
