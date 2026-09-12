//! `peppy stack remove`: one copy off the running stack.

use std::sync::Arc;
use std::time::Duration;

use core_node_api::encoding::StackBudgets;
use peppylib::core_node::transport::poll;

use super::goal::drive_stack_goal;
use crate::commands::{CALLER_INSTANCE_ID, GOAL_TIMEOUT};
use crate::context::AppContext;
use crate::error::{Error, Result};

/// How long the copy's host has to report its shutdown grace.
const HOST_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) fn remove(ctx: &Arc<AppContext>, name: config::runtime::Name) -> Result<()> {
    crate::commands::block_on(async {
        let conn = ctx.connect_to_daemon().await?;
        let stack = poll(
            &core_node_api::encoding::StackListRequest::new(),
            conn.messenger,
            &conn.core_node_name,
            CALLER_INSTANCE_ID,
            &conn.target_core_node,
            GOAL_TIMEOUT,
        )
        .await
        .map_err(|error| {
            Error::ExecutionFailed(format!("cannot inspect the stack before removal: {error}"))
        })?;
        let timeout = match stack.copies.iter().find(|copy| copy.name == name.as_str()) {
            Some(copy) => {
                let grace = if copy.core_node.as_str() == conn.target_core_node {
                    stack.shutdown_grace_secs
                } else {
                    // The host's grace sizes the watchdog. A host that does
                    // not answer in time is one the daemon drops the copy
                    // from when it is off the federation, or stops under
                    // that host's own grace when it is merely slow; the
                    // coordinator's grace stands in for it then.
                    poll(
                        &core_node_api::encoding::StackListRequest::new(),
                        conn.messenger,
                        &conn.core_node_name,
                        CALLER_INSTANCE_ID,
                        copy.core_node.as_str(),
                        HOST_PROBE_TIMEOUT,
                    )
                    .await
                    .map_or(stack.shutdown_grace_secs, |host| host.shutdown_grace_secs)
                };
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
/// the copy's instances, plus the default launch idle budget its preflight
/// runs under.
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
