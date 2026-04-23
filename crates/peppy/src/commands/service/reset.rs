use std::sync::Arc;
use std::time::Duration;

use core_node_api::encoding::NodeResetRequest;
use tracing::info;

use crate::commands::{CALLER_INSTANCE_ID, Command};
use crate::context::AppContext;
use crate::error::{Error, Result};
use core_node::transport::poll_node_reset;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub struct ResetCommand {}

impl Command for ResetCommand {
    fn execute(self, ctx: &Arc<AppContext>) -> Result<()> {
        crate::commands::block_on(reset_async(ctx))
    }
}

async fn reset_async(ctx: &Arc<AppContext>) -> Result<()> {
    let conn = ctx.connect_to_daemon().await?;

    info!(
        "Resetting node stack on daemon '{}'...",
        conn.core_node_name
    );

    let response = poll_node_reset(
        &NodeResetRequest::new(),
        conn.messenger,
        &conn.core_node_name,
        CALLER_INSTANCE_ID,
        &conn.core_node_name,
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
