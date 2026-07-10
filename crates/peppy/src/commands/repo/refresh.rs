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
use peppylib::core_node::transport::send_goal;

const IDLE_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 3600;

pub(super) fn repo_refresh(ctx: &Arc<AppContext>) -> Result<()> {
    crate::commands::block_on(repo_refresh_async(ctx))
}

async fn repo_refresh_async(ctx: &Arc<AppContext>) -> Result<()> {
    let conn = ctx.connect_to_daemon().await?;

    let mut action_handle = send_goal(
        &RepoRefreshGoal,
        conn.messenger,
        &conn.core_node_name,
        CALLER_INSTANCE_ID,
        Some(&conn.target_core_node),
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
                output.add_line(&format_refresh_line(&feedback));
            }
        },
        |payload| {
            RepoRefreshResult::decode(payload)
                .map_err(|err| format!("Failed to decode repo refresh result: {err}"))
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
        "Repository refresh complete. {} node(s), {} launcher(s), {} contract(s), {} pairing(s) found.",
        result.total_nodes_found,
        result.total_launchers_found,
        result.total_contracts_found,
        result.total_pairings_found
    );
    Ok(())
}

fn format_refresh_line(feedback: &RepoRefreshFeedback) -> String {
    match feedback {
        RepoRefreshFeedback::Progress { message } => message.clone(),
        RepoRefreshFeedback::Excluded {
            source_type,
            identity,
        } => format!("Excluded {} ({})", identity, source_type),
        RepoRefreshFeedback::Discovered {
            kind,
            item_name,
            item_tag,
            source_type,
            path,
            ..
        } if item_tag.is_empty() => {
            format!("Found {} {} ({}, {})", kind, item_name, source_type, path)
        }
        RepoRefreshFeedback::Discovered {
            kind,
            item_name,
            item_tag,
            source_type,
            path,
            ..
        } => format!(
            "Found {} {}:{} ({}, {})",
            kind, item_name, item_tag, source_type, path
        ),
    }
}
