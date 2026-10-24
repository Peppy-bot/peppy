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
pub(crate) use container_mounts::{LocalMounts, prepare_container_mounts};
pub use copies::remove::copy_removal_budget;
pub(crate) use launch::nodes::STACK_LAUNCH_GIT_HASH;
pub use launch::phases::{idle_timeout_flag, slow_connection_hint};
pub(crate) use list::listen_for_stack_list;
pub use reset::stack_reset_timeout;
pub(crate) use reset::{clear_stack_slice, listen_for_stack_reset};
pub(crate) use state::ActiveLaunch;

use crate::services::action_loop::WorkStop;
use crate::services::response::failure;
use config::failure_kind::PeppyFailureKind;
use core_node_api::encoding::{LaunchResult, NodeLogs};
use peppylib::messaging::Failure;
use std::time::Duration;

/// The budget for one question to a participant: a stack listing, a node
/// inspection, or a clock membership update.
const STACK_QUERY_TIMEOUT: Duration = Duration::from_secs(30);

/// What a stack change reports when it refuses, fails or stops at a
/// cancel: the cause, the failure kind and the operator's whole account of
/// it.
type ChangeResult<T> = std::result::Result<T, WorkStop>;

/// What a stack change ended with: its result or its failure, and the log
/// file of each node phase it ran, which its goal reports whichever way it
/// ends.
pub(super) struct ChangeEnd {
    pub(super) node_logs: NodeLogs,
    pub(super) outcome: ChangeResult<LaunchResult>,
}

impl ChangeEnd {
    /// The end of a change that ran no node phase.
    pub(super) fn without_node_logs(outcome: ChangeResult<LaunchResult>) -> Self {
        Self {
            node_logs: NodeLogs::default(),
            outcome,
        }
    }

    /// The end of a change that stopped with `stop` before any node phase.
    pub(super) fn stopped(stop: impl Into<WorkStop>) -> Self {
        Self::without_node_logs(Err(stop.into()))
    }
}

/// A change refused because its goal, its launcher or its plan is invalid,
/// or names what the machines do not hold.
fn rejected(reason: impl ToString) -> Failure {
    failure(PeppyFailureKind::Rejected, reason)
}

/// A change refused because the stack or a machine is not in the state it
/// needs: busy, reserved, or holding what the change would collide with.
fn precondition_failed(reason: impl ToString) -> Failure {
    failure(PeppyFailureKind::PreconditionFailed, reason)
}

/// A change whose step ran and gave no result: an add, a build, a start or
/// a write that failed.
fn attempt_failed(reason: impl ToString) -> Failure {
    failure(PeppyFailureKind::AttemptFailed, reason)
}

/// A change that needs a machine or an instance that is gone.
fn resource_unavailable(reason: impl ToString) -> Failure {
    failure(PeppyFailureKind::ResourceUnavailable, reason)
}

/// The failure of a question to a peer daemon that did not answer it as
/// asked: its own error reply keeps its kind, a peer that is gone is
/// `resource_unavailable`, a peer that answers too late is `timeout`.
fn peer_failure(error: &peppylib::PeppyError, message: impl ToString) -> Failure {
    failure(crate::error::peppylib_failure_kind(error), message)
}

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
    .map_err(|error| peer_failure(&error, format!("cannot inspect `{host}`: {error}")).into())
}

/// Whether the stack runs anything beyond its root entity, which is always
/// present.
pub(crate) fn holds_nodes(node_stack: &node_stack::NodeStack) -> bool {
    node_stack.snapshot().len() > 1
}
