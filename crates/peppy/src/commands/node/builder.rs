use core_node_api::ActionId;
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
use peppylib::core_node::transport::send_goal;

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
    build_node_on(
        conn.messenger,
        &conn.core_node_name,
        &conn.target_core_node,
        &params.node_name,
        &params.node_tag,
        &params.timeouts,
        params.force,
    )
    .await
}

/// Sends a `node_build` goal and polls it to completion, addressed to the
/// caller's own core node. Kept on the single-name signature for callers that
/// build where they are bound (integration tests included); anything honoring
/// a `--core-node` override goes through [`build_node_on`].
pub async fn build_node_async(
    messenger: &MessengerHandle,
    core_node_name: &str,
    node_name: &str,
    node_tag: &str,
    timeouts: &TimeoutConfig,
    force: bool,
) -> Result<()> {
    build_node_on(
        messenger,
        core_node_name,
        core_node_name,
        node_name,
        node_tag,
        timeouts,
        force,
    )
    .await
}

/// Like [`build_node_async`] but with the caller identity and the build's
/// host split: `bound_core_node` is the local daemon (the sender address the
/// goal rides under), `target_core_node` the daemon that runs the build. They
/// coincide without a `--core-node` override.
pub(crate) async fn build_node_on(
    messenger: &MessengerHandle,
    bound_core_node: &str,
    target_core_node: &str,
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

    let mut action_handle = send_goal(
        &goal,
        messenger,
        bound_core_node,
        CALLER_INSTANCE_ID,
        Some(target_core_node),
        GOAL_TIMEOUT,
    )
    .await
    .map_err(|e| Error::ExecutionFailed(format!("Failed to send node_build goal: {}", e)))?;

    let build_result = crate::commands::action_poll::run_action_with_feedback::<
        NodeBuildGoalResponse,
        NodeBuildFeedback,
        NodeBuildResult,
    >(
        messenger,
        &mut action_handle,
        timeouts,
        ActionId::NodeBuild.name(),
    )
    .await?;

    info!(
        "Built node {}:{}. Artifact: {}",
        node_name,
        node_tag,
        build_result.artifact_path.to_string_lossy()
    );

    Ok(())
}
