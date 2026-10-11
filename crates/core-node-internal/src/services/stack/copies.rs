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
use core_node_api::encoding::LaunchResult;

/// Runs one change over the active launch and writes the record back,
/// whatever the outcome.
async fn change_active_launch(
    ctx: &StackChangeContext,
    change: impl AsyncFnOnce(&mut ActiveLaunch) -> ChangeResult<()>,
) -> LaunchResult {
    let mut active = match active_launch(ctx) {
        Ok(active) => active,
        Err(reason) => return LaunchResult::failure(ctx.log.path(), reason),
    };
    let outcome = change(&mut active).await;
    *ctx.slice_ownership.active.lock() = Some(active);
    match outcome {
        Ok(()) => LaunchResult::success(ctx.log.path()),
        Err(reason) => LaunchResult::failure(ctx.log.path(), reason),
    }
}
