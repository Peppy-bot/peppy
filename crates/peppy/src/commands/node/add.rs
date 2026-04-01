use core_node::encoding::{
    NodeAddFeedback, NodeAddGoal, NodeAddGoalResponse, NodeAddResult, NodeInfoRequest,
    NodeInfoResponse, NodeSource,
};
use peppylib::{ActionMessenger, MessengerHandle, PeppyError};
use std::io::{self, BufRead, Write};
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

use super::TimeoutConfig;
use super::env::caller_env_overrides;
use super::source::{parse_node_source, parse_variant_source};
use super::start::start_instance_async;
use crate::context::AppContext;
use crate::error::{Error, Result};
use crate::terminal::ScrollingOutput;

/// Options for starting a node instance immediately after adding it.
pub struct StartAfterAddOptions {
    pub args: Vec<(String, String)>,
    pub instance_id: Option<String>,
}

/// Parameters for adding a node.
pub struct AddNodeParams {
    pub source: String,
    pub git_ref: Option<String>,
    pub variant: Option<String>,
    pub start_options: Option<StartAfterAddOptions>,
    pub timeouts: TimeoutConfig,
    pub force: bool,
    pub confirm_reader: Option<Box<dyn BufRead>>,
}

const CALLER_INSTANCE_ID: &str = "peppy-cli";
// Timeout for the goal to be accepted (should be fast)
const GOAL_TIMEOUT: Duration = Duration::from_secs(30);
// Timeout for the node info request
const INFO_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

fn validate_git_ref(git_ref: Option<&str>) -> Result<Option<String>> {
    let git_ref = git_ref.map(str::trim);
    if let Some(git_ref) = git_ref
        && git_ref.is_empty()
    {
        return Err(Error::ExecutionFailed(
            "`--ref` cannot be empty".to_string(),
        ));
    }
    Ok(git_ref.map(str::to_owned))
}

pub fn add_node(ctx: &Arc<AppContext>, params: AddNodeParams) -> Result<()> {
    crate::commands::block_on(add_node_async(ctx, params))
}

