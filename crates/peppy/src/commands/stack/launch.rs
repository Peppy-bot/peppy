use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use config::peppy_config::PeppyLauncherParser;
use core_node::encoding::{
    LaunchFeedback, LaunchFeedbackStep, LaunchGoal, LaunchGoalResponse, LaunchResult,
    NodeAddLogEntry, NodeStartLogEntry,
};
use peppylib::{ActionMessenger, PeppyError};
use tracing::info;

use crate::commands::node::{DEFAULT_IDLE_TIMEOUT_SECS, caller_env_overrides};
use crate::context::AppContext;
use crate::error::{Error, Result};
use crate::terminal::ScrollingOutput;

const CALLER_INSTANCE_ID: &str = "peppy-cli";
const GOAL_TIMEOUT: Duration = Duration::from_secs(30);
// Idle timeout for the overall launch result (resets on feedback from daemon)
const IDLE_TIMEOUT: Duration = Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS);
// Absolute max timeout for the entire launch operation (2x per-operation max to allow for multi-node sequential processing)
const MAX_TIMEOUT: Duration = Duration::from_secs(7200);
const FEEDBACK_DRAIN_TIMEOUT: Duration = Duration::from_millis(50);
const RESULT_POLL_TIMEOUT: Duration = Duration::from_millis(200);
const SCROLLING_OUTPUT_LINES: usize = 10;

fn display_node_log_files(add_logs: &[NodeAddLogEntry], start_logs: &[NodeStartLogEntry]) {
    if add_logs.is_empty() && start_logs.is_empty() {
        return;
    }
    println!("Node log files:");
    if !add_logs.is_empty() {
        println!("  Add:");
        for e in add_logs {
            println!("    `{}`: {}", e.node_label, e.log_path.display());
        }
    }
    if !start_logs.is_empty() {
        println!("  Start:");
        for e in start_logs {
            let marker = if e.failed { " [FAILED]" } else { "" };
            let line = format!(
                "    {} (`{}`){}: {}",
                e.instance_id,
                e.node_label,
                marker,
                e.log_path.display()
            );
            if e.failed {
                eprintln!("{line}");
            } else {
                println!("{line}");
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
        *current_scrolling_step = Some(feedback.step.clone());
    }

    match &feedback.step {
        LaunchFeedbackStep::LauncherStep => {
            if feedback.is_stdout() {
                println!("{}", feedback.line);
            } else {
                eprintln!("{}", feedback.line);
            }
        }
        LaunchFeedbackStep::AddingNode | LaunchFeedbackStep::StartingNode => {
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
    node_start_idle_timeout_secs: u64,
    max_timeout_secs: u64,
) -> Result<()> {
    crate::commands::block_on(launch_async(
        ctx,
        launcher_config_path,
        node_add_idle_timeout_secs,
        node_start_idle_timeout_secs,
        max_timeout_secs,
    ))
}

async fn launch_async(
    ctx: &Arc<AppContext>,
    launcher_config_path: PathBuf,
    node_add_idle_timeout_secs: u64,
    node_start_idle_timeout_secs: u64,
    max_timeout_secs: u64,
) -> Result<()> {
    let daemon_state = ctx.read_daemon_state()?;
    let core_node_name = daemon_state.core_node_name;

    // Canonicalize the path so the core node can find the file regardless of its working directory
    let launcher_config_path = launcher_config_path.canonicalize().map_err(|e| {
        Error::ExecutionFailed(format!(
            "Failed to resolve launcher config path '{}': {}",
            launcher_config_path.display(),
            e
        ))
    })?;

    PeppyLauncherParser::from_path(&launcher_config_path).map_err(Error::PeppyConfig)?;

    info!(
        "Calling launcher on daemon '{}' with config={}",
        core_node_name,
        launcher_config_path.display()
    );

    ctx.connect().await?;
    let messenger_handle = ctx
        .messenger_handle()
        .ok_or_else(|| Error::ExecutionFailed("Failed to connect to daemon".to_string()))?;

    let goal = LaunchGoal::new(
        &launcher_config_path,
        node_add_idle_timeout_secs,
        node_start_idle_timeout_secs,
        max_timeout_secs,
    )
    .with_env_vars(caller_env_overrides());

    let mut action_handle = goal
        .send_goal(
            messenger_handle,
            &core_node_name,
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

    let absolute_deadline = tokio::time::Instant::now() + MAX_TIMEOUT;
    let mut last_activity = tokio::time::Instant::now();
    let mut scrolling_output: Option<ScrollingOutput> = None;
    let mut current_scrolling_step: Option<LaunchFeedbackStep> = None;

    loop {
        // Drain feedback so the subscriber channel doesn't fill up and block publication.
        loop {
            let now = tokio::time::Instant::now();
            if now >= absolute_deadline {
                if let Some(output) = scrolling_output.as_mut() {
                    output.clear();
                }
                return Err(Error::ExecutionFailed(format!(
                    "Launch timed out: max timeout exceeded. Log file: {}",
                    goal_response.log_path.display()
                )));
            }
            if now.duration_since(last_activity) >= IDLE_TIMEOUT {
                if let Some(output) = scrolling_output.as_mut() {
                    output.clear();
                }
                return Err(Error::ExecutionFailed(format!(
                    "Launch timed out: no output received for {}s. Log file: {}",
                    IDLE_TIMEOUT.as_secs(),
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
        if now >= absolute_deadline {
            return Err(Error::ExecutionFailed(format!(
                "Launch timed out: max timeout exceeded. Log file: {}",
                goal_response.log_path.display()
            )));
        }
        if now.duration_since(last_activity) >= IDLE_TIMEOUT {
            return Err(Error::ExecutionFailed(format!(
                "Launch timed out: no output received for {}s. Log file: {}",
                IDLE_TIMEOUT.as_secs(),
                goal_response.log_path.display()
            )));
        }

        match ActionMessenger::request_result(messenger_handle, &action_handle, RESULT_POLL_TIMEOUT)
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

                        display_node_log_files(&result.node_add_logs, &result.node_start_logs);

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
                        let pending = std::str::from_utf8(payload.as_ref())
                            .map(|text| text.starts_with("result pending"))
                            .unwrap_or(false);
                        if !pending {
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
