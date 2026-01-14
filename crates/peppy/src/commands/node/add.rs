use config::node::NodeConfigParser;
use master_node::encoding::{NodeAddFeedback, NodeAddGoal, NodeAddResult};
use peppylib::{ActionMessenger, PeppyError};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

use super::start::start_instance_async;
use crate::context::{AppContext, DaemonState};
use crate::error::{Error, Result};
use crate::terminal::ScrollingOutput;

const CALLER_INSTANCE_ID: &str = "peppy-cli";
// Timeout for the goal to be accepted (should be fast)
const GOAL_TIMEOUT: Duration = Duration::from_secs(30);
// Timeout for the result (the actual add operation can take a long time)
const RESULT_TIMEOUT: Duration = Duration::from_secs(900); // 15min

pub fn add_node(
    ctx: &Arc<AppContext>,
    node_dir: PathBuf,
    run: bool,
    args: Vec<(String, String)>,
    instance_id: Option<String>,
) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    let peppy_json5 = node_dir.join("peppy.json5");
    rt.block_on(add_node_async(ctx, peppy_json5, run, args, instance_id))
}

async fn add_node_async(
    ctx: &Arc<AppContext>,
    peppy_json5: PathBuf,
    run: bool,
    args: Vec<(String, String)>,
    instance_id: Option<String>,
) -> Result<()> {
    let daemon_state = DaemonState::read().map_err(|e| {
        Error::ExecutionFailed(format!(
            "Failed to read daemon state. Is the peppy daemon running? Error: {}",
            e
        ))
    })?;
    let master_node_name = daemon_state.master_node_name;

    // Canonicalize the path to ensure we have an absolute path.
    // This is important for relative paths like "peppy.json5" where parent() would be empty.
    let peppy_json5 = peppy_json5.canonicalize().map_err(|e| {
        Error::ExecutionFailed(format!(
            "Failed to resolve path '{}': {}",
            peppy_json5.display(),
            e
        ))
    })?;

    // Parse node config to discover node name/tag and validate it.
    let node_config = NodeConfigParser::from_path(&peppy_json5).map_err(Error::PeppyConfig)?;
    let node_name = node_config.manifest.name.as_str().to_string();
    let node_tag = node_config.manifest.tag.clone();

    let from_dir = peppy_json5
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| ctx.root_dir.clone());

    info!(
        "Running `add_cmd` for {}:{} on master '{}'...",
        node_name, node_tag, master_node_name
    );

    ctx.connect().await?;
    let messenger_handle = ctx
        .messenger_handle()
        .ok_or_else(|| Error::ExecutionFailed("Failed to connect to daemon".to_string()))?;

    // Send the goal to start the add action
    let add_goal = NodeAddGoal::new(from_dir);
    let mut action_handle = add_goal
        .send_goal(
            messenger_handle,
            &master_node_name,
            CALLER_INSTANCE_ID,
            Some(&master_node_name),
            None,
            GOAL_TIMEOUT,
        )
        .await
        .map_err(|e| Error::ExecutionFailed(format!("Failed to send node_add goal: {}", e)))?;

    // Number of lines to display in the scrolling output region
    const SCROLLING_OUTPUT_LINES: usize = 10;
    let mut scrolling_output = ScrollingOutput::new(SCROLLING_OUTPUT_LINES);

    let deadline = tokio::time::Instant::now() + RESULT_TIMEOUT;
    let add_result = loop {
        // Drain feedback so the publisher doesn't block on a full channel.
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                scrolling_output.clear();
                return Err(Error::ExecutionFailed(
                    "Timeout waiting for node_add result".to_string(),
                ));
            }
            let remaining = deadline - now;
            let drain_timeout = Duration::from_millis(50).min(remaining);
            match tokio::time::timeout(drain_timeout, action_handle.on_next_feedback()).await {
                Ok(Ok(msg)) => {
                    let payload = msg.payload();
                    if let Ok(feedback) = NodeAddFeedback::decode(&payload.to_bytes()) {
                        if feedback.is_log_path() {
                            info!("Log file: {}", feedback.line);
                        } else {
                            scrolling_output.add_line(&feedback.line, feedback.is_stderr());
                        }
                    }
                }
                Ok(Err(_)) => break,
                Err(_) => break,
            }
        }

        let now = tokio::time::Instant::now();
        if now >= deadline {
            scrolling_output.clear();
            return Err(Error::ExecutionFailed(
                "Timeout waiting for node_add result".to_string(),
            ));
        }
        let remaining = deadline - now;
        let poll_timeout = Duration::from_millis(200).min(remaining);
        match ActionMessenger::request_result(messenger_handle, &action_handle, poll_timeout).await
        {
            Ok(msg) => {
                let payload = msg.payload().to_bytes();
                match NodeAddResult::decode(&payload) {
                    Ok(result) => break result,
                    Err(err) => {
                        let pending = std::str::from_utf8(payload.as_ref())
                            .map(|text| text.starts_with("result pending"))
                            .unwrap_or(false);
                        if !pending {
                            scrolling_output.clear();
                            return Err(Error::ExecutionFailed(format!(
                                "Failed to decode node_add result: {}",
                                err
                            )));
                        }
                    }
                }
            }
            Err(PeppyError::ActionResultTimeout { .. }) => {}
            Err(err) => {
                scrolling_output.clear();
                return Err(Error::ExecutionFailed(format!(
                    "Failed to get node_add result: {}",
                    err
                )));
            }
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    // Clear the scrolling output now that we're done processing feedback
    scrolling_output.clear();

    if !add_result.success {
        return Err(Error::ExecutionFailed(
            add_result
                .error_message
                .unwrap_or_else(|| "node_add failed with no error message".to_string()),
        ));
    }

    info!("Added node {}:{} to the node stack", node_name, node_tag);
    info!(
        "Snapshot path: {}",
        add_result.snapshot_path.to_string_lossy()
    );

    if !run {
        return Ok(());
    }

    start_instance_async(
        messenger_handle,
        &master_node_name,
        &node_name,
        &node_tag,
        &args,
        instance_id,
    )
    .await?;

    Ok(())
}