async fn add_node_async(ctx: &Arc<AppContext>, params: AddNodeParams) -> Result<()> {
    let AddNodeParams {
        source,
        git_ref,
        variant,
        start_options,
        timeouts,
        force,
        mut confirm_reader,
    } = params;
    let daemon_state = ctx.read_daemon_state()?;
    let core_node_name = daemon_state.core_node_name.clone();
    let git_hash = daemon_state.git_hash.clone();

    // Validate git_ref and parse the source into a NodeSource
    let git_ref = validate_git_ref(git_ref.as_deref())?;
    let node_source = parse_node_source(&source, git_ref)?;

    // Log the resolved source path (may differ from the original when the CLI
    // walked up from a variant subdirectory to the root node directory).
    let display_source = match &node_source {
        NodeSource::Fs(p) => p.display().to_string(),
        _ => source.clone(),
    };
    info!("Adding node from {}...", display_source);

    // Parse variant source early so the preflight check uses the same merged config
    // that the actual add will use.
    let variant_source = variant.as_deref().map(parse_variant_source).transpose()?;

    info!(
        "Running `add_cmd` for '{}' on daemon '{}'...",
        source, core_node_name
    );

    ctx.connect().await?;
    let messenger_handle = ctx
        .messenger_handle()
        .ok_or_else(|| Error::ExecutionFailed("Failed to connect to daemon".to_string()))?;

    let pre_add_node_info = if !force {
        Some(fetch_node_info(messenger_handle, &core_node_name, node_source.clone()).await?)
    } else {
        None
    };

    // Check for existing instances and prompt for confirmation if needed
    if let Some(ref info) = pre_add_node_info
        && !info.instances_names.is_empty()
    {
        let node_name = info.config.manifest.name.as_str();
        let node_tag = &info.config.manifest.tag;
        let confirm = confirm_overwrite(
            node_name,
            node_tag,
            &info.instances_names,
            confirm_reader.take(),
        )?;
        if !confirm {
            return Err(Error::ExecutionFailed(
                "Node add aborted by user".to_string(),
            ));
        }
    }

    // Create and send the goal to start the add action
    // Pass max timeout as the goal timeout for daemon-side busy reporting
    let mut add_goal = NodeAddGoal::from_source(node_source, git_hash, timeouts.max_secs)
        .with_env_vars(caller_env_overrides());
    if let Some(variant_source) = variant_source {
        add_goal = add_goal.with_variant_source(variant_source);
    }
    let mut action_handle = add_goal
        .send_goal(
            messenger_handle,
            &core_node_name,
            CALLER_INSTANCE_ID,
            Some(&core_node_name),
            None,
            GOAL_TIMEOUT,
        )
        .await
        .map_err(|e| Error::ExecutionFailed(format!("Failed to send node_add goal: {}", e)))?;

    // Read the goal response to get the log file path
    let goal_response_payload = action_handle.goal_response().payload();
    let goal_response = NodeAddGoalResponse::decode(&goal_response_payload)
        .map_err(|e| Error::ExecutionFailed(format!("Failed to decode goal response: {}", e)))?;

    if !goal_response.accepted {
        return Err(Error::ExecutionFailed(format!(
            "Goal rejected: {}",
            goal_response
                .rejection_reason
                .unwrap_or_else(|| "unknown reason".to_string())
        )));
    }

    info!("Log file: {}", goal_response.log_path.display());

    // Number of lines to display in the scrolling output region
    const SCROLLING_OUTPUT_LINES: usize = 10;
    let mut scrolling_output = ScrollingOutput::new(SCROLLING_OUTPUT_LINES);

    let idle_timeout = Duration::from_secs(timeouts.idle_secs);
    let absolute_deadline = tokio::time::Instant::now() + Duration::from_secs(timeouts.max_secs);
    let mut last_activity = tokio::time::Instant::now();
    let add_result = loop {
        // Drain feedback so the publisher doesn't block on a full channel.
        loop {
            let now = tokio::time::Instant::now();
            if now >= absolute_deadline {
                scrolling_output.clear();
                return Err(Error::ExecutionFailed(format!(
                    "Timeout: max timeout of {}s exceeded. \
                     Use --max-timeout <seconds> to increase.",
                    timeouts.max_secs
                )));
            }
            if now.duration_since(last_activity) >= idle_timeout {
                scrolling_output.clear();
                return Err(Error::ExecutionFailed(format!(
                    "Timeout: no output received for {}s. \
                     Use --idle-timeout <seconds> to increase.",
                    timeouts.idle_secs
                )));
            }
            let drain_timeout = Duration::from_millis(50);
            match tokio::time::timeout(drain_timeout, action_handle.on_next_feedback()).await {
                Ok(Ok(msg)) => {
                    last_activity = tokio::time::Instant::now();
                    let payload = msg.payload();
                    if let Ok(feedback) = NodeAddFeedback::decode(payload.as_ref()) {
                        scrolling_output.add_line(&feedback.line, feedback.is_stderr());
                    }
                }
                Ok(Err(_)) => break,
                Err(_) => break,
            }
        }

        let now = tokio::time::Instant::now();
        if now >= absolute_deadline {
            scrolling_output.clear();
            return Err(Error::ExecutionFailed(format!(
                "Timeout: max timeout of {}s exceeded. \
                 Use --max-timeout <seconds> to increase.",
                timeouts.max_secs
            )));
        }
        if now.duration_since(last_activity) >= idle_timeout {
            scrolling_output.clear();
            return Err(Error::ExecutionFailed(format!(
                "Timeout: no output received for {}s. \
                 Use --idle-timeout <seconds> to increase.",
                timeouts.idle_secs
            )));
        }
        let poll_timeout = Duration::from_millis(200);
        match ActionMessenger::request_result(messenger_handle, &action_handle, poll_timeout).await
        {
            Ok(msg) => {
                let payload = msg.payload();
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

    if let (Some(name), Some(tag)) = (&add_result.node_name, &add_result.node_tag) {
        info!("Added node {}:{} to the node stack", name, tag);
    } else {
        info!("Added node to the node stack");
    }
    info!(
        "Snapshot path: {}",
        add_result.snapshot_path.to_string_lossy()
    );

    let Some(start_options) = start_options else {
        return Ok(());
    };

    let node_name = add_result.node_name.as_deref().ok_or_else(|| {
        Error::ExecutionFailed(
            "Failed to determine node name after adding. Try running `peppy node list`.".into(),
        )
    })?;
    let node_tag = add_result.node_tag.as_deref().ok_or_else(|| {
        Error::ExecutionFailed(
            "Failed to determine node tag after adding. Try running `peppy node list`.".into(),
        )
    })?;

    start_instance_async(
        messenger_handle,
        &core_node_name,
        node_name,
        node_tag,
        &start_options.args,
        start_options.instance_id,
        &timeouts,
    )
    .await?;

    Ok(())
}

/// Fetches node info for a given source using NodeInfoRequest.
/// This includes the node config, whether it's in the node stack, and running instance names.
async fn fetch_node_info(
    messenger: &MessengerHandle,
    core_node_name: &str,
    node_source: NodeSource,
) -> Result<NodeInfoResponse> {
    NodeInfoRequest::new(node_source)
        .poll(
            messenger,
            core_node_name,
            CALLER_INSTANCE_ID,
            core_node_name,
            INFO_REQUEST_TIMEOUT,
        )
        .await
        .map_err(|e| {
            Error::ExecutionFailed(format!("Failed to check node info before adding: {}", e))
        })
}

fn confirm_overwrite(
    node_name: &str,
    tag: &str,
    instance_ids: &[String],
    mut reader: Option<Box<dyn BufRead>>,
) -> Result<bool> {
    let count = instance_ids.len();
    let suffix = if count == 1 { "instance" } else { "instances" };
    let ids = instance_ids
        .iter()
        .map(|id| format!("\"{}\"", id))
        .collect::<Vec<_>>()
        .join(", ");

    print!(
        "Node `{}:{}` already exists with {} running {} ({}). \
         Adding this node will stop {} and overwrite the existing node. Continue? [y/n] ",
        node_name,
        tag,
        count,
        suffix,
        ids,
        if count == 1 { "it" } else { "them" }
    );
    io::stdout().flush().map_err(|e| {
        Error::ExecutionFailed(format!("Failed to write confirmation prompt: {}", e))
    })?;

    let mut input = String::new();
    if let Some(ref mut reader) = reader {
        reader.read_line(&mut input)
    } else {
        io::stdin().read_line(&mut input)
    }
    .map_err(|e| Error::ExecutionFailed(format!("Failed to read confirmation response: {}", e)))?;

    let response = input.trim().to_ascii_lowercase();
    Ok(matches!(response.as_str(), "y" | "yes"))
}

#[cfg(test)]
mod tests {
    use super::super::source::parse_git_repo_url_and_path;
    use super::*;

    #[test]
    fn parses_git_repo_url_and_repo_path_from_https_url() {
        let (repo_url, repo_path) = parse_git_repo_url_and_path(
            "https://github.com/Peppy-bot/example_nodes.git/fake_uvc_camera",
        )
        .expect("should parse git source");

        assert_eq!(
            repo_url.to_bstring().to_string(),
            "https://github.com/Peppy-bot/example_nodes.git"
        );
        assert_eq!(repo_path, "fake_uvc_camera");
    }

    #[test]
    fn parses_http_source_for_tar_zst_url() {
        let source = parse_node_source("https://example.com/fake_uvc_camera.tar.zst", None)
            .expect("should parse http source");

        match &source {
            NodeSource::Http { url, .. } => {
                assert_eq!(url.as_str(), "https://example.com/fake_uvc_camera.tar.zst");
            }
            other => panic!("expected http source, got {other:?}"),
        }
    }

    #[test]
    fn parses_git_source_with_ref() {
        let source = parse_node_source(
            "https://github.com/Peppy-bot/example_nodes.git/uvc_camera",
            Some("v0.1.0".to_string()),
        )
        .expect("should parse git source");

        match &source {
            NodeSource::Git {
                repo_url,
                repo_path,
                repo_ref,
            } => {
                assert_eq!(
                    repo_url.to_bstring().to_string(),
                    "https://github.com/Peppy-bot/example_nodes.git"
                );
                assert_eq!(repo_path, "uvc_camera");
                assert_eq!(repo_ref.as_deref(), Some("v0.1.0"));
            }
            other => panic!("expected git source, got {other:?}"),
        }
    }

    #[test]
    fn parses_git_source_without_ref() {
        let source = parse_node_source(
            "https://github.com/Peppy-bot/example_nodes.git/uvc_camera",
            None,
        )
        .expect("should parse git source");

        match &source {
            NodeSource::Git { repo_ref, .. } => {
                assert!(repo_ref.is_none());
            }
            other => panic!("expected git source, got {other:?}"),
        }
    }

    #[test]
    fn rejects_ref_for_http_archive() {
        let err = parse_node_source(
            "https://example.com/fake_uvc_camera.tar.zst",
            Some("v0.1.0".to_string()),
        )
        .expect_err("should reject --ref for http archive");

        assert!(err.to_string().contains("--ref"));
    }

    #[test]
    fn rejects_ref_for_local_source() {
        // Note: This test will fail to canonicalize the path since it doesn't exist,
        // but it still validates that --ref is rejected for local sources
        let err = parse_node_source("some/local/path", Some("v0.1.0".to_string()))
            .expect_err("should reject --ref for local sources");

        assert!(err.to_string().contains("--ref"));
    }

    #[test]
    fn validate_git_ref_rejects_empty() {
        let err = validate_git_ref(Some("")).expect_err("should reject empty --ref");

        assert!(err.to_string().contains("--ref"));
    }

    #[test]
    fn validate_git_ref_trims_whitespace() {
        let result =
            validate_git_ref(Some("  v0.1.0  ")).expect("should accept whitespace-padded ref");

        assert_eq!(result, Some("v0.1.0".to_string()));
    }

    #[test]
    fn validate_git_ref_accepts_none() {
        let result = validate_git_ref(None).expect("should accept None");

        assert!(result.is_none());
    }
}
