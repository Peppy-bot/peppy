use std::sync::Arc;
use std::time::Duration;

use daemon_node::encoding::NodeResetRequest;
use tracing::info;

use crate::commands::Command;
use crate::context::AppContext;
use crate::error::{Error, Result};

const CALLER_INSTANCE_ID: &str = "peppy-cli";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub struct ResetCommand {}

impl Command for ResetCommand {
    fn execute(self, ctx: &Arc<AppContext>) -> Result<()> {
        crate::commands::block_on(reset_async(ctx))
    }
}

async fn reset_async(ctx: &Arc<AppContext>) -> Result<()> {
    let daemon_state = ctx.read_daemon_state().map_err(|e| {
        Error::ExecutionFailed(format!(
            "Failed to read daemon state. Is the peppy daemon running? Error: {}",
            e
        ))
    })?;
    let daemon_node_name = daemon_state.daemon_node_name;

    ctx.connect().await?;
    let messenger_handle = ctx
        .messenger_handle()
        .ok_or_else(|| Error::ExecutionFailed("Failed to connect to daemon".to_string()))?;

    info!("Resetting node stack on daemon '{}'...", daemon_node_name);

    let response = NodeResetRequest::new()
        .poll(
            messenger_handle,
            &daemon_node_name,
            CALLER_INSTANCE_ID,
            &daemon_node_name,
            REQUEST_TIMEOUT,
        )
        .await
        .map_err(|e| Error::ExecutionFailed(format!("Failed to call node_reset service: {}", e)))?;

    if !response.success {
        return Err(Error::ExecutionFailed(
            response
                .error_message
                .unwrap_or_else(|| "Node stack reset failed".to_string()),
        ));
    }

    info!("Node stack reset successfully");
    Ok(())
}
