//! Driving one stack goal to its result: sending it, relaying its
//! feedback, holding it to the operator's budgets, and reporting the log
//! files it wrote.

use std::path::Path;
use std::time::Duration;

use core_node::{idle_timeout_flag, slow_connection_hint};
use core_node_api::encoding::{
    LaunchFeedback, LaunchFeedbackStep, LaunchGoalResponse, LaunchResult, NodeAddLogEntry,
    NodeBuildLogEntry, NodeRunLogEntry, StackBudgets,
};
use peppylib::ActionMessenger;
use peppylib::core_node::transport::send_goal;
use peppylib::messaging::ResultStatus;
use tracing::info;

use super::super::action_poll::FEEDBACK_DRAIN_TIMEOUT;
use crate::commands::{CALLER_INSTANCE_ID, GOAL_TIMEOUT, SCROLLING_OUTPUT_LINES};
use crate::error::{Error, Result};
use crate::terminal::ScrollingOutput;

// Minimum CLI fallback ceiling when the user opts into `--max-timeout-secs`. Ensures the CLI's
// safety net never fires before the daemon's own per-phase timeout, so users see the daemon's
// precise error first. When the user omits the flag, no CLI ceiling is installed; the
// contract is idle-only (daemon-side `max_timeout_secs = None`).
const CLI_MAX_TIMEOUT_FLOOR: Duration = Duration::from_secs(7200);
// Headroom granted to the daemon to surface its own timeout error before the CLI's fallback
// ceiling fires. Keeps the error the user sees specific ("build idle timeout exceeded...") rather
// than a generic CLI-side "daemon hung" message.
const DAEMON_RESPONSE_GRACE: Duration = Duration::from_secs(60);

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

/// One line of the "Node log files" listing:
/// `node_name:tag@core-node: /path/to/log`, with a ` [FAILED]` marker before
/// the colon when the phase failed. The core node names the machine whose
/// filesystem holds the log file.
fn node_log_line(node_label: &str, core_node: &str, failed: bool, log_path: &Path) -> String {
    let marker = if failed { " [FAILED]" } else { "" };
    format!("{node_label}@{core_node}{marker}: {}", log_path.display())
}

