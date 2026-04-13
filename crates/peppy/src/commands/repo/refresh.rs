use std::sync::Arc;
use std::time::Duration;

use core_node::encoding::{
    RepoRefreshFeedback, RepoRefreshGoal, RepoRefreshGoalResponse, RepoRefreshResult,
};
use peppylib::ActionMessenger;
use tracing::info;

use crate::commands::{CALLER_INSTANCE_ID, GOAL_TIMEOUT};
use crate::context::AppContext;
use crate::error::{Error, Result};

const FEEDBACK_TIMEOUT: Duration = Duration::from_millis(100);
const RESULT_POLL_TIMEOUT: Duration = Duration::from_millis(200);
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);

pub(super) fn repo_refresh(ctx: &Arc<AppContext>) -> Result<()> {
    crate::commands::block_on(repo_refresh_async(ctx))
}

async fn repo_refresh_async(ctx: &Arc<AppContext>) -> Result<()> {
    let conn = ctx.connect_to_daemon().await?;

    let mut action_handle = RepoRefreshGoal
        .send_goal(
            conn.messenger,
            &conn.core_node_name,
            CALLER_INSTANCE_ID,
            Some(&conn.core_node_name),
            None,
            GOAL_TIMEOUT,
        )
        .await
        .map_err(|e| Error::ExecutionFailed(format!("Failed to send repo refresh goal: {}", e)))?;

    // Decode goal response
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

    let mut last_activity = tokio::time::Instant::now();

    loop {
        // Drain feedback
        loop {
            if last_activity.elapsed() >= IDLE_TIMEOUT {
                return Err(Error::ExecutionFailed(
                    "Timeout: no activity during repo refresh".to_string(),
                ));
            }

            match tokio::time::timeout(FEEDBACK_TIMEOUT, action_handle.on_next_feedback()).await {
                Ok(Ok(msg)) => {
                    last_activity = tokio::time::Instant::now();
                    if let Ok(feedback) = RepoRefreshFeedback::decode(&msg.payload()) {
                        if feedback.variants.is_empty() {
                            info!(
                                "  Found {}:{} ({}, {})",
                                feedback.node_name,
                                feedback.node_tag,
                                feedback.source_type,
                                feedback.path,
                            );
                        } else {
                            info!(
                                "  Found {}:{} ({}, {}) [variants: {}]",
                                feedback.node_name,
                                feedback.node_tag,
                                feedback.source_type,
                                feedback.path,
                                feedback.variants.join(", "),
                            );
                        }
                    }
                }
                Ok(Err(_)) => break, // channel closed
                Err(_) => break,     // timeout — drain complete
            }
        }

        // Check for result
        match ActionMessenger::request_result(conn.messenger, &action_handle, RESULT_POLL_TIMEOUT)
            .await
        {
            Ok(msg) => {
                let payload = msg.payload();
                if peppylib::encoding::is_result_pending(&payload) {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
                let result = RepoRefreshResult::decode(&payload).map_err(|e| {
                    Error::ExecutionFailed(format!("Failed to decode refresh result: {}", e))
                })?;

                if result.success {
                    info!(
                        "Repository refresh complete. {} node(s) found.",
                        result.total_nodes_found
                    );
                    return Ok(());
                } else {
                    return Err(Error::ExecutionFailed(format!(
                        "Repository refresh failed: {}",
                        result
                            .error_message
                            .unwrap_or_else(|| "unknown error".to_string())
                    )));
                }
            }
            Err(peppylib::PeppyError::ActionResultTimeout { .. }) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => {
                return Err(Error::ExecutionFailed(format!(
                    "Failed to get refresh result: {}",
                    e
                )));
            }
        }
    }
}
