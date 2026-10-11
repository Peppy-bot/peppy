use std::sync::Arc;

use core_node_api::encoding::{
    RepoRefreshFeedback, RepoRefreshGoal, RepoRefreshGoalResponse, RepoRefreshResult,
};
use tracing::{info, warn};

use super::identity_label;
use crate::commands::action_poll::poll_action_to_completion;
use crate::commands::node::TimeoutConfig;
use crate::commands::{CALLER_INSTANCE_ID, GOAL_TIMEOUT, SCROLLING_OUTPUT_LINES};
use crate::context::AppContext;
use crate::error::{Error, Result};
use crate::terminal::ScrollingOutput;
use peppylib::core_node::transport::send_goal;

const IDLE_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 3600;

pub(super) fn repo_refresh(ctx: &Arc<AppContext>, strict: bool) -> Result<()> {
    crate::commands::block_on(repo_refresh_async(ctx, strict))
}

async fn repo_refresh_async(ctx: &Arc<AppContext>, strict: bool) -> Result<()> {
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

    let goal_response_payload = action_handle.goal_reply().body.clone();
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
        "Repository refresh complete. {} node(s), {} launcher(s), {} contract(s), {} pairing(s), {} MCP exposure(s) found.",
        result.total_nodes_found,
        result.total_launchers_found,
        result.total_contracts_found,
        result.total_pairings_found,
        result.total_mcp_exposures_found
    );

    if result.failure_report.is_empty() {
        return Ok(());
    }
    // With `--strict`, the report is the error: a CI job or a release
    // check stops here, at the refresh, rather than later in a test that
    // fails for want of what this repository publishes.
    if strict {
        return Err(Error::ExecutionFailed(format!(
            "{}\nThe refresh ran with --strict, so a repository that could not be read fails it.",
            result.failure_report
        )));
    }
    // Otherwise a repository that could not be read is not a failed
    // command: every other repository updated, and this one still serves
    // the entries it last published. Naming it and naming the retry is the
    // whole response, and it is what lets an install finish on a hub that
    // was briefly unreachable rather than stopping there.
    warn!("{}", result.failure_report);
    warn!("Run `peppy repo refresh` again to pick those repositories up.");
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
        } => format!(
            "Found {kind} {} ({source_type}, {path})",
            identity_label(item_name, item_tag)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_node_api::encoding::{RepoItemKind, RepoSourceKind};

    fn discovered(kind: RepoItemKind, item_name: &str, item_tag: &str) -> RepoRefreshFeedback {
        RepoRefreshFeedback::Discovered {
            kind,
            item_name: item_name.to_owned(),
            item_tag: item_tag.to_owned(),
            source_type: RepoSourceKind::Git,
            path: "items/peppy.json5".to_owned(),
            sha256: "a".repeat(64),
        }
    }

    /// A tagged item reads as `name:tag`; a launcher has no tag, so it reads
    /// as its bare name rather than with a dangling `:`.
    #[test]
    fn format_refresh_line_omits_the_tag_of_an_untagged_item() {
        assert_eq!(
            format_refresh_line(&discovered(RepoItemKind::Node, "uvc_camera", "v1")),
            "Found node uvc_camera:v1 (git, items/peppy.json5)"
        );
        assert_eq!(
            format_refresh_line(&discovered(RepoItemKind::Launcher, "camera_boot", "")),
            "Found launcher camera_boot (git, items/peppy.json5)"
        );
    }
}
