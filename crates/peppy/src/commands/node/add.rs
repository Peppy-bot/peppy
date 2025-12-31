use config::node::NodeConfigParser;
use config::runtime::RuntimeConfig;
use master_node::encoding::NodeAddRequest;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

use super::run::start_instance_async;
use crate::commands::serve::DaemonState;
use crate::context::AppContext;
use crate::error::{Error, Result};

const CALLER_INSTANCE_ID: &str = "peppy-cli";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub fn add_node(
    ctx: &Arc<AppContext>,
    peppy_json5: PathBuf,
    run: bool,
    args: Vec<(String, String)>,
    instance_id: Option<String>,
) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(add_node_async(ctx, peppy_json5, run, args, instance_id))
}

async fn add_node_async(
    ctx: &Arc<AppContext>,
    peppy_json5: PathBuf,
    run: bool,
    args: Vec<(String, String)>,
    instance_id: Option<String>,
) -> Result<()> {
    let daemon_state = DaemonState::read().map_err(|e| {
        Error::ExecutionFailed(format!(
            "Failed to read daemon state. Is the peppy daemon running? Error: {}",
            e
        ))
    })?;
    let master_node_name = daemon_state.master_node_name;

    // Parse node config to discover node name/tag and validate it.
    let node_config = NodeConfigParser::from_path(&peppy_json5).map_err(Error::PeppyConfig)?;
    let node_name = node_config.manifest.name.as_str().to_string();
    let node_tag = node_config.manifest.tag.clone();

    // Read the raw config as content to send to the master node.
    let peppy_json5_content = std::fs::read_to_string(&peppy_json5)?;
    let from_dir = peppy_json5
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| ctx.root_dir.clone());

    info!(
        "Calling node_add for {}:{} on master '{}'...",
        node_name, node_tag, master_node_name
    );

    ctx.connect().await?;
    let messenger_handle = ctx
        .messenger_handle()
        .ok_or_else(|| Error::ExecutionFailed("Failed to connect to daemon".to_string()))?;

    let add_request = NodeAddRequest::new(peppy_json5_content, from_dir);
    let add_response = add_request
        .poll(
            messenger_handle,
            &master_node_name,
            CALLER_INSTANCE_ID,
            &master_node_name,
            REQUEST_TIMEOUT,
        )
        .await
        .map_err(|e| Error::ExecutionFailed(format!("Failed to call node_add service: {}", e)))?;

    if !add_response.success {
        return Err(Error::ExecutionFailed(
            add_response
                .error_message
                .unwrap_or_else(|| "node_add failed with no error message".to_string()),
        ));
    }

    info!("Added node {}:{} to the node stack", node_name, node_tag);

    if !run {
        return Ok(());
    }

    // For `--run`, spawn an instance using the shared start logic
    let codegen_md5 =
        RuntimeConfig::generate_peppy_config_md5(&peppy_json5).map_err(Error::PeppyConfig)?;

    start_instance_async(
        messenger_handle,
        &master_node_name,
        &node_name,
        &node_tag,
        &args,
        instance_id,
        &codegen_md5,
    )
    .await?;

    Ok(())
}
