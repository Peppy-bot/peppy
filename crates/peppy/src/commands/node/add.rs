use config::node::NodeConfigParser;
use master_node::encoding::{
    NodeAddFeedback, NodeAddGoal, NodeAddGoalResponse, NodeAddResult, NodeListRequest,
};
use node_stack::SerializedNodeGraph;
use peppylib::{ActionMessenger, MessengerHandle, PeppyError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

use super::source::{
    is_probably_remote_source, is_supported_http_archive, parse_git_repo_url_and_path,
};
use super::start::start_instance_async;
use crate::context::AppContext;
use crate::error::{Error, Result};
use crate::terminal::ScrollingOutput;

const CALLER_INSTANCE_ID: &str = "peppy-cli";
// Timeout for the goal to be accepted (should be fast)
const GOAL_TIMEOUT: Duration = Duration::from_secs(30);

struct PreparedNodeAdd {
    goal: NodeAddGoal,
    pre_add_node_ref: Option<(String, String)>,
}

fn node_ref_from_peppy_json5(peppy_json5: &Path) -> Result<(String, String)> {
    let node_config = NodeConfigParser::from_path(peppy_json5).map_err(Error::PeppyConfig)?;
    Ok((
        node_config.manifest.name.as_str().to_string(),
        node_config.manifest.tag.clone(),
    ))
}

fn prepare_node_add_goal(
    root_dir: &Path,
    source: &str,
    git_hash: &str,
    git_ref: Option<&str>,
) -> Result<PreparedNodeAdd> {
    let git_ref = git_ref.map(str::trim);
    if let Some(git_ref) = git_ref
        && git_ref.is_empty()
    {
        return Err(Error::ExecutionFailed(
            "`--ref` cannot be empty".to_string(),
        ));
    }

    if is_probably_remote_source(source) {
        if let Ok(url) = url::Url::parse(source)
            && matches!(url.scheme(), "http" | "https")
            && is_supported_http_archive(&url)
        {
            if git_ref.is_some() {
                return Err(Error::ExecutionFailed(
                    "`--ref` is only supported for git sources".to_string(),
                ));
            }
            return Ok(PreparedNodeAdd {
                goal: NodeAddGoal::new_http(url, git_hash.to_string()),
                pre_add_node_ref: None,
            });
        }

        let (repo_url, repo_path) = parse_git_repo_url_and_path(source)?;
        return Ok(PreparedNodeAdd {
            goal: NodeAddGoal::new_git(
                repo_url,
                repo_path,
                git_ref.map(str::to_owned),
                git_hash.to_string(),
            ),
            pre_add_node_ref: None,
        });
    }

    if git_ref.is_some() {
        return Err(Error::ExecutionFailed(
            "`--ref` is only supported for git sources".to_string(),
        ));
    }

    let source_path = PathBuf::from(source);
    let peppy_json5 = if source_path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json5"))
    {
        source_path
    } else {
        source_path.join("peppy.json5")
    };

    // Canonicalize the path to ensure we have an absolute path.
    // This is important for relative paths like "peppy.json5" where parent() would be empty.
    let peppy_json5 = peppy_json5.canonicalize().map_err(|e| {
        Error::ExecutionFailed(format!(
            "Failed to resolve path '{}': {}",
            peppy_json5.display(),
            e
        ))
    })?;

    let (node_name, node_tag) = node_ref_from_peppy_json5(&peppy_json5)?;

    let from_dir = peppy_json5
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| root_dir.to_path_buf());

    Ok(PreparedNodeAdd {
        goal: NodeAddGoal::new(from_dir, git_hash.to_string()),
        pre_add_node_ref: Some((node_name, node_tag)),
    })
}

