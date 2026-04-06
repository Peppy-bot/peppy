use std::sync::Arc;
use std::time::Duration;

use core_node::encoding::NodeSyncRequest;
use tracing::info;

use super::source::resolve_node_root_dir;
use crate::commands::CALLER_INSTANCE_ID;
use crate::context::AppContext;
use crate::error::{Error, Result};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub fn sync_node(ctx: &Arc<AppContext>) -> Result<()> {
    crate::commands::block_on(sync_node_async(ctx))
}

async fn sync_node_async(ctx: &Arc<AppContext>) -> Result<()> {
    // If the current directory doesn't contain a valid root config (e.g. we're
    // inside a variant subdirectory), walk up to find the root node directory.
    let node_root_dir = resolve_node_root_dir(&ctx.root_dir)?;
    let conn = ctx.connect_to_daemon().await?;

    info!(
        "Syncing node from {} via daemon '{}'...",
        node_root_dir.display(),
        conn.core_node_name
    );

    let request = NodeSyncRequest::new(node_root_dir, conn.git_hash);
    let response = request
        .poll(
            conn.messenger,
            &conn.core_node_name,
            CALLER_INSTANCE_ID,
            &conn.core_node_name,
            REQUEST_TIMEOUT,
        )
        .await
        .map_err(|e| {
            Error::ExecutionFailed(format!("Failed to call node_generate service: {}", e))
        })?;

    if !response.success {
        let msg = if response.error_message.trim().is_empty() {
            "node_generate failed with no error message".to_string()
        } else {
            response.error_message
        };
        return Err(Error::ExecutionFailed(msg));
    }

    info!("Synced node interfaces successfully");
    Ok(())
}
