mod action;
mod benchmark;
mod container_mounts;
mod copies;
#[cfg(test)]
mod fixtures;
mod launch;
mod list;
mod reset;
mod state;

pub(crate) use action::{StackChangeDefaults, StackChangeTimeouts, listen_for_stack_action};
pub(crate) use benchmark::listen_for_stack_benchmark;
pub(crate) use container_mounts::{prepare_additional_container_mounts, prepare_container_mounts};
pub use copies::remove::copy_removal_budget;
pub(crate) use launch::nodes::STACK_LAUNCH_GIT_HASH;
pub use launch::phases::{idle_timeout_flag, slow_connection_hint};
pub(crate) use list::listen_for_stack_list;
pub use reset::stack_reset_timeout;
pub(crate) use reset::{clear_stack_slice, listen_for_stack_reset};
pub(crate) use state::ActiveLaunch;

use std::time::Duration;

/// The budget for one question to a participant: a stack listing, a node
/// inspection, or a clock membership update.
const STACK_QUERY_TIMEOUT: Duration = Duration::from_secs(30);

/// What a stack change reports when it refuses or fails: the operator's
/// whole account of it.
type ChangeResult<T> = std::result::Result<T, String>;

/// The stack listing of `host`, a participant of the launch.
async fn stack_list_on(
    ctx: &action::StackChangeContext,
    host: &str,
) -> ChangeResult<core_node_api::encoding::StackListResponse> {
    peppylib::core_node::transport::poll(
        &core_node_api::encoding::StackListRequest::new(),
        &ctx.messenger,
        &ctx.bound_core_node,
        &ctx.core_instance_id,
        host,
        STACK_QUERY_TIMEOUT,
    )
    .await
    .map_err(|error| format!("cannot inspect `{host}`: {error}"))
}

/// Whether the stack runs anything beyond its root entity, which is always
/// present.
pub(crate) fn holds_nodes(node_stack: &node_stack::NodeStack) -> bool {
    node_stack.snapshot().len() > 1
}