fn display_node_log_files(
    add_logs: &[NodeAddLogEntry],
    build_logs: &[NodeBuildLogEntry],
    run_logs: &[NodeRunLogEntry],
) {
    if add_logs.is_empty() && build_logs.is_empty() && run_logs.is_empty() {
        return;
    }
    let line = |node_label: &str, core_node: &str, failed: bool, log_path: &Path| {
        (
            node_log_line(node_label, core_node, failed, log_path),
            failed,
        )
    };
    let sections = [
        (
            "Add",
            add_logs
                .iter()
                .map(|e| line(&e.node_label, &e.core_node, e.failed, &e.log_path))
                .collect::<Vec<_>>(),
        ),
        (
            "Build",
            build_logs
                .iter()
                .map(|e| line(&e.node_label, &e.core_node, e.failed, &e.log_path))
                .collect(),
        ),
        (
            "Run",
            run_logs
                .iter()
                .map(|e| line(&e.node_label, &e.core_node, e.failed, &e.log_path))
                .collect(),
        ),
    ];
    info!("Node log files:");
    for (title, lines) in sections {
        if lines.is_empty() {
            continue;
        }
        info!("  {title}:");
        for (text, failed) in lines {
            if failed {
                tracing::error!("    {text}");
            } else {
                info!("    {text}");
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
            output.add_line(&feedback.line);
        }
    }
}

/// A goal [`drive_stack_goal`] drives, and the word every line it writes
/// calls that goal by. Visible only inside `commands::stack`, so the three
/// stack goals are the whole set.
pub(super) trait StackGoal: core_node_api::ActionGoal {
    const OPERATION: &'static str;
}

impl StackGoal for core_node_api::encoding::LaunchGoal {
    const OPERATION: &'static str = "Launch";
}

impl StackGoal for core_node_api::encoding::StackJoinGoal {
    const OPERATION: &'static str = "Join";
}

impl StackGoal for core_node_api::encoding::StackRemoveGoal {
    const OPERATION: &'static str = "Remove";
}

/// Sends `goal` to the daemon and follows it to its result under `budgets`.
pub(super) async fn drive_stack_goal<G: StackGoal>(
    conn: &crate::context::DaemonConnection<'_>,
    goal: &G,
    budgets: &StackBudgets,
) -> Result<()> {
    let operation = G::OPERATION;
    let cli_max_timeout: Option<Duration> = compute_cli_max_timeout(budgets.max_timeout_secs);

    // CLI-side liveness watchdog: trips if no feedback arrives from any phase. Must cover the
    // longest per-phase idle budget (only one phase runs at a time) plus a grace window so the
    // daemon's phase-specific timeout always fires first and surfaces a precise error.
    let cli_idle_timeout = Duration::from_secs(
        budgets
            .node_add_idle_timeout_secs
            .max(budgets.node_build_idle_timeout_secs)
            .max(budgets.node_run_idle_timeout_secs),
    )
    .saturating_add(DAEMON_RESPONSE_GRACE);

    let mut action_handle = send_goal(
        goal,
        conn.messenger,
        &conn.core_node_name,
        CALLER_INSTANCE_ID,
        Some(&conn.target_core_node),
        GOAL_TIMEOUT,
    )
    .await
    .map_err(|e| Error::ExecutionFailed(format!("Failed to send {operation} goal: {e}")))?;

    let goal_response = LaunchGoalResponse::decode(&action_handle.goal_reply().body)
        .map_err(|e| Error::ExecutionFailed(format!("Failed to decode goal response: {}", e)))?;

    if !goal_response.accepted {
        let reason = goal_response
            .rejection_reason
            .unwrap_or_else(|| "unknown reason".to_string());
        return Err(Error::ExecutionFailed(format!(
            "{operation} goal rejected: {}",
            reason
        )));
    }

    info!(
        "{operation} goal accepted, log file: {}",
        goal_response.log_path.display()
    );

    let absolute_deadline: Option<tokio::time::Instant> =
        cli_max_timeout.and_then(|d| tokio::time::Instant::now().checked_add(d));
    let mut last_activity = tokio::time::Instant::now();
    let mut scrolling_output: Option<ScrollingOutput> = None;
    let mut current_scrolling_step: Option<LaunchFeedbackStep> = None;

    // Drain feedback until the server closes the stream on completion,
    // honoring the idle / max-timeout budgets.
    loop {
        let now = tokio::time::Instant::now();
        if let Some(deadline) = absolute_deadline
            && now >= deadline
        {
            if let Some(output) = scrolling_output.as_mut() {
                output.clear();
            }
            return Err(Error::ExecutionFailed(format!(
                "{operation} timed out: max timeout exceeded. Log file: {}",
                goal_response.log_path.display()
            )));
        }
        if now.duration_since(last_activity) >= cli_idle_timeout {
            if let Some(output) = scrolling_output.as_mut() {
                output.clear();
            }
            // Name the phase that went quiet and the flag that raises its
            // budget, so a slow-connection user is pointed at the fix instead
            // of a bare timeout.
            let (phase, hint) = match current_scrolling_step {
                Some(step) => (
                    format!(" during the {} phase", step.phase_label()),
                    idle_timeout_flag(step)
                        .map(|flag| format!("; {}", slow_connection_hint(flag)))
                        .unwrap_or_default(),
                ),
                None => (String::new(), String::new()),
            };
            return Err(Error::ExecutionFailed(format!(
                "{operation} timed out: no output received for {}s{phase}{hint}. Log file: {}",
                cli_idle_timeout.as_secs(),
                goal_response.log_path.display()
            )));
        }

        match tokio::time::timeout(FEEDBACK_DRAIN_TIMEOUT, action_handle.on_next_feedback()).await {
            Ok(Ok(msg)) => {
                last_activity = tokio::time::Instant::now();
                let payload = msg.payload_bytes();
                if let Ok(feedback) = LaunchFeedback::decode(payload.as_ref()) {
                    handle_feedback(
                        &feedback,
                        &mut scrolling_output,
                        &mut current_scrolling_step,
                    );
                }
            }
            Ok(Err(_)) => break, // end-of-stream: the goal has completed
            Err(_) => {}         // drain slice elapsed; re-check timeouts and keep draining
        }
    }

    // The goal has completed; fetch its (server-buffered) result once. Give it
    // the remaining max budget so it resolves promptly.
    let now = tokio::time::Instant::now();
    let result_timeout = match absolute_deadline {
        Some(deadline) => deadline
            .saturating_duration_since(now)
            .max(Duration::from_secs(1)),
        None => Duration::from_secs(30),
    };
    match ActionMessenger::request_result(conn.messenger, &action_handle, result_timeout).await {
        Ok(reply) => {
            let body = match reply.status {
                ResultStatus::Completed | ResultStatus::Cancelled => reply.body,
                ResultStatus::Abandoned => {
                    if let Some(output) = scrolling_output.as_mut() {
                        output.clear();
                    }
                    return Err(Error::ExecutionFailed(format!(
                        "{operation} was abandoned by its worker before producing a result"
                    )));
                }
                ResultStatus::Expired => {
                    if let Some(output) = scrolling_output.as_mut() {
                        output.clear();
                    }
                    return Err(Error::ExecutionFailed(format!(
                        "{operation} result expired before it could be fetched"
                    )));
                }
            };
            let result = LaunchResult::decode(body.as_ref()).map_err(|err| {
                Error::ExecutionFailed(format!("Failed to decode {operation} result: {err}"))
            })?;

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
                    "{operation} failed: {}. Log file: {}",
                    error_msg,
                    result.log_path.display()
                )));
            }

            info!("{operation} completed successfully");
            Ok(())
        }
        Err(err) => {
            if let Some(output) = scrolling_output.as_mut() {
                output.clear();
            }
            Err(Error::ExecutionFailed(format!(
                "Failed to get {operation} result: {}",
                err
            )))
        }
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

    #[test]
    fn node_log_line_names_the_core_node_holding_the_file() {
        let line = node_log_line(
            "deliberative_planner:v1",
            "cn-vibrant-chaplygin",
            false,
            Path::new("/tmp/.peppy/logs/add/deliberative_planner_v1.log"),
        );
        assert_eq!(
            line,
            "deliberative_planner:v1@cn-vibrant-chaplygin: \
             /tmp/.peppy/logs/add/deliberative_planner_v1.log"
        );
    }

    #[test]
    fn node_log_line_marks_a_failed_phase_before_the_colon() {
        let line = node_log_line(
            "reactive_policy:v1",
            "cn-robot-7",
            true,
            Path::new("/tmp/.peppy/logs/build/reactive_policy_v1.log"),
        );
        assert_eq!(
            line,
            "reactive_policy:v1@cn-robot-7 [FAILED]: /tmp/.peppy/logs/build/reactive_policy_v1.log"
        );
    }
}
