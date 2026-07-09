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

/// How long the CLI waits for a `node_stop` to return: the daemon's full
/// force-kill deadline (hook grace + event-loop join + interpreter finalize),
/// plus the reap window, plus a messaging margin. Derived from the same
/// [`core_node::force_kill_deadline`] the daemon uses so the CLI can never give
/// up before a stop that actually succeeded. The single source of the formula:
/// `stop_node_async` calls it and the unit test pins it.
fn stop_request_timeout(shutdown_grace_secs: u64) -> Duration {
    core_node::force_kill_deadline(Duration::from_secs(shutdown_grace_secs))
        + core_node::TEARDOWN_REAP_BUDGET
        + STOP_MESSAGING_MARGIN
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

    // Wait long enough to outlast the daemon's full force-kill deadline (grace +
    // event-loop join + interpreter finalize) plus its reap window; otherwise a
    // deliberately long grace, or a node that legitimately takes most of that
    // window to exit, would make the CLI give up before the (successful) stop
    // returns.
    //
    // Sized from the *local* daemon's `shutdown_grace_secs`: `DaemonState`
    // only records the local generation, so a `--core-node` stop borrows the
    // local grace as its estimate of the remote daemon's. (Optional follow-up:
    // pre-fetch the remote grace via the `info` service.)
    let request_timeout = stop_request_timeout(conn.shutdown_grace_secs);

    // The service root (`to_target`) and the discovery scope both name the
    // target core node; only the bound identity stays local.
    let stop_request = NodeStopRequest::new(instance_id.clone());
    let stop_response = poll_node_stop(
        &stop_request,
        conn.messenger,
        &conn.core_node_name,
        CALLER_INSTANCE_ID,
        SenderTarget::node(&conn.target_core_node, CORE_NODE_TAG)
            .map_err(|e| Error::ExecutionFailed(format!("Failed to build sender target: {e}")))?,
        &conn.target_core_node,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_request_timeout_is_the_documented_formula() {
        // Pin the production formula itself (not a re-derived lower bound), so a
        // change to any term is caught here. Uses the default grace as the anchor.
        let grace = config::peppy_config::DEFAULT_SHUTDOWN_GRACE_SECS;
        let expected = core_node::force_kill_deadline(Duration::from_secs(grace))
            + core_node::TEARDOWN_REAP_BUDGET
            + STOP_MESSAGING_MARGIN;
        assert_eq!(stop_request_timeout(grace), expected);
    }

    #[test]
    fn cli_timeout_strictly_outlasts_the_daemon_worst_case_stop() {
        // The top link of the timeout chain: the CLI request timeout must exceed
        // the daemon's entire worst-case stop (force-kill deadline + reap) for
        // every accepted grace, with the messaging margin as the headroom. The
        // daemon's deadline > node-bounded-exit link is pinned separately in
        // core-node's `shutdown_grace_margin` test; together they give the full
        // CLI > daemon-deadline > node-exit chain. Reverting the CLI formula to
        // the old `grace + reap + margin` (dropping force_kill_deadline) would
        // fail this for any grace where the join + finalize terms matter.
        for grace_secs in 1..=600 {
            let daemon_worst_case = core_node::force_kill_deadline(Duration::from_secs(grace_secs))
                + core_node::TEARDOWN_REAP_BUDGET;
            let cli = stop_request_timeout(grace_secs);
            assert!(
                cli > daemon_worst_case,
                "CLI timeout {cli:?} for grace {grace_secs}s must outlast the daemon's \
                 worst-case stop {daemon_worst_case:?} (force-kill deadline + reap)",
            );
            assert_eq!(
                cli - daemon_worst_case,
                STOP_MESSAGING_MARGIN,
                "the headroom must be exactly the messaging margin",
            );
        }
    }
}
