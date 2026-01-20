use master_node::encoding::NodeStopRequest;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

use crate::context::AppContext;
use crate::error::{Error, Result};

const CALLER_INSTANCE_ID: &str = "peppy-cli";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub fn stop_node(ctx: &Arc<AppContext>, instance_id: String) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(stop_node_async(ctx, instance_id))
}

async fn stop_node_async(ctx: &Arc<AppContext>, instance_id: String) -> Result<()> {
    let daemon_state = ctx.read_daemon_state().map_err(|e| {
        Error::ExecutionFailed(format!(
            "Failed to read daemon state. Is the peppy daemon running? Error: {}",
            e
        ))
    })?;
    let master_node_name = daemon_state.master_node_name;

    info!(
        "Calling node_stop for instance_id '{}' on master '{}'...",
        instance_id, master_node_name
    );

    ctx.connect().await?;
    let messenger_handle = ctx
        .messenger_handle()
        .ok_or_else(|| Error::ExecutionFailed("Failed to connect to daemon".to_string()))?;

    let stop_request = NodeStopRequest::new(instance_id.clone());
    let stop_response = stop_request
        .poll(
            messenger_handle,
            &master_node_name,
            CALLER_INSTANCE_ID,
            &master_node_name,
            &master_node_name,
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
