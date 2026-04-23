use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use config::launcher::PeppyLauncherParser;
use core_node_api::encoding::{
    LaunchFeedback, LaunchFeedbackStep, LaunchGoal, LaunchGoalResponse, LaunchResult,
    NodeAddLogEntry, NodeBuildLogEntry, NodeRunLogEntry,
};
use peppylib::{ActionMessenger, PeppyError};
use tracing::info;

use crate::commands::node::caller_env_overrides;
use crate::commands::{CALLER_INSTANCE_ID, GOAL_TIMEOUT, SCROLLING_OUTPUT_LINES};
use crate::context::AppContext;
use crate::error::{Error, Result};
use crate::terminal::ScrollingOutput;

use core_node::transport::LaunchGoalSendGoalExt;
// Minimum CLI fallback ceiling when the user opts into `--max-timeout-secs`. Ensures the CLI's
// safety net never fires before the daemon's own per-phase timeout, so users see a precise
// daemon-side error rather than a generic CLI fallback. When the user omits the flag, no CLI
// ceiling is installed — the contract is idle-only (daemon-side `max_timeout_secs = None`).
const CLI_MAX_TIMEOUT_FLOOR: Duration = Duration::from_secs(7200);
// Headroom granted to the daemon to surface its own timeout error before the CLI's fallback
// ceiling fires. Keeps the error the user sees specific ("build idle timeout exceeded...") rather
// than a generic CLI-side "daemon hung" message.
const DAEMON_RESPONSE_GRACE: Duration = Duration::from_secs(60);
const FEEDBACK_DRAIN_TIMEOUT: Duration = Duration::from_millis(50);
const RESULT_POLL_TIMEOUT: Duration = Duration::from_millis(200);

// CLI wall-clock fallback ceiling. `None` means idle-only (daemon-side contract honored).
// When the user opts into `--max-timeout-secs`, add DAEMON_RESPONSE_GRACE and enforce
// CLI_MAX_TIMEOUT_FLOOR so the daemon's per-phase error fires first.
fn compute_cli_max_timeout(max_timeout_secs: Option<u64>) -> Option<Duration> {
    max_timeout_secs.map(|n| {
        Duration::from_secs(n)
            .saturating_add(DAEMON_RESPONSE_GRACE)
            .max(CLI_MAX_TIMEOUT_FLOOR)
    })
}

fn display_node_log_files(
    add_logs: &[NodeAddLogEntry],
    build_logs: &[NodeBuildLogEntry],
    run_logs: &[NodeRunLogEntry],
) {
    if add_logs.is_empty() && build_logs.is_empty() && run_logs.is_empty() {
        return;
    }
    info!("Node log files:");
    if !add_logs.is_empty() {
        info!("  Add:");
        for e in add_logs {
            info!("    `{}`: {}", e.node_label, e.log_path.display());
        }
    }
    if !build_logs.is_empty() {
        info!("  Build:");
        for e in build_logs {
            let marker = if e.failed { " [FAILED]" } else { "" };
            if e.failed {
                tracing::error!("    `{}`{}: {}", e.node_label, marker, e.log_path.display());
            } else {
                info!("    `{}`: {}", e.node_label, e.log_path.display());
            }
        }
    }
    if !run_logs.is_empty() {
        info!("  Run:");
        for e in run_logs {
            let marker = if e.failed { " [FAILED]" } else { "" };
            if e.failed {
                tracing::error!(
                    "    {} (`{}`){}: {}",
                    e.instance_id,
                    e.node_label,
                    marker,
                    e.log_path.display()
                );
            } else {
                info!(
                    "    {} (`{}`): {}",
                    e.instance_id,
                    e.node_label,
                    e.log_path.display()
                );
            }
        }
    }
}

