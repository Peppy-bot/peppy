use core_node::encoding::prelude::*;
use core_node_api::encoding::{
    NodeBuildFeedback, NodeBuildGoal, NodeBuildGoalResponse, NodeBuildResult,
};
use peppylib::MessengerHandle;
use std::sync::Arc;
use tracing::info;

use super::TimeoutConfig;
use super::env::caller_env_overrides;
use crate::commands::{CALLER_INSTANCE_ID, GOAL_TIMEOUT};
use crate::context::AppContext;
use crate::error::{Error, Result};

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

    let build_result = crate::commands::action_poll::run_action_with_feedback::<
        NodeBuildGoalResponse,
        NodeBuildFeedback,
        NodeBuildResult,
    >(messenger, &mut action_handle, timeouts, "node_build")
    .await?;

    info!(
        "Built node {}:{}. Artifact: {}",
        node_name,
        node_tag,
        build_result.artifact_path.to_string_lossy()
    );

    Ok(())
}
