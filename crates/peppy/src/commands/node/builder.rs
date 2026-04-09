use core_node::encoding::{
    NodeBuildFeedback, NodeBuildGoal, NodeBuildGoalResponse, NodeBuildResult,
};
use peppylib::MessengerHandle;
use std::sync::Arc;
use tracing::info;

use super::TimeoutConfig;
use super::env::caller_env_overrides;
use crate::commands::{CALLER_INSTANCE_ID, GOAL_TIMEOUT, SCROLLING_OUTPUT_LINES};
use crate::context::AppContext;
use crate::error::{Error, Result};
use crate::terminal::ScrollingOutput;

pub struct BuildNodeParams {
    pub node_name: String,
    pub node_tag: String,
    pub timeouts: TimeoutConfig,
    pub force: bool,
}

pub fn build_node(ctx: &Arc<AppContext>, params: BuildNodeParams) -> Result<()> {
    crate::commands::block_on(build_node_async_with_connect(ctx, params))
}

async fn build_node_async_with_connect(
    ctx: &Arc<AppContext>,
    params: BuildNodeParams,
) -> Result<()> {
    let conn = ctx.connect_to_daemon().await?;
    build_node_async(
        conn.messenger,
        &conn.core_node_name,
        &params.node_name,
        &params.node_tag,
        &params.timeouts,
        params.force,
    )
    .await
}

/// Sends a `node_build` goal and polls it to completion. Used by both the
/// CLI `node build` command and `node add --build` chaining.
pub async fn build_node_async(
    messenger: &MessengerHandle,
    core_node_name: &str,
    node_name: &str,
    node_tag: &str,
    timeouts: &TimeoutConfig,
    force: bool,
) -> Result<()> {
    info!("Building node {}:{}...", node_name, node_tag);

    let mut goal = NodeBuildGoal::new(node_name, node_tag, timeouts.max_secs)
        .with_env_vars(caller_env_overrides());
    if force {
        goal = goal.with_force(true);
    }

    let mut action_handle = goal
        .send_goal(
            messenger,
            core_node_name,
            CALLER_INSTANCE_ID,
            Some(core_node_name),
            None,
            GOAL_TIMEOUT,
        )
        .await
        .map_err(|e| Error::ExecutionFailed(format!("Failed to send node_build goal: {}", e)))?;

    let goal_response_payload = action_handle.goal_response().payload();
    let goal_response = NodeBuildGoalResponse::decode(&goal_response_payload)
        .map_err(|e| Error::ExecutionFailed(format!("Failed to decode goal response: {}", e)))?;

    if !goal_response.accepted {
        return Err(Error::ExecutionFailed(format!(
            "Goal rejected: {}",
            goal_response
                .rejection_reason
                .unwrap_or_else(|| "unknown reason".to_string())
        )));
    }

    info!("Log file: {}", goal_response.log_path.display());

    let mut scrolling_output = ScrollingOutput::new(SCROLLING_OUTPUT_LINES);

    let build_result = crate::commands::action_poll::poll_action_to_completion(
        messenger,
        &mut action_handle,
        timeouts,
        &mut scrolling_output,
        |payload, output| {
            if let Ok(feedback) = NodeBuildFeedback::decode(payload) {
                output.add_line(&feedback.line, feedback.is_stderr());
            }
        },
        |payload| match NodeBuildResult::decode(payload) {
            Ok(result) => Ok(Some(result)),
            Err(err) => {
                if peppylib::encoding::is_result_pending(payload) {
                    Ok(None)
                } else {
                    Err(format!("Failed to decode node_build result: {err}"))
                }
            }
        },
    )
    .await?;

    scrolling_output.clear();

    if !build_result.success {
        return Err(Error::ExecutionFailed(
            build_result
                .error_message
                .unwrap_or_else(|| "node_build failed with no error message".to_string()),
        ));
    }

    info!(
        "Built node {}:{}. Artifact: {}",
        node_name,
        node_tag,
        build_result.artifact_path.to_string_lossy()
    );

    Ok(())
}
