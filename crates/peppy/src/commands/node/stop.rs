use core_node_api::encoding::NodeStopRequest;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

use crate::commands::CALLER_INSTANCE_ID;
use crate::context::AppContext;
use crate::error::{Error, Result};
use core_node_api::names::CORE_NODE_TAG;
use peppylib::core_node::transport::poll_node_stop;
use peppylib::messaging::SenderTarget;

/// Time allowed beyond the daemon's worst-case stop duration (its
/// [`core_node::force_kill_deadline`] plus [`core_node::TEARDOWN_REAP_BUDGET`]),
/// covering the messaging round-trip. The CLI must wait slightly longer than the
/// daemon can possibly take to cooperatively stop, then force-kill and reap a
/// stuck node, or it would report a timeout for a stop that actually succeeded.
const STOP_MESSAGING_MARGIN: Duration = Duration::from_secs(5);

pub fn stop_node(ctx: &Arc<AppContext>, instance_id: String) -> Result<()> {
    crate::commands::block_on(stop_node_async(ctx, instance_id))
}

async fn stop_node_async(ctx: &Arc<AppContext>, instance_id: String) -> Result<()> {
    let conn = ctx.connect_to_daemon().await?;

    info!(
        "Calling node_stop for instance_id '{}' on daemon '{}'...",
        instance_id, conn.core_node_name
    );

    // Wait long enough to outlast the daemon's full force-kill deadline (grace +
    // event-loop join + interpreter finalize) plus its reap window; otherwise a
    // deliberately long grace, or a node that legitimately takes most of that
    // window to exit, would make the CLI give up before the (successful) stop
    // returns.
    let request_timeout =
        core_node::force_kill_deadline(Duration::from_secs(conn.shutdown_grace_secs))
            + core_node::TEARDOWN_REAP_BUDGET
            + STOP_MESSAGING_MARGIN;

    let stop_request = NodeStopRequest::new(instance_id.clone());
    let stop_response = poll_node_stop(
        &stop_request,
        conn.messenger,
        &conn.core_node_name,
        CALLER_INSTANCE_ID,
        SenderTarget::node(&conn.core_node_name, CORE_NODE_TAG)
            .map_err(|e| Error::ExecutionFailed(format!("Failed to build sender target: {e}")))?,
        request_timeout,
    )
    .await
    .map_err(|e| Error::ExecutionFailed(format!("Failed to call node_stop service: {}", e)))?;

    if !stop_response.success {
        return Err(Error::ExecutionFailed(
            stop_response
                .error_message
                .unwrap_or_else(|| "node_stop failed with no error message".to_string()),
        ));
    }

    // The node was stopped either way; warn the user when it had to be
    // force-killed (it ignored the cooperative shutdown within the grace
    // period) so it is not mistaken for a clean graceful exit.
    if stop_response.force_killed {
        warn!(
            "Node instance '{}' did not shut down gracefully within the grace period and was force-killed",
            instance_id
        );
    } else {
        info!("Stopped node instance '{}'", instance_id);
    }
    Ok(())
}
