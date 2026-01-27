use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use config::peppy_config::PeppyLauncherParser;
use master_node::encoding::{
    LaunchFeedback, LaunchFeedbackStep, LaunchGoal, LaunchGoalResponse, LaunchResult,
};
use peppylib::{ActionMessenger, PeppyError};
use tracing::info;

use crate::context::AppContext;
use crate::error::{Error, Result};

const CALLER_INSTANCE_ID: &str = "peppy-cli";
const GOAL_TIMEOUT: Duration = Duration::from_secs(30);
const RESULT_TIMEOUT: Duration = Duration::from_secs(300);
const FEEDBACK_DRAIN_TIMEOUT: Duration = Duration::from_millis(50);
const RESULT_POLL_TIMEOUT: Duration = Duration::from_millis(200);

fn feedback_step_label(step: &LaunchFeedbackStep) -> &'static str {
    match step {
        LaunchFeedbackStep::LauncherStep => "launcher",
        LaunchFeedbackStep::AddingNode => "add",
        LaunchFeedbackStep::StartingNode => "start",
    }
}

pub fn launch(ctx: &Arc<AppContext>, launcher_config_path: PathBuf) -> Result<()> {
    crate::commands::block_on(launch_async(ctx, launcher_config_path))
}

async fn launch_async(ctx: &Arc<AppContext>, launcher_config_path: PathBuf) -> Result<()> {
    let daemon_state = ctx.read_daemon_state().map_err(|e| {
        Error::ExecutionFailed(format!(
            "Failed to read daemon state. Is the peppy daemon running? Error: {}",
            e
        ))
    })?;
    let master_node_name = daemon_state.master_node_name;

    PeppyLauncherParser::from_path(&launcher_config_path).map_err(Error::PeppyConfig)?;
    let peppy_launcher_json5 = std::fs::read_to_string(&launcher_config_path)?;
    let nodes_directory = launcher_config_path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| ctx.root_dir.clone());

    info!(
        "Calling launcher on master '{}' with nodes_directory={}",
        master_node_name,
        nodes_directory.display()
    );

    ctx.connect().await?;
    let messenger_handle = ctx
        .messenger_handle()
        .ok_or_else(|| Error::ExecutionFailed("Failed to connect to daemon".to_string()))?;

    let goal = LaunchGoal::new(peppy_launcher_json5, nodes_directory);

    let mut action_handle = goal
        .send_goal(
            messenger_handle,
            &master_node_name,
            CALLER_INSTANCE_ID,
            None,
            None,
            GOAL_TIMEOUT,
        )
        .await
        .map_err(|e| Error::ExecutionFailed(format!("Failed to send launch goal: {}", e)))?;

    let goal_response = LaunchGoalResponse::decode(
        &action_handle.goal_response().payload().to_bytes(),
    )
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

    let deadline = tokio::time::Instant::now() + RESULT_TIMEOUT;

    loop {
        // Drain feedback so the subscriber channel doesn't fill up and block publication.
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err(Error::ExecutionFailed(format!(
                    "Launch timed out waiting for result. Log file: {}",
                    goal_response.log_path.display()
                )));
            }
            let remaining = deadline - now;
            let drain_timeout = FEEDBACK_DRAIN_TIMEOUT.min(remaining);

            match tokio::time::timeout(drain_timeout, action_handle.on_next_feedback()).await {
                Ok(Ok(msg)) => {
                    let payload = msg.payload().to_bytes();
                    if let Ok(feedback) = LaunchFeedback::decode(&payload) {
                        let step = feedback_step_label(&feedback.step);
                        if feedback.is_stdout() {
                            println!("[{step}] {}", feedback.line);
                        } else {
                            eprintln!("[{step}] {}", feedback.line);
                        }
                    }
                }
                Ok(Err(_)) => break,
                Err(_) => break,
            }
        }

        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(Error::ExecutionFailed(format!(
                "Launch timed out waiting for result. Log file: {}",
                goal_response.log_path.display()
            )));
        }
        let remaining = deadline - now;
        let poll_timeout = RESULT_POLL_TIMEOUT.min(remaining);

        match ActionMessenger::request_result(messenger_handle, &action_handle, poll_timeout).await
        {
            Ok(msg) => {
                let payload = msg.payload().to_bytes();
                match LaunchResult::decode(&payload) {
                    Ok(result) => {
                        // Drain any remaining feedback so output is stable on completion.
                        loop {
                            let Ok(Some(msg)) = action_handle.try_next_feedback() else {
                                break;
                            };
                            let payload = msg.payload().to_bytes();
                            if let Ok(feedback) = LaunchFeedback::decode(&payload) {
                                let step = feedback_step_label(&feedback.step);
                                if feedback.is_stdout() {
                                    println!("[{step}] {}", feedback.line);
                                } else {
                                    eprintln!("[{step}] {}", feedback.line);
                                }
                            }
                        }

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
