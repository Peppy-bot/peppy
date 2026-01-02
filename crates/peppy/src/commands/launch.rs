use super::Command;
use crate::context::{AppContext, DaemonState};
use crate::error::{Error, Result};
use config::peppy_config::PeppyLauncherParser;
use master_node::encoding::LauncherRequest;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

const CALLER_INSTANCE_ID: &str = "peppy-cli";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub struct LaunchCommand {
    /// Path to the launch file
    pub launcher_config_path: PathBuf,
}

impl Command for LaunchCommand {
    fn execute(self, ctx: &Arc<AppContext>) -> Result<()> {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(execute_async(self, ctx))
    }
}

async fn execute_async(command: LaunchCommand, ctx: &Arc<AppContext>) -> Result<()> {
    let daemon_state = DaemonState::read().map_err(|e| {
        Error::ExecutionFailed(format!(
            "Failed to read daemon state. Is the peppy daemon running? Error: {}",
            e
        ))
    })?;
    let master_node_name = daemon_state.master_node_name;

    PeppyLauncherParser::from_path(&command.launcher_config_path).map_err(Error::PeppyConfig)?;
    let peppy_launcher_json5 = std::fs::read_to_string(&command.launcher_config_path)?;
    let nodes_directory = command
        .launcher_config_path
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

    let request = LauncherRequest::new(peppy_launcher_json5, nodes_directory);
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