fn handle_feedback(
    feedback: &LaunchFeedback,
    scrolling_output: &mut Option<ScrollingOutput>,
    current_scrolling_step: &mut Option<LaunchFeedbackStep>,
) {
    // Check if we're switching between steps
    let step_changed = current_scrolling_step
        .as_ref()
        .map(|s| std::mem::discriminant(s) != std::mem::discriminant(&feedback.step))
        .unwrap_or(true);

    if step_changed {
        // Clear existing scrolling output if we were in a scrolling step
        if let Some(output) = scrolling_output.as_mut() {
            output.clear();
            *scrolling_output = None;
        }
        *current_scrolling_step = Some(feedback.step);
    }

    match &feedback.step {
        LaunchFeedbackStep::LauncherStep => {
            if feedback.is_stdout() {
                info!("{}", feedback.line);
            } else {
                tracing::warn!("{}", feedback.line);
            }
        }
        LaunchFeedbackStep::AddingNode
        | LaunchFeedbackStep::BuildingNode
        | LaunchFeedbackStep::RunningNode => {
            let output = scrolling_output
                .get_or_insert_with(|| ScrollingOutput::new(SCROLLING_OUTPUT_LINES));
            output.add_line(&feedback.line, feedback.is_stderr());
        }
    }
}

pub fn launch(
    ctx: &Arc<AppContext>,
    launcher_config_path: PathBuf,
    node_add_idle_timeout_secs: u64,
    node_build_idle_timeout_secs: u64,
    node_run_idle_timeout_secs: u64,
    max_timeout_secs: Option<u64>,
) -> Result<()> {
    crate::commands::block_on(launch_async(
        ctx,
        launcher_config_path,
        node_add_idle_timeout_secs,
        node_build_idle_timeout_secs,
        node_run_idle_timeout_secs,
        max_timeout_secs,
    ))
}

