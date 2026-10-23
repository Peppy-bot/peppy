use core_node_api::encoding::NodeStopRequest;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

use crate::commands::caller_instance_id;
use crate::context::AppContext;
use crate::error::{Error, Result};
use core_node_api::names::CORE_NODE_TAG;
use peppylib::core_node::transport::poll_node_stop;
use peppylib::messaging::SenderTarget;

use super::{daemon_wait, warn_force_killed};

/// How long the CLI waits for a `node_stop` to return: the daemon's bound on
/// a stop ([`core_node::node_stop_bound`]) plus a messaging margin. The
/// single source of the formula: `stop_node_async` calls it and the unit
/// test pins it.
fn stop_request_timeout(shutdown_grace_secs: u64) -> Duration {
    daemon_wait(core_node::node_stop_bound(Duration::from_secs(
        shutdown_grace_secs,
    )))
}

pub fn stop_node(ctx: &Arc<AppContext>, instance_id: String) -> Result<()> {
    crate::commands::block_on(stop_node_async(ctx, instance_id))
}

async fn stop_node_async(ctx: &Arc<AppContext>, instance_id: String) -> Result<()> {
    let conn = ctx.connect_to_daemon().await?;

    info!(
        "Calling node_stop for instance_id '{}' on daemon '{}'...",
        instance_id, conn.target_core_node
    );

    // Long enough to outlast the daemon's worst-case stop (see
    // `stop_request_timeout`), so a long grace, or a node that takes most of
    // its window to exit, does not make the CLI give up before the stop
    // returns.
    //
    // Sized from the *local* daemon's `shutdown_grace_secs`: `DaemonState`
    // only records the local generation, so a `--core-node` stop borrows the
    // local grace as its estimate of the remote daemon's.
    let request_timeout = stop_request_timeout(conn.shutdown_grace_secs);

    // The service root (`to_target`) and the discovery scope both name the
    // target core node; only the bound identity stays local.
    let stop_request = NodeStopRequest::new(instance_id.clone());
    let stop_response = poll_node_stop(
        &stop_request,
        conn.messenger,
        &conn.core_node_name,
        caller_instance_id(),
        SenderTarget::node(&conn.target_core_node, CORE_NODE_TAG)
            .map_err(|e| Error::ExecutionFailed(format!("Failed to build sender target: {e}")))?,
        &conn.target_core_node,
        request_timeout,
    )
    .await
    .map_err(|e| crate::commands::request_error("Failed to stop the node instance", e))?;

    if stop_response.force_killed {
        warn_force_killed(&instance_id);
    } else {
        info!("Stopped node instance '{}'", instance_id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::DAEMON_MESSAGING_MARGIN;
    use super::*;

    #[test]
    fn cli_timeout_strictly_outlasts_the_daemon_worst_case_stop() {
        // The top link of the timeout chain: the CLI request timeout must exceed
        // the daemon's bound on a stop (`node_stop_bound`, which holds
        // the force-kill deadline) for every accepted grace, with the messaging
        // margin as the headroom. The daemon's deadline > node-bounded-exit
        // link is pinned separately in core-node's `shutdown_grace_margin`
        // test; together they give the full CLI > daemon-deadline > node-exit
        // chain.
        for grace_secs in 1..=600 {
            let grace = Duration::from_secs(grace_secs);
            let daemon_worst_case = core_node::node_stop_bound(grace);
            let cli = stop_request_timeout(grace_secs);
            assert!(
                daemon_worst_case > core_node::force_kill_deadline(grace),
                "the daemon's stop bound for grace {grace_secs}s holds its force-kill deadline",
            );
            assert!(
                cli > daemon_worst_case,
                "CLI timeout {cli:?} for grace {grace_secs}s must outlast the daemon's \
                 worst-case stop {daemon_worst_case:?}",
            );
            assert_eq!(
                cli - daemon_worst_case,
                DAEMON_MESSAGING_MARGIN,
                "the headroom must be exactly the messaging margin",
            );
        }
    }
}