#[allow(clippy::too_many_arguments)]
pub fn add_node(
    ctx: &Arc<AppContext>,
    source: String,
    git_ref: Option<String>,
    run: bool,
    args: Vec<(String, String)>,
    instance_id: Option<String>,
    timeout_secs: u64,
    force: bool,
) -> Result<()> {
    crate::commands::block_on(add_node_async(
        ctx,
        source,
        git_ref,
        run,
        args,
        instance_id,
        timeout_secs,
        force,
    ))
}

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[allow(clippy::too_many_arguments)]
async fn add_node_async(
    ctx: &Arc<AppContext>,
    source: String,
    git_ref: Option<String>,
    run: bool,
    args: Vec<(String, String)>,
    instance_id: Option<String>,
    timeout_secs: u64,
    force: bool,
) -> Result<()> {
    let daemon_state = ctx.read_daemon_state().map_err(|e| {
        Error::ExecutionFailed(format!(
            "Failed to read daemon state. Is the peppy daemon running? Error: {}",
            e
        ))
    })?;
    let master_node_name = daemon_state.master_node_name.clone();
    let git_hash = daemon_state.git_hash.clone();

    let PreparedNodeAdd {
        goal: add_goal,
        pre_add_node_ref,
    } = prepare_node_add_goal(&ctx.root_dir, &source, &git_hash, git_ref.as_deref())?;

    if let Some((node_name, node_tag)) = pre_add_node_ref.as_ref() {
        info!(
            "Running `add_cmd` for {}:{} on master '{}'...",
            node_name, node_tag, master_node_name
        );
    } else {
        info!(
            "Running `add_cmd` for '{}' on master '{}'...",
            source, master_node_name
        );
    }

    ctx.connect().await?;
    let messenger_handle = ctx
        .messenger_handle()
        .ok_or_else(|| Error::ExecutionFailed("Failed to connect to daemon".to_string()))?;

    // If we know the node name/tag from a local source, check for existing instances.
    // Use a separate connection to avoid interfering with the action messenger.
    if let Some((node_name, node_tag)) = pre_add_node_ref.as_ref()
        && !force
    {
        // Try to fetch instance IDs using a fresh connection
        if let Ok(instance_ids) =
            fetch_instance_ids(&daemon_state, &master_node_name, node_name, node_tag).await
            && !instance_ids.is_empty()
        {
            let confirm = confirm_overwrite(node_name, node_tag, &instance_ids)?;
            if !confirm {
                return Err(Error::ExecutionFailed(
                    "Node add aborted by user".to_string(),
                ));
            }
        }
    }

    // Send the goal to start the add action
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

    // Read the goal response to get the log file path
    let goal_response_payload = action_handle.goal_response().payload().to_bytes();
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

    let result_timeout = Duration::from_secs(timeout_secs);
    let deadline = tokio::time::Instant::now() + result_timeout;
    let add_result = loop {
        // Drain feedback so the publisher doesn't block on a full channel.
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                scrolling_output.clear();
                return Err(Error::ExecutionFailed(format!(
                    "Timeout waiting for node_add result after {} seconds. \
                     Use --timeout <seconds> to increase the timeout.",
                    timeout_secs
                )));
            }
            let remaining = deadline - now;
            let drain_timeout = Duration::from_millis(50).min(remaining);
            match tokio::time::timeout(drain_timeout, action_handle.on_next_feedback()).await {
                Ok(Ok(msg)) => {
                    let payload = msg.payload();
                    if let Ok(feedback) = NodeAddFeedback::decode(&payload.to_bytes()) {
                        scrolling_output.add_line(&feedback.line, feedback.is_stderr());
                    }
                }
                Ok(Err(_)) => break,
                Err(_) => break,
            }
        }

        let now = tokio::time::Instant::now();
        if now >= deadline {
            scrolling_output.clear();
            return Err(Error::ExecutionFailed(format!(
                "Timeout waiting for node_add result after {} seconds. \
                 Use --timeout <seconds> to increase the timeout.",
                timeout_secs
            )));
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

    let snapshot_node_ref =
        node_ref_from_peppy_json5(&add_result.snapshot_path.join("peppy.json5")).ok();
    let node_ref = snapshot_node_ref.or(pre_add_node_ref);

    if let Some((node_name, node_tag)) = node_ref.as_ref() {
        info!("Added node {}:{} to the node stack", node_name, node_tag);
    } else {
        info!("Added node to the node stack");
    }
    info!(
        "Snapshot path: {}",
        add_result.snapshot_path.to_string_lossy()
    );

    if !run {
        return Ok(());
    }

    let (node_name, node_tag) = node_ref.ok_or_else(|| {
        Error::ExecutionFailed(
            "Failed to determine node name/tag after adding. Try running `peppy node list`."
                .to_string(),
        )
    })?;

    start_instance_async(
        messenger_handle,
        &master_node_name,
        &node_name,
        &node_tag,
        &args,
        instance_id,
        timeout_secs,
    )
    .await?;

    Ok(())
}

