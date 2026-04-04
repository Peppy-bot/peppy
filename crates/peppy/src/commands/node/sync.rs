use std::sync::Arc;
use std::time::Duration;

use core_node::encoding::NodeSyncRequest;
use tracing::info;

use super::source::resolve_node_root_dir;
use crate::context::AppContext;
use crate::error::{Error, Result};

const CALLER_INSTANCE_ID: &str = "peppy-cli";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub fn sync_node(ctx: &Arc<AppContext>) -> Result<()> {
    crate::commands::block_on(sync_node_async(ctx))
}

async fn sync_node_async(ctx: &Arc<AppContext>) -> Result<()> {
    let daemon_state = ctx.read_daemon_state()?;
    let core_node_name = daemon_state.core_node_name;
    let git_hash = daemon_state.git_hash;

    // If the current directory doesn't contain a valid root config (e.g. we're
    // inside a variant subdirectory), walk up to find the root node directory.
    let node_root_dir = resolve_node_root_dir(&ctx.root_dir)?;

    info!(
        "Syncing node from {} via daemon '{}'...",
        node_root_dir.display(),
        core_node_name
    );

    ctx.connect().await?;
    let messenger_handle = ctx
        .messenger_handle()
        .ok_or_else(|| Error::ExecutionFailed("Failed to connect to daemon".to_string()))?;

    let request = NodeSyncRequest::new(node_root_dir, git_hash);
    let response = request
        .poll(
            messenger_handle,
            &core_node_name,
            CALLER_INSTANCE_ID,
            &core_node_name,
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
