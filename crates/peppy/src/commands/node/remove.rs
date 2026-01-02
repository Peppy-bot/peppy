use std::sync::Arc;
use std::time::Duration;

use master_node::encoding::NodeRemoveRequest;
use tracing::info;

use super::NodeName;
use crate::context::{AppContext, DaemonState};
use crate::error::{Error, Result};

const CALLER_INSTANCE_ID: &str = "peppy-cli";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub fn remove_node(ctx: &Arc<AppContext>, node_name: NodeName, stop_instances: bool) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(remove_node_async(ctx, node_name, stop_instances))
}

async fn remove_node_async(
    ctx: &Arc<AppContext>,
    node_name: NodeName,
    stop_instances: bool,
) -> Result<()> {
    let daemon_state = DaemonState::read().map_err(|e| {
        Error::ExecutionFailed(format!(
            "Failed to read daemon state. Is the peppy daemon running? Error: {}",
            e
        ))
    })?;
    let master_node_name = daemon_state.master_node_name;

    info!(
        "Calling node_remove for '{}' on master '{}' (stop_instances={})...",
        node_name.as_str(),
        master_node_name,
        stop_instances
    );

    ctx.connect().await?;
    let messenger_handle = ctx
        .messenger_handle()
        .ok_or_else(|| Error::ExecutionFailed("Failed to connect to daemon".to_string()))?;

    let remove_request =
        NodeRemoveRequest::new(node_name.as_str()).with_stop_instances(stop_instances);
    let remove_response = remove_request
        .poll(
            messenger_handle,
            &master_node_name,
            CALLER_INSTANCE_ID,
            &master_node_name,
            REQUEST_TIMEOUT,
        )
        .await
        .map_err(|e| {
            Error::ExecutionFailed(format!("Failed to call node_remove service: {}", e))
        })?;

    if !remove_response.success {
        return Err(Error::ExecutionFailed(
            remove_response
                .error_message
                .unwrap_or_else(|| "node_remove failed with no error message".to_string()),
        ));
    }

    info!("Removed node '{}'", node_name.as_str());
    Ok(())
}
