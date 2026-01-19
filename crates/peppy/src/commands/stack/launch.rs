use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use config::peppy_config::PeppyLauncherParser;
use config::runtime::LauncherRuntimeConfig;
use master_node::encoding::LaunchRequest;
use tracing::info;

use crate::context::AppContext;
use crate::daemon_state::DaemonState;
use crate::error::{Error, Result};

const CALLER_INSTANCE_ID: &str = "peppy-cli";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub fn launch(ctx: &Arc<AppContext>, launcher_config_path: PathBuf) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(launch_async(ctx, launcher_config_path))
}

async fn launch_async(ctx: &Arc<AppContext>, launcher_config_path: PathBuf) -> Result<()> {
    let daemon_state = DaemonState::read().map_err(|e| {
        Error::ExecutionFailed(format!(
            "Failed to read daemon state. Is the peppy daemon running? Error: {}",
            e
        ))
    })?;
    let master_node_name = daemon_state.master_node_name;

    PeppyLauncherParser::from_path(&launcher_config_path).map_err(Error::PeppyConfig)?;
    let peppy_launcher_json5 = std::fs::read_to_string(&launcher_config_path)?;
    let nodes_directory = launcher_config_path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| ctx.root_dir.clone());

    info!(
        "Calling launcher on master '{}' with nodes_directory={}",
        master_node_name,
        nodes_directory.display()
    );

    ctx.connect().await?;
    let messenger_handle = ctx
        .messenger_handle()
        .ok_or_else(|| Error::ExecutionFailed("Failed to connect to daemon".to_string()))?;

    let (messaging_host, messaging_port) = messenger_handle
        .messaging_endpoint()
        .await
        .unwrap_or_else(|| {
            (
                config::consts::DEFAULT_MESSAGING_HOST.to_string(),
                daemon_state.messaging_port,
            )
        });

    let launcher_runtime_config = LauncherRuntimeConfig::new(messaging_host, messaging_port);
    let launcher_runtime_config_json =
        serde_json::to_string(&launcher_runtime_config).map_err(|e| {
            Error::ExecutionFailed(format!("Failed to serialize runtime config: {}", e))
        })?;

    let request = LaunchRequest::new(
        peppy_launcher_json5,
        nodes_directory,
        launcher_runtime_config_json,
    );
    let response = request
        .poll(
            messenger_handle,
            &master_node_name,
            CALLER_INSTANCE_ID,
            &master_node_name,
            None,
            REQUEST_TIMEOUT,
        )
        .await
        .map_err(|e| Error::ExecutionFailed(format!("Failed to call launcher service: {}", e)))?;

    if !response.success {
        return Err(Error::ExecutionFailed(response.error_message));
    }

    info!("Launch configuration applied successfully");
    Ok(())
}
