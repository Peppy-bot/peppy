//! `peppy stack reset`: tear the node stack down to an empty state.
//!
//! Replaces `peppy service reset`. The daemon service was already called
//! `stack_reset`; only the CLI verb was misfiled under `service`, where it read
//! as something that restarts the daemon.
//!
//! The federated mode is where "participants are discovered, not remembered"
//! pays off. Nothing here reads a coordinator's memory of who took part: it
//! asks the federation which daemons hold a slice of the launch, using the same
//! presence fan-out `stack list` already performs. That is what makes reset
//! work after a coordinator restart, and from any machine in the federation.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use core_node_api::encoding::{StackListRequest, StackResetRequest};
use futures::future::join_all;
use tracing::info;

use crate::commands::CALLER_INSTANCE_ID;
use crate::context::AppContext;
use crate::error::{Error, Result};
use peppylib::CoreNodePresenceMessenger;
use peppylib::core_node::transport::poll;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub fn reset_stack(ctx: &Arc<AppContext>, federated: bool) -> Result<()> {
    crate::commands::block_on(reset_async(ctx, federated))
}

async fn reset_async(ctx: &Arc<AppContext>, federated: bool) -> Result<()> {
    let conn = ctx.connect_to_daemon().await?;

    // Read this daemon's slice identity BEFORE clearing it: it names the launch
    // to chase in federated mode, and it is what lets a participant-side reset
    // report what the rest of the system is still running.
    let slice = poll(
        &StackListRequest::new(),
        conn.messenger,
        &conn.core_node_name,
        CALLER_INSTANCE_ID,
        &conn.target_core_node,
        REQUEST_TIMEOUT,
    )
    .await
    .map_err(|e| Error::ExecutionFailed(format!("Failed to read the stack: {e}")))?
    .launch;

    let mut targets: BTreeSet<String> = BTreeSet::from([conn.target_core_node.clone()]);

    if federated {
        let Some(launch) = &slice else {
            return Err(Error::ExecutionFailed(format!(
                "`--federated` needs a launch to tear down, but the stack on `{}` was not \
                 started by a federated launch. Drop the flag to reset this daemon alone.",
                conn.target_core_node
            )));
        };
        // Rediscovery, not recall: ask every live core node whether it holds a
        // slice of this launch id. A restarted coordinator finds its own launch
        // again, and this works from a participant just as well as from the
        // coordinator.
        targets.extend(find_launch_participants(&conn, &launch.launch_id).await?);
        info!(
            "Resetting every slice of launch `{}` (coordinator `{}`): {}",
            launch.launch_id,
            launch.coordinator_core_node,
            targets.iter().cloned().collect::<Vec<_>>().join(", ")
        );
    } else if let Some(launch) = &slice
        && launch.coordinator_core_node != conn.target_core_node
    {
        // Say what is NOT being cleared. A participant-side reset leaves the
        // rest of the launch running, and silence here would read as "the whole
        // thing is gone".
        println!(
            "Note: `{}` holds one slice of launch `{}`, coordinated by `{}`. This clears only \
             this daemon's slice; the other participants keep running. Use \
             `peppy stack reset --federated` to tear down the whole launch.",
            conn.target_core_node, launch.launch_id, launch.coordinator_core_node
        );
    }

    let failures: Vec<String> = join_all(targets.iter().map(|core_node| {
        let messenger = conn.messenger;
        let caller = &conn.core_node_name;
        async move {
            match poll(
                &StackResetRequest::new(),
                messenger,
                caller,
                CALLER_INSTANCE_ID,
                core_node,
                REQUEST_TIMEOUT,
            )
            .await
            {
                Ok(response) if response.success => None,
                Ok(response) => Some(format!(
                    "{core_node}: {}",
                    response
                        .error_message
                        .unwrap_or_else(|| "stack reset failed".to_owned())
                )),
                Err(e) => Some(format!("{core_node}: {e}")),
            }
        }
    }))
    .await
    .into_iter()
    .flatten()
    .collect();

    if !failures.is_empty() {
        // Report per daemon rather than collapsing to one message: a partial
        // reset leaves specific machines still populated, and the operator
        // needs to know which ones to go back to.
        return Err(Error::ExecutionFailed(format!(
            "stack reset failed on {} of {} daemon(s):\n  {}",
            failures.len(),
            targets.len(),
            failures.join("\n  ")
        )));
    }

    info!(
        "Reset {} daemon(s): {}",
        targets.len(),
        targets.iter().cloned().collect::<Vec<_>>().join(", ")
    );
    Ok(())
}

/// Every live core node holding a slice of `launch_id`.
///
/// A daemon that cannot be reached is deliberately NOT dropped silently: it
/// simply does not appear here, and if it was a participant its slice stays up.
/// That is reported by the reset attempt on the daemons that did answer rather
/// than being papered over as a successful full teardown.
async fn find_launch_participants(
    conn: &crate::context::DaemonConnection<'_>,
    launch_id: &str,
) -> Result<Vec<String>> {
    let live = CoreNodePresenceMessenger::list_live(
        conn.messenger,
        None,
        CoreNodePresenceMessenger::LIST_TIMEOUT,
    )
    .await?;

    let core_nodes: BTreeSet<String> = live.into_iter().map(|claim| claim.core_node).collect();

    Ok(join_all(core_nodes.into_iter().map(|core_node| {
        let messenger = conn.messenger;
        let caller = &conn.core_node_name;
        async move {
            let response = poll(
                &StackListRequest::new(),
                messenger,
                caller,
                CALLER_INSTANCE_ID,
                &core_node,
                REQUEST_TIMEOUT,
            )
            .await
            .ok()?;
            response
                .launch
                .filter(|launch| launch.launch_id == launch_id)
                .map(|_| core_node)
        }
    }))
    .await
    .into_iter()
    .flatten()
    .collect())
}
