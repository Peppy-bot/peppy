use std::sync::Arc;
use std::time::Duration;

use config::consts::NODE_CONFIG_FILE;
use config::peppy_config::BuildSystem;
use master_node::encoding::NodeSyncRequest;
use tracing::info;

use crate::context::AppContext;
use crate::error::{Error, Result};

const CALLER_INSTANCE_ID: &str = "peppy-cli";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub fn sync_node(ctx: &Arc<AppContext>, build_system: BuildSystem) -> Result<()> {
    crate::commands::block_on(sync_node_async(ctx, build_system))
}

async fn sync_node_async(ctx: &Arc<AppContext>, build_system: BuildSystem) -> Result<()> {
    let daemon_state = ctx.read_daemon_state().map_err(|e| {
        Error::ExecutionFailed(format!(
            "Failed to read daemon state. Is the peppy daemon running? Error: {}",
            e
        ))
    })?;
    let master_node_name = daemon_state.master_node_name;

    let node_root_dir = ctx.root_dir.clone();
    let node_config_path = node_root_dir.join(NODE_CONFIG_FILE);
    if !node_config_path.exists() {
        return Err(Error::ExecutionFailed(format!(
            "Missing '{}' in node directory: {}",
            NODE_CONFIG_FILE,
            node_root_dir.display()
        )));
    }

    info!(
        "Syncing node at {} (build_system={}) via master '{}'...",
        node_root_dir.display(),
        build_system,
        master_node_name
    );

    ctx.connect().await?;
    let messenger_handle = ctx
        .messenger_handle()
        .ok_or_else(|| Error::ExecutionFailed("Failed to connect to daemon".to_string()))?;

    let request = NodeSyncRequest::new(node_root_dir).with_build_system(build_system);
    let response = request
        .poll(
            messenger_handle,
            &master_node_name,
            CALLER_INSTANCE_ID,
            &master_node_name,
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