async fn launch_async(
    ctx: &Arc<AppContext>,
    launcher_config_path: PathBuf,
    node_add_idle_timeout_secs: u64,
    node_build_idle_timeout_secs: u64,
    node_run_idle_timeout_secs: u64,
    max_timeout_secs: Option<u64>,
) -> Result<()> {
    // Canonicalize the path so the core node can find the file regardless of its working directory
    let launcher_config_path = launcher_config_path.canonicalize().map_err(|e| {
        Error::ExecutionFailed(format!(
            "Failed to resolve launcher config path '{}': {}",
            launcher_config_path.display(),
            e
        ))
    })?;

    PeppyLauncherParser::from_path(&launcher_config_path).map_err(Error::PeppyConfig)?;

    let conn = ctx.connect_to_daemon().await?;

    info!(
        "Calling launcher on daemon '{}' with config={}",
        conn.core_node_name,
        launcher_config_path.display()
    );

    let goal = LaunchGoal::new(
        &launcher_config_path,
        node_add_idle_timeout_secs,
        node_build_idle_timeout_secs,
        node_run_idle_timeout_secs,
        max_timeout_secs,
    )
    .with_env_vars(caller_env_overrides());

    // CLI fallback ceiling: when the user opts into a max we grant the daemon a response-grace
    // window to surface its own error first, but never less than the absolute floor in case the
    // daemon hangs entirely. `None` honors the daemon's idle-only contract — no CLI ceiling.
    let cli_max_timeout: Option<Duration> = compute_cli_max_timeout(max_timeout_secs);

    // CLI-side liveness watchdog: trips if no feedback arrives from any phase. Must cover the
    // longest per-phase idle budget (only one phase runs at a time) plus a grace window so the
    // daemon's phase-specific timeout always fires first and surfaces a precise error.
    let cli_idle_timeout = Duration::from_secs(
        node_add_idle_timeout_secs
            .max(node_build_idle_timeout_secs)
            .max(node_run_idle_timeout_secs),
    )
    .saturating_add(DAEMON_RESPONSE_GRACE);

    let mut action_handle = goal
        .send_goal(
            conn.messenger,
            &conn.core_node_name,
            CALLER_INSTANCE_ID,
            None,
            None,
            GOAL_TIMEOUT,
        )
        .await
        .map_err(|e| Error::ExecutionFailed(format!("Failed to send launch goal: {}", e)))?;

    let goal_response = LaunchGoalResponse::decode(&action_handle.goal_response().payload())
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

    let absolute_deadline: Option<tokio::time::Instant> =
        cli_max_timeout.and_then(|d| tokio::time::Instant::now().checked_add(d));
    let mut last_activity = tokio::time::Instant::now();
    let mut scrolling_output: Option<ScrollingOutput> = None;
    let mut current_scrolling_step: Option<LaunchFeedbackStep> = None;

    loop {
        // Drain feedback so the subscriber channel doesn't fill up and block publication.
        loop {
            let now = tokio::time::Instant::now();
            if let Some(deadline) = absolute_deadline
                && now >= deadline
            {
                if let Some(output) = scrolling_output.as_mut() {
                    output.clear();
                }
                return Err(Error::ExecutionFailed(format!(
                    "Launch timed out: max timeout exceeded. Log file: {}",
                    goal_response.log_path.display()
                )));
            }
            if now.duration_since(last_activity) >= cli_idle_timeout {
                if let Some(output) = scrolling_output.as_mut() {
                    output.clear();
                }
                return Err(Error::ExecutionFailed(format!(
                    "Launch timed out: no output received for {}s. Log file: {}",
                    cli_idle_timeout.as_secs(),
                    goal_response.log_path.display()
                )));
            }

            match tokio::time::timeout(FEEDBACK_DRAIN_TIMEOUT, action_handle.on_next_feedback())
                .await
            {
                Ok(Ok(msg)) => {
                    last_activity = tokio::time::Instant::now();
                    let payload = msg.payload();
                    if let Ok(feedback) = LaunchFeedback::decode(&payload) {
                        handle_feedback(
                            &feedback,
                            &mut scrolling_output,
                            &mut current_scrolling_step,
                        );
                    }
                }
                Ok(Err(_)) => break,
                Err(_) => break,
            }
        }

        let now = tokio::time::Instant::now();
        if let Some(deadline) = absolute_deadline
            && now >= deadline
        {
            return Err(Error::ExecutionFailed(format!(
                "Launch timed out: max timeout exceeded. Log file: {}",
                goal_response.log_path.display()
            )));
        }
        if now.duration_since(last_activity) >= cli_idle_timeout {
            return Err(Error::ExecutionFailed(format!(
                "Launch timed out: no output received for {}s. Log file: {}",
                cli_idle_timeout.as_secs(),
                goal_response.log_path.display()
            )));
        }

        match ActionMessenger::request_result(conn.messenger, &action_handle, RESULT_POLL_TIMEOUT)
            .await
        {
            Ok(msg) => {
                let payload = msg.payload();
                match LaunchResult::decode(&payload) {
                    Ok(result) => {
                        // Drain any remaining feedback so output is stable on completion.
                        loop {
                            let Ok(Some(msg)) = action_handle.try_next_feedback() else {
                                break;
                            };
                            let payload = msg.payload();
                            if let Ok(feedback) = LaunchFeedback::decode(&payload) {
                                handle_feedback(
                                    &feedback,
                                    &mut scrolling_output,
                                    &mut current_scrolling_step,
                                );
                            }
                        }

                        // Clear the scrolling output now that we're done
                        if let Some(output) = scrolling_output.as_mut() {
                            output.clear();
                        }

                        display_node_log_files(
                            &result.node_add_logs,
                            &result.node_build_logs,
                            &result.node_run_logs,
                        );

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
                        return Ok(());
                    }
                    Err(err) => {
                        if !peppylib::encoding::is_result_pending(payload.as_ref()) {
                            return Err(Error::ExecutionFailed(format!(
                                "Failed to decode launch result: {}",
                                err
                            )));
                        }
                    }
                }
            }
            Err(PeppyError::ActionResultTimeout { .. }) => {}
            Err(err) => {
                return Err(Error::ExecutionFailed(format!(
                    "Failed to get launch result: {}",
                    err
                )));
            }
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_preserves_idle_only_contract() {
        assert_eq!(compute_cli_max_timeout(None), None);
    }

    #[test]
    fn small_value_hits_the_floor() {
        let got = compute_cli_max_timeout(Some(60)).expect("some");
        assert_eq!(got, CLI_MAX_TIMEOUT_FLOOR);
    }

    #[test]
    fn large_value_dominates_the_floor() {
        let n = CLI_MAX_TIMEOUT_FLOOR.as_secs() * 2;
        let got = compute_cli_max_timeout(Some(n)).expect("some");
        assert_eq!(got, Duration::from_secs(n) + DAEMON_RESPONSE_GRACE);
    }

    #[test]
    fn saturating_add_does_not_panic_at_u64_max() {
        let got = compute_cli_max_timeout(Some(u64::MAX)).expect("some");
        assert_eq!(got, Duration::MAX);
    }
}
