//! `peppy stack remove`: one copy off the running stack.

use std::sync::Arc;
use std::time::Duration;

use config::runtime::CopyName;
use core_node_api::encoding::StackBudgets;
use futures::future::join_all;
use peppylib::core_node::transport::poll;

use super::goal::drive_stack_goal;
use crate::commands::{GOAL_TIMEOUT, caller_instance_id};
use crate::context::AppContext;
use crate::error::{Error, Result};

/// How long each machine of the copy has to report its shutdown grace.
const HOST_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Removes the copy `name`, its idle watchdog sized from the longest
/// shutdown grace of the copy's machines, read from all of them at the same
/// time, for each of its instances.
pub(super) fn remove(ctx: &Arc<AppContext>, name: CopyName) -> Result<()> {
    crate::commands::block_on(async {
        let conn = ctx.connect_to_daemon().await?;
        let stack = poll(
            &core_node_api::encoding::StackListRequest::new(),
            conn.messenger,
            &conn.core_node_name,
            caller_instance_id(),
            &conn.target_core_node,
            GOAL_TIMEOUT,
        )
        .await
        .map_err(|error| {
            Error::ExecutionFailed(format!("cannot inspect the stack before removal: {error}"))
        })?;
        let timeout = match stack.copies.iter().find(|copy| copy.name == name) {
            Some(copy) => {
                // Each machine's grace sizes the watchdog. A machine that does
                // not answer in time is one the daemon stops nothing on when
                // it is off the federation, or stops under its own grace when
                // it is merely slow; the coordinator's grace stands in for it
                // then.
                let conn = &conn;
                let fallback = stack.shutdown_grace_secs;
                let graces = join_all(copy.core_nodes().into_iter().map(|machine| async move {
                    if machine.as_str() == conn.target_core_node {
                        return fallback;
                    }
                    poll(
                        &core_node_api::encoding::StackListRequest::new(),
                        conn.messenger,
                        &conn.core_node_name,
                        caller_instance_id(),
                        machine.as_str(),
                        HOST_PROBE_TIMEOUT,
                    )
                    .await
                    .map_or(fallback, |host| host.shutdown_grace_secs)
                }))
                .await;
                let grace = graces
                    .into_iter()
                    .max()
                    .unwrap_or(stack.shutdown_grace_secs);
                removal_idle_timeout(grace, copy.instance_ids.len())
            }
            // The daemon holds no such copy and refuses the goal, well within
            // the default idle budget.
            None => core_node_api::encoding::DEFAULT_IDLE_TIMEOUT_SECS,
        };
        drive_stack_goal(
            &conn,
            &core_node_api::encoding::StackRemoveGoal::new(name),
            &StackBudgets::new(timeout, timeout, timeout, None),
        )
        .await
    })
}

/// The CLI's idle watchdog for a removal: the daemon's budget for stopping
/// the copy's instances, here each under the longest grace of the copy's
/// machines, so it is at least the daemon's, plus the default launch idle
/// budget its preflight runs under.
fn removal_idle_timeout(shutdown_grace_secs: u64, instance_count: usize) -> u64 {
    core_node::copy_removal_budget(shutdown_grace_secs, instance_count)
        .as_secs()
        .saturating_add(core_node_api::encoding::DEFAULT_IDLE_TIMEOUT_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removal_watchdog_covers_each_instances_configured_shutdown_grace() {
        let floor = core_node_api::encoding::DEFAULT_IDLE_TIMEOUT_SECS;
        assert_eq!(removal_idle_timeout(900, 0), floor);
        assert!(removal_idle_timeout(900, 4) > 3600 + floor);
        assert!(removal_idle_timeout(900, 4) > removal_idle_timeout(900, 1));
    }
}
