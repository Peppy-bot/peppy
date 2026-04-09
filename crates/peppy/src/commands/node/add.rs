use crate::context::DaemonConnection;
use core_node::encoding::{
    NodeActionFeedback, NodeActionGoalResponse, NodeAddGoal, NodeAddResult, NodeBuildGoal,
    NodeBuildResult, NodeInfoRequest, NodeInfoResponse, NodeSource,
};
use peppylib::MessengerHandle;
use std::io::BufRead;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

use super::TimeoutConfig;
use super::env::caller_env_overrides;
use super::source::{parse_node_source, parse_variant_source};
use super::start::start_instance_async;
use crate::commands::{CALLER_INSTANCE_ID, GOAL_TIMEOUT, SCROLLING_OUTPUT_LINES};
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
    // Validate git_ref and parse the source into a NodeSource
    let git_ref = validate_git_ref(git_ref.as_deref())?;
    let node_source = parse_node_source(&source, git_ref)?;

    // Log the resolved source path (may differ from the original when the CLI
    // walked up from a variant subdirectory to the root node directory).
    let display_source = match &node_source {
        NodeSource::Fs(p) => p.display().to_string(),
        _ => source.clone(),
    };
    if let Some(ref v) = variant {
        info!(
            "Adding node variant '{}' from root node {}...",
            v, display_source
        );
    } else {
        info!("Adding node from {}...", display_source);
    }

    // Parse variant source early so the preflight check uses the same merged config
    // that the actual add will use.
    let variant_source = variant.as_deref().map(parse_variant_source).transpose()?;

    let conn = ctx.connect_to_daemon().await?;

    info!(
        "Running `add_cmd` for '{}' on daemon '{}'...",
        source, conn.core_node_name
    );

    let pre_add_node_info = if !force {
        Some(
            fetch_node_info(
                conn.messenger,
                &conn.core_node_name,
                node_source.clone(),
                Duration::from_secs(timeouts.max_secs),
            )
            .await?,
        )
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
    let mut add_goal =
        NodeAddGoal::from_source(node_source, conn.git_hash.clone(), timeouts.max_secs)
            .with_env_vars(caller_env_overrides())
            .with_force(force);
    if let Some(variant_source) = variant_source {
        add_goal = add_goal.with_variant_source(variant_source);
    }
    let mut action_handle = add_goal
        .send_goal(
            conn.messenger,
            &conn.core_node_name,
            CALLER_INSTANCE_ID,
            Some(&conn.core_node_name),
            None,
            GOAL_TIMEOUT,
        )
        .await
        .map_err(|e| Error::ExecutionFailed(format!("Failed to send node_add goal: {}", e)))?;

    // Read the goal response to get the log file path
    let goal_response_payload = action_handle.goal_response().payload();
    let goal_response = NodeActionGoalResponse::decode(&goal_response_payload)
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

    let mut scrolling_output = ScrollingOutput::new(SCROLLING_OUTPUT_LINES);

    let add_result = crate::commands::action_poll::poll_action_to_completion(
        conn.messenger,
        &mut action_handle,
        &timeouts,
        &mut scrolling_output,
        |payload, output| {
            if let Ok(feedback) = NodeActionFeedback::decode(payload) {
                output.add_line(&feedback.line, feedback.is_stderr());
            }
        },
        |payload| match NodeAddResult::decode(payload) {
            Ok(result) => Ok(Some(result)),
            Err(err) => {
                if peppylib::encoding::is_result_pending(payload) {
                    Ok(None)
                } else {
                    Err(format!("Failed to decode node_add result: {err}"))
                }
            }
        },
    )
    .await?;

    scrolling_output.clear();

    if !add_result.success {
        return Err(Error::ExecutionFailed(
            add_result
                .error_message
                .unwrap_or_else(|| "node_add failed with no error message".to_string()),
        ));
    }

    let node_name = add_result.node_name.clone().ok_or_else(|| {
        Error::ExecutionFailed(
            "Failed to determine node name after adding. Try running `peppy node list`.".into(),
        )
    })?;
    let node_tag = add_result.node_tag.clone().ok_or_else(|| {
        Error::ExecutionFailed(
            "Failed to determine node tag after adding. Try running `peppy node list`.".into(),
        )
    })?;
    info!("Added node {}:{} to the node stack", node_name, node_tag);

    // node_add only registers the entity in `Added`. The build is now a
    // separate daemon goal — drive it before optionally starting an instance.
    let build_result = run_node_build_goal(&conn, &node_name, &node_tag, &timeouts, force).await?;
    info!(
        "Built node {}:{} (snapshot: {})",
        node_name,
        node_tag,
        build_result.snapshot_path.display()
    );

    let Some(start_options) = start_options else {
        return Ok(());
    };
    let node_name = node_name.as_str();
    let node_tag = node_tag.as_str();

    start_instance_async(
        conn.messenger,
        &conn.core_node_name,
        node_name,
        node_tag,
        &start_options.args,
        start_options.instance_id,
        &timeouts,
    )
    .await?;

    Ok(())
}

