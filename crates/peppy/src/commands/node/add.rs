use config::node::NodeConfigParser;
use core_node::encoding::{
    NodeAddFeedback, NodeAddGoal, NodeAddGoalResponse, NodeAddResult, NodeInfoRequest, NodeSource,
};
use peppylib::{MessengerHandle, PeppyError};
use std::io::BufRead;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

use super::TimeoutConfig;
use super::env::caller_env_overrides;
use super::run::run_instance_async;
use super::source::{parse_node_source, parse_variant_source};
use crate::commands::{CALLER_INSTANCE_ID, GOAL_TIMEOUT};
use crate::context::AppContext;
use crate::error::{Error, Result};

/// Options for running a node instance immediately after adding it.
pub struct RunAfterAddOptions {
    pub args: Vec<(String, String)>,
    pub instance_id: Option<String>,
}

/// Parameters for adding a node.
pub struct AddNodeParams {
    pub source: String,
    pub git_ref: Option<String>,
    pub variant: Option<String>,
    pub run_options: Option<RunAfterAddOptions>,
    pub timeouts: TimeoutConfig,
    pub force: bool,
    pub confirm_reader: Option<Box<dyn BufRead>>,
    /// Whether to run `peppy node sync` on the source *before* adding. Only
    /// meaningful for local filesystem sources. Independent of `chain_build`
    /// — not implied by `--build`/`--run`.
    pub sync: bool,
    /// Whether to chain a `node build` after the add succeeds. Set by the
    /// CLI when the user passes `--build` (or `--run`, which implies it).
    pub chain_build: bool,
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
        run_options,
        timeouts,
        force,
        mut confirm_reader,
        sync,
        chain_build,
    } = params;
    // Validate git_ref and parse the source into a NodeSource
    let git_ref = validate_git_ref(git_ref.as_deref())?;
    let node_source = parse_node_source(&source, git_ref)?;

    // `--sync` forces a `peppy node sync` *before* the add so the snapshot
    // taken by the daemon includes freshly regenerated peppygen output.
    // Only local filesystem sources can be synced; remote sources are fetched
    // and synced server-side by the daemon.
    //
    // Note: `sync_node_async` calls `ctx.connect_to_daemon()` internally, and
    // so does the add path below. `AppContext` caches the messenger handle in
    // a `OnceCell`, so the second call is cheap and reuses the same
    // connection.
    if sync {
        match &node_source {
            NodeSource::Fs(p) => {
                super::sync::sync_node_async(ctx, Some(p.clone())).await?;
            }
            _ => {
                return Err(Error::ExecutionFailed(
                    "--sync is only valid for local node sources; remote sources are synced server-side on fetch".into(),
                ));
            }
        }
    }

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
        "Adding node '{}' on daemon '{}'...",
        source, conn.core_node_name
    );

    // Preflight conflict check: if we can extract (name, tag) from the source
    // locally without hitting the network, ask the daemon whether that node is
    // already in the stack with running instances and prompt for overwrite.
    //
    // Variants and remote sources cannot be preflighted from the client: their
    // effective (name, tag) requires fetching + merging, which only the daemon
    // does (during the add action itself). In those cases we skip the prompt
    // and let the daemon's add action stop any existing instances transparently.
    if !force
        && variant_source.is_none()
        && let NodeSource::Fs(ref path) = node_source
    {
        let running_instances = fetch_running_instances_for_local_source(
            conn.messenger,
            &conn.core_node_name,
            path,
            Duration::from_secs(timeouts.max_secs),
        )
        .await?;

        if let Some((node_name, node_tag, instance_ids)) = running_instances {
            let confirm =
                confirm_overwrite(&node_name, &node_tag, &instance_ids, confirm_reader.take())?;
            if !confirm {
                return Err(Error::ExecutionFailed(
                    "Node add aborted by user".to_string(),
                ));
            }
        }
    }

    // Create and send the goal to start the add action
    // Pass max timeout as the goal timeout for daemon-side busy reporting
    let mut add_goal = NodeAddGoal::from_source(node_source, conn.git_hash, timeouts.max_secs)
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

    let add_result = crate::commands::action_poll::run_action_with_feedback::<
        NodeAddGoalResponse,
        NodeAddFeedback,
        NodeAddResult,
    >(conn.messenger, &mut action_handle, &timeouts, "node_add")
    .await?;

    if let (Some(name), Some(tag)) = (&add_result.node_name, &add_result.node_tag) {
        info!("Added node {}:{} to the node stack", name, tag);
    } else {
        info!("Added node to the node stack");
    }

    if !chain_build && run_options.is_none() {
        return Ok(());
    }

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

    crate::commands::node::builder::build_node_async(
        conn.messenger,
        &conn.core_node_name,
        node_name,
        node_tag,
        &timeouts,
        false,
    )
    .await?;

    let Some(run_options) = run_options else {
        return Ok(());
    };

    run_instance_async(
        conn.messenger,
        &conn.core_node_name,
        node_name,
        node_tag,
        &run_options.args,
        run_options.instance_id,
        &timeouts,
    )
    .await?;

    Ok(())
}

/// Preflight check for the overwrite confirmation prompt.
///
/// Parses the node's `peppy.json5` locally to extract `(name, tag)`, then asks
/// the daemon whether that entity is currently in the stack. Returns
/// `Some((name, tag, running_instance_ids))` only when the node exists in the
/// stack AND has at least one `Running` instance — exactly the case that
/// needs a user confirmation before overwrite.
///
/// Returns `None` when:
/// - The local config can't be parsed (the add action itself will surface a
///   clearer error once it tries to use the source).
/// - The node isn't in the stack yet (nothing to confirm).
/// - The node is in the stack but has no running instances (the add action
///   can safely overwrite it without prompting).
async fn fetch_running_instances_for_local_source(
    messenger: &MessengerHandle,
    core_node_name: &str,
    source_path: &Path,
    timeout: Duration,
) -> Result<Option<(String, String, Vec<String>)>> {
    let config_path = source_path.join(config::consts::NODE_CONFIG_FILE);
    let Ok(parsed) = NodeConfigParser::from_path(&config_path) else {
        return Ok(None);
    };
    let node_name = parsed.manifest_name().to_owned();
    let node_tag = parsed.manifest_tag().to_owned();

    let info = match NodeInfoRequest::new(node_name.clone(), node_tag.clone())
        .poll(
            messenger,
            core_node_name,
            CALLER_INSTANCE_ID,
            core_node_name,
            timeout,
        )
        .await
    {
        Ok(info) => info,
        // The daemon returns `InvalidServiceRequest` when the node isn't in
        // the stack. The caller-side transport reflects that as a
        // `ServiceError` whose reason begins with "invalid service request"
        // (from `PeppyError::InvalidServiceRequest`'s Display impl). Any
        // such rejection means "not in the stack yet" — nothing to confirm.
        Err(core_node::Error::Peppylib(PeppyError::ServiceError { ref reason, .. }))
            if reason.contains("invalid service request") =>
        {
            return Ok(None);
        }
        Err(e) => {
            return Err(Error::ExecutionFailed(format!(
                "Failed to check node info before adding: {}",
                e
            )));
        }
    };

    let running: Vec<String> = info
        .instances
        .iter()
        .filter(|inst| inst.state == "running")
        .map(|inst| inst.instance_id.clone())
        .collect();

    if running.is_empty() {
        Ok(None)
    } else {
        Ok(Some((node_name, node_tag, running)))
    }
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
