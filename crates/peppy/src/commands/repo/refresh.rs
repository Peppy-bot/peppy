use std::sync::Arc;

use core_node_api::encoding::{
    RepoRefreshFeedback, RepoRefreshGoal, RepoRefreshGoalResponse, RepoRefreshResult,
};
use tracing::info;

use crate::commands::action_poll::poll_action_to_completion;
use crate::commands::node::TimeoutConfig;
use crate::commands::{CALLER_INSTANCE_ID, GOAL_TIMEOUT, SCROLLING_OUTPUT_LINES};
use crate::context::AppContext;
use crate::error::{Error, Result};
use crate::terminal::ScrollingOutput;
use peppylib::core_node::transport::send_repo_refresh;

const IDLE_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 3600;

pub(super) fn repo_refresh(ctx: &Arc<AppContext>) -> Result<()> {
    crate::commands::block_on(repo_refresh_async(ctx))
}

async fn repo_refresh_async(ctx: &Arc<AppContext>) -> Result<()> {
    let conn = ctx.connect_to_daemon().await?;

    let mut action_handle = send_repo_refresh(
        &RepoRefreshGoal,
        conn.messenger,
        &conn.core_node_name,
        CALLER_INSTANCE_ID,
        Some(&conn.core_node_name),
        None,
        GOAL_TIMEOUT,
    )
    .await
    .map_err(|e| Error::ExecutionFailed(format!("Failed to send repo refresh goal: {}", e)))?;

    let goal_response_payload = action_handle.goal_response().payload();
    let goal_response = RepoRefreshGoalResponse::decode(&goal_response_payload)
        .map_err(|e| Error::ExecutionFailed(format!("Failed to decode goal response: {}", e)))?;

    if !goal_response.accepted {
        return Err(Error::ExecutionFailed(format!(
            "Repo refresh rejected: {}",
            goal_response
                .rejection_reason
                .unwrap_or_else(|| "unknown reason".to_string())
        )));
    }

    info!("Refreshing repositories...");

    let timeouts = TimeoutConfig {
        idle_secs: IDLE_TIMEOUT_SECS,
        max_secs: MAX_TIMEOUT_SECS,
    };
    let mut scrolling_output = ScrollingOutput::new(SCROLLING_OUTPUT_LINES);

    let result = poll_action_to_completion::<RepoRefreshResult>(
        conn.messenger,
        &mut action_handle,
        &timeouts,
        &mut scrolling_output,
        |payload, output| {
            if let Ok(feedback) = RepoRefreshFeedback::decode(payload) {
                output.add_line(&format_refresh_line(&feedback), false);
            }
        },
        |payload| match RepoRefreshResult::decode(payload) {
            Ok(result) => Ok(Some(result)),
            Err(err) => {
                if peppylib::encoding::is_result_pending(payload) {
                    Ok(None)
                } else {
                    Err(format!("Failed to decode repo refresh result: {err}"))
                }
            }
        },
    )
    .await?;

    scrolling_output.clear();

    if !result.success {
        return Err(Error::ExecutionFailed(format!(
            "Repository refresh failed: {}",
            result
                .error_message
                .unwrap_or_else(|| "unknown error".to_string())
        )));
    }

    info!(
        "Repository refresh complete. {} node(s) found.",
        result.total_nodes_found
    );
    Ok(())
}

fn format_refresh_line(feedback: &RepoRefreshFeedback) -> String {
    if !feedback.status_message.is_empty() {
        feedback.status_message.clone()
    } else if feedback.excluded {
        format!("Excluded {} ({})", feedback.path, feedback.source_type)
    } else if feedback.variants.is_empty() {
        format!(
            "Found {}:{} ({}, {})",
            feedback.node_name, feedback.node_tag, feedback.source_type, feedback.path
        )
    } else {
        format!(
            "Found {}:{} ({}, {}) [variants: {}]",
            feedback.node_name,
            feedback.node_tag,
            feedback.source_type,
            feedback.path,
            feedback.variants.join(", ")
        )
    }
}