/// Sends a `node_build` goal to the daemon for an already-`Added` entity and
/// polls it to completion. Mirrors the `node_add` goal lifecycle: send goal,
/// validate goal response, stream feedback, decode result, surface failures.
async fn run_node_build_goal(
    conn: &DaemonConnection<'_>,
    node_name: &str,
    node_tag: &str,
    timeouts: &TimeoutConfig,
    force: bool,
) -> Result<NodeBuildResult> {
    let build_goal = NodeBuildGoal::new(node_name, node_tag, timeouts.max_secs).with_force(force);
    let mut action_handle = build_goal
        .send_goal(
            conn.messenger,
            &conn.core_node_name,
            CALLER_INSTANCE_ID,
            Some(&conn.core_node_name),
            None,
            GOAL_TIMEOUT,
        )
        .await
        .map_err(|e| Error::ExecutionFailed(format!("Failed to send node_build goal: {}", e)))?;

    let goal_response_payload = action_handle.goal_response().payload();
    let goal_response = NodeActionGoalResponse::decode(&goal_response_payload).map_err(|e| {
        Error::ExecutionFailed(format!("Failed to decode build goal response: {}", e))
    })?;

    if !goal_response.accepted {
        return Err(Error::ExecutionFailed(format!(
            "Build goal rejected: {}",
            goal_response
                .rejection_reason
                .unwrap_or_else(|| "unknown reason".to_string())
        )));
    }

    info!("Build log file: {}", goal_response.log_path.display());

    let mut scrolling_output = ScrollingOutput::new(SCROLLING_OUTPUT_LINES);

    let build_result = crate::commands::action_poll::poll_action_to_completion(
        conn.messenger,
        &mut action_handle,
        timeouts,
        &mut scrolling_output,
        |payload, output| {
            if let Ok(feedback) = NodeActionFeedback::decode(payload) {
                output.add_line(&feedback.line, feedback.is_stderr());
            }
        },
        |payload| match NodeBuildResult::decode(payload) {
            Ok(result) => Ok(Some(result)),
            Err(err) => {
                if peppylib::encoding::is_result_pending(payload) {
                    Ok(None)
                } else {
                    Err(format!("Failed to decode node_build result: {err}"))
                }
            }
        },
    )
    .await?;

    scrolling_output.clear();

    if !build_result.success {
        return Err(Error::ExecutionFailed(
            build_result
                .error_message
                .unwrap_or_else(|| "node_build failed with no error message".to_string()),
        ));
    }

    Ok(build_result)
}

/// Fetches node info for a given source using NodeInfoRequest.
/// This includes the node config, whether it's in the node stack, and running instance names.
async fn fetch_node_info(
    messenger: &MessengerHandle,
    core_node_name: &str,
    node_source: NodeSource,
    timeout: Duration,
) -> Result<NodeInfoResponse> {
    NodeInfoRequest::new(node_source)
        .poll(
            messenger,
            core_node_name,
            CALLER_INSTANCE_ID,
            core_node_name,
            timeout,
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
    use crate::commands::confirm::{confirm_prompt, format_instance_ids};

    let count = instance_ids.len();
    let suffix = if count == 1 { "instance" } else { "instances" };
    let ids = format_instance_ids(instance_ids);
    let pronoun = if count == 1 { "it" } else { "them" };

    let message = format!(
        "Node `{node_name}:{tag}` already exists with {count} running {suffix} ({ids}). \
         Adding this node will stop {pronoun} and overwrite the existing node. Continue? [y/n] ",
    );

    confirm_prompt(
        &message,
        reader.as_mut().map(|r| r.as_mut() as &mut dyn BufRead),
    )
}

#[cfg(test)]
mod tests {
    use super::super::source::parse_git_repo_url_and_path;
    use super::*;

    #[test]
    fn parses_git_repo_url_and_repo_path_from_https_url() {
        let (repo_url, repo_path) =
            parse_git_repo_url_and_path("https://github.com/Peppy-bot/nodes_hub.git/uvc_camera")
                .expect("should parse git source");

        assert_eq!(
            repo_url.to_bstring().to_string(),
            "https://github.com/Peppy-bot/nodes_hub.git"
        );
        assert_eq!(repo_path, "uvc_camera");
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
            "https://github.com/Peppy-bot/nodes_hub.git/uvc_camera",
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
                    "https://github.com/Peppy-bot/nodes_hub.git"
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
            "https://github.com/Peppy-bot/nodes_hub.git/uvc_camera",
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