/// Fetches instance IDs
async fn fetch_instance_ids(
    daemon_state: &crate::daemon_state::DaemonState,
    master_node_name: &str,
    node_name: &str,
    tag: &str,
) -> Result<Vec<String>> {
    // Create a completely fresh connection for this check to avoid
    // interfering with the main connection used for send_goal
    let messenger = MessengerHandle::from_host_port(
        config::consts::DEFAULT_MESSAGING_HOST,
        daemon_state.messaging_port,
    )
    .await
    .map_err(|e| {
        Error::ExecutionFailed(format!(
            "Failed to create messenger for instance check: {}",
            e
        ))
    })?;

    let response = NodeListRequest::new(false)
        .poll(
            &messenger,
            master_node_name,
            CALLER_INSTANCE_ID,
            master_node_name,
            REQUEST_TIMEOUT,
        )
        .await
        .map_err(|e| {
            Error::ExecutionFailed(format!(
                "Failed to check running instances before adding: {}",
                e
            ))
        })?;

    let graph: SerializedNodeGraph = serde_json::from_str(&response.graph_json)
        .map_err(|e| Error::ExecutionFailed(format!("Failed to parse graph JSON: {}", e)))?;

    Ok(graph
        .nodes
        .into_iter()
        .find(|node| node.name == node_name && node.tag == tag)
        .map(|node| node.instance_ids)
        .unwrap_or_default())
}

fn confirm_overwrite(node_name: &str, tag: &str, instance_ids: &[String]) -> Result<bool> {
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
    io::stdin().read_line(&mut input).map_err(|e| {
        Error::ExecutionFailed(format!("Failed to read confirmation response: {}", e))
    })?;

    let response = input.trim().to_ascii_lowercase();
    Ok(matches!(response.as_str(), "y" | "yes"))
}

#[cfg(test)]
mod tests {
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
    fn prepares_http_goal_for_tar_zst_url() {
        let prepared = prepare_node_add_goal(
            Path::new("/"),
            "https://example.com/fake_uvc_camera.tar.zst",
            "git-hash",
            None,
        )
        .expect("should prepare goal");

        match &prepared.goal.source {
            master_node::encoding::NodeSource::Http { url } => {
                assert_eq!(url.as_str(), "https://example.com/fake_uvc_camera.tar.zst");
            }
            other => panic!("expected http source, got {other:?}"),
        }
        assert!(prepared.pre_add_node_ref.is_none());
    }

    #[test]
    fn prepares_git_goal_with_ref_flag() {
        let prepared = prepare_node_add_goal(
            Path::new("/"),
            "https://github.com/Peppy-bot/example_nodes.git/uvc_camera",
            "git-hash",
            Some("v0.1.0"),
        )
        .expect("should prepare goal");

        match &prepared.goal.source {
            master_node::encoding::NodeSource::Git {
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
        assert!(prepared.pre_add_node_ref.is_none());
    }

    #[test]
    fn prepares_git_goal_without_ref_flag() {
        let prepared = prepare_node_add_goal(
            Path::new("/"),
            "https://github.com/Peppy-bot/example_nodes.git/uvc_camera",
            "git-hash",
            None,
        )
        .expect("should prepare goal");

        match &prepared.goal.source {
            master_node::encoding::NodeSource::Git { repo_ref, .. } => {
                assert!(repo_ref.is_none());
            }
            other => panic!("expected git source, got {other:?}"),
        }
        assert!(prepared.pre_add_node_ref.is_none());
    }

    #[test]
    fn rejects_ref_flag_for_http_archive() {
        let err = match prepare_node_add_goal(
            Path::new("/"),
            "https://example.com/fake_uvc_camera.tar.zst",
            "git-hash",
            Some("v0.1.0"),
        ) {
            Ok(_) => panic!("should reject --ref for http archive"),
            Err(err) => err,
        };

        assert!(err.to_string().contains("--ref"));
    }

    #[test]
    fn rejects_ref_flag_for_local_source() {
        let err = match prepare_node_add_goal(
            Path::new("/"),
            "some/local/path",
            "git-hash",
            Some("v0.1.0"),
        ) {
            Ok(_) => panic!("should reject --ref for local sources"),
            Err(err) => err,
        };

        assert!(err.to_string().contains("--ref"));
    }
}
