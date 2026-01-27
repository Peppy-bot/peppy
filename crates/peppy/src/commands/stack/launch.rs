use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use config::peppy_config::PeppyLauncherParser;
use config::runtime::LauncherRuntimeConfig;
use master_node::encoding::{LaunchGoal, LaunchGoalResponse, LaunchResult};
use tracing::info;

use crate::context::AppContext;
use crate::error::{Error, Result};

const CALLER_INSTANCE_ID: &str = "peppy-cli";
const GOAL_TIMEOUT: Duration = Duration::from_secs(30);
const RESULT_TIMEOUT: Duration = Duration::from_secs(300);

pub fn launch(ctx: &Arc<AppContext>, launcher_config_path: PathBuf) -> Result<()> {
    crate::commands::block_on(launch_async(ctx, launcher_config_path))
}

async fn launch_async(ctx: &Arc<AppContext>, launcher_config_path: PathBuf) -> Result<()> {
    let daemon_state = ctx.read_daemon_state().map_err(|e| {
        Error::ExecutionFailed(format!(
            "Failed to read daemon state. Is the peppy daemon running? Error: {}",
            e
        ))
    })?;
    let master_node_name = daemon_state.master_node_name;
    let git_hash = daemon_state.git_hash;

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

    let launcher_runtime_config =
        LauncherRuntimeConfig::new(messaging_host, messaging_port, git_hash);
    let launcher_runtime_config_json =
        serde_json::to_string(&launcher_runtime_config).map_err(|e| {
            Error::ExecutionFailed(format!("Failed to serialize runtime config: {}", e))
        })?;

    let goal = LaunchGoal::new(
        peppy_launcher_json5,
        nodes_directory,
        launcher_runtime_config_json,
    );

    let action_handle = goal
        .send_goal(
            messenger_handle,
            &master_node_name,
            CALLER_INSTANCE_ID,
            None,
            None,
            GOAL_TIMEOUT,
        )
        .await
        .map_err(|e| Error::ExecutionFailed(format!("Failed to send launch goal: {}", e)))?;

    let goal_response = LaunchGoalResponse::decode(
        &action_handle.goal_response().payload().to_bytes(),
    )
    .map_err(|e| Error::ExecutionFailed(format!("Failed to decode goal response: {}", e)))?;

    if !goal_response.accepted {
        let reason = goal_response
            .rejection_reason
            .unwrap_or_else(|| "unknown reason".to_string());
        return Err(Error::ExecutionFailed(format!(
            "Launch goal rejected: {}",
            reason
        )));
    }

    info!(
        "Launch goal accepted, log file: {}",
        goal_response.log_path.display()
    );

    // TODO: Subscribe to feedback and stream output to console

    let result = LaunchResult::request_result(messenger_handle, &action_handle, RESULT_TIMEOUT)
        .await
        .map_err(|e| Error::ExecutionFailed(format!("Failed to get launch result: {}", e)))?;

    if !result.success {
        let error_msg = result
            .error_message
            .unwrap_or_else(|| "unknown error".to_string());
        return Err(Error::ExecutionFailed(format!(
            "Launch failed: {}. Log file: {}",
            error_msg,
            result.log_path.display()
        )));
    }

    info!("Launch configuration applied successfully");
    Ok(())
}
