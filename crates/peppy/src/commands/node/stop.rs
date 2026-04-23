use core_node_api::encoding::NodeStopRequest;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

use crate::commands::CALLER_INSTANCE_ID;
use crate::context::AppContext;
use crate::error::{Error, Result};
use core_node::transport::poll_node_stop;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub fn stop_node(ctx: &Arc<AppContext>, instance_id: String) -> Result<()> {
    crate::commands::block_on(stop_node_async(ctx, instance_id))
}

async fn stop_node_async(ctx: &Arc<AppContext>, instance_id: String) -> Result<()> {
    let conn = ctx.connect_to_daemon().await?;

    info!(
        "Calling node_stop for instance_id '{}' on daemon '{}'...",
        instance_id, conn.core_node_name
    );

    let stop_request = NodeStopRequest::new(instance_id.clone());
    let stop_response = poll_node_stop(
        &stop_request,
        conn.messenger,
        &conn.core_node_name,
        CALLER_INSTANCE_ID,
        &conn.core_node_name,
        &conn.core_node_name,
        REQUEST_TIMEOUT,
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

    info!("Stopped node instance '{}'", instance_id);
    Ok(())
}
