use config::node::NodeConfigParser;
use core_node_api::encoding::{
    NodeAddFeedback, NodeAddGoal, NodeAddGoalResponse, NodeAddResult, NodeInfoRequest,
    NodeInfoResponse, NodeSource,
};
use peppylib::MessengerHandle;
use std::io::BufRead;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

use super::TimeoutConfig;
use super::env::caller_env_overrides;
use super::run::validate_and_run_instance;
use super::source::parse_node_source;
use crate::commands::{CALLER_INSTANCE_ID, GOAL_TIMEOUT};
use crate::context::AppContext;
use crate::error::{Error, Result};

use peppylib::core_node::transport::{poll_node_info, send_node_add};
/// Options that only apply when `peppy node add` chains a run after the add
/// (`--run` / `-r`). Grouping them into an `Option<RunAfterAddOptions>` on
/// [`AddNodeParams`] makes the invariant explicit in the type: positional
/// `args`, `--instance-id`, and `--bind` simply do not exist on an add that
/// stops at "added" (or at "added + built"). The clap surface enforces the
/// same rule with `requires = "run"` so an invocation like `peppy node add .
/// --bind feed@cam_a` is rejected at parse time rather than silently
/// dropped.
pub struct RunAfterAddOptions {
    pub args: Vec<(String, String)>,
    pub instance_id: Option<String>,
    /// `--bind KEY@VALUE` pairs to pin pinned `link_id`s to producer
    /// `instance_id`s. Validated by the same launcher rules that gate
    /// `peppy node run` via [`validate_and_run_instance`].
    pub binds: Vec<(String, String)>,
    /// `--pair LINK_ID@PEER_INSTANCE[/PEER_LINK]` pairing requests,
    /// validated and established by the same rules as `peppy node run`.
    pub pairs: Vec<(String, String)>,
    /// `--defer-pair LINK_ID` slots explicitly starting unpaired.
    pub defer_pairs: Vec<String>,
}

/// Parameters for adding a node.
pub struct AddNodeParams {
    pub source: String,
    pub git_ref: Option<String>,
    pub run_options: Option<RunAfterAddOptions>,
    pub timeouts: TimeoutConfig,
    pub force: bool,
    pub confirm_reader: Option<Box<dyn BufRead>>,
    /// Whether to run `peppy node sync` on the source *before* adding. Only
    /// meaningful for local filesystem sources. Independent of `chain_build`
    /// (not implied by `--build`/`--run`).
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
                super::sync::sync_node_async(ctx, Some(p.clone()), false).await?;
            }
            _ => {
                return Err(Error::ExecutionFailed(
                    "--sync is only valid for local node sources; remote sources are synced server-side on fetch".into(),
                ));
            }
        }
    }

    let display_source = match &node_source {
        NodeSource::Fs(p) => p.display().to_string(),
        _ => source.clone(),
    };
    info!("Adding node from {}...", display_source);

    let conn = ctx.connect_to_daemon().await?;

    info!(
        "Adding node '{}' on daemon '{}'...",
        source, conn.core_node_name
    );

    // Preflight conflict check: if we can cheaply determine the root
    // `(name, tag)` without network I/O, ask the daemon whether that node
    // is already in the stack with active instances.
    //
    // - `NodeSource::Fs`: read `(name, tag)` from the local `peppy.json5`.
    // - `NodeSource::RepoNode`: use the user-supplied `(name, tag)`
    //   directly; no parsing needed. (Validated at parse time by
    //   `NodeSource::repo_node`.)
    //
    // `Git`/`Http` root sources still skip the preflight since we'd need to
    // fetch them to read the manifest; the daemon's add action stops
    // existing instances transparently in that path.
    if !force {
        let active_instances = match &node_source {
            NodeSource::Fs(path) => {
                fetch_active_instances_for_local_source(
                    conn.messenger,
                    &conn.core_node_name,
                    path,
                    Duration::from_secs(timeouts.max_secs),
                )
                .await?
            }
            NodeSource::RepoNode { name, tag } => {
                fetch_active_instances_for_name_tag(
                    conn.messenger,
                    &conn.core_node_name,
                    name.clone(),
                    tag.clone(),
                    Duration::from_secs(timeouts.max_secs),
                )
                .await?
            }
            NodeSource::Git { .. } | NodeSource::Http { .. } => None,
        };

        if let Some((node_name, node_tag, instance_ids)) = active_instances {
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
    let add_goal = NodeAddGoal::from_source(node_source, conn.git_hash, timeouts.max_secs)
        .with_env_vars(caller_env_overrides())
        .with_force(force);
    let mut action_handle = send_node_add(
        &add_goal,
        conn.messenger,
        &conn.core_node_name,
        CALLER_INSTANCE_ID,
        Some(&conn.core_node_name),
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
        force,
    )
    .await?;

    let Some(run_options) = run_options else {
        return Ok(());
    };

    validate_and_run_instance(
        conn.messenger,
        &conn.core_node_name,
        node_name,
        node_tag,
        &run_options.args,
        run_options.instance_id,
        &run_options.binds,
        &run_options.pairs,
        &run_options.defer_pairs,
        &timeouts,
    )
    .await?;

    Ok(())
}

/// Preflight check for the overwrite confirmation prompt.
///
/// Parses the node's `peppy.json5` locally to extract `(name, tag)`, then asks
/// the daemon whether that entity is currently in the stack. Returns
/// `Some((name, tag, active_instance_ids))` only when the node exists in the
/// stack AND has at least one active instance (`Starting` or `Running`);
/// exactly the case that needs a user confirmation before overwrite. Both
/// states count as active because the daemon tears down every tracked
/// instance when it overwrites a node, regardless of lifecycle state.
///
/// Returns `None` when:
/// - The local config can't be parsed (the add action itself will surface a
///   clearer error once it tries to use the source).
/// - The node isn't in the stack yet (the daemon answers with
///   `NodeInfoResponse::NotInStack`, a successful negative lookup).
/// - The node is in the stack but has no active instances (the add action
///   can safely overwrite it without prompting).
async fn fetch_active_instances_for_local_source(
    messenger: &MessengerHandle,
    core_node_name: &str,
    source_path: &Path,
    timeout: Duration,
) -> Result<Option<(String, String, Vec<String>)>> {
    let config_path = source_path.join(config::consts::NODE_CONFIG_FILE);
    let Ok(parsed) = NodeConfigParser::from_path(&config_path) else {
        return Ok(None);
    };
    let node_name = parsed.manifest.name.as_str().to_owned();
    let node_tag = parsed.manifest.tag.clone();
    fetch_active_instances_for_name_tag(messenger, core_node_name, node_name, node_tag, timeout)
        .await
}

/// Queries the daemon for the `(name, tag)` entity in the stack and returns
/// its active (`running`/`starting`) instance ids when present. Used by the
/// `RepoNode` preflight, which already has `(name, tag)` in hand, and by
/// the `Fs` wrapper above after it reads them from `peppy.json5`.
async fn fetch_active_instances_for_name_tag(
    messenger: &MessengerHandle,
    core_node_name: &str,
    node_name: String,
    node_tag: String,
    timeout: Duration,
) -> Result<Option<(String, String, Vec<String>)>> {
    let response = poll_node_info(
        &NodeInfoRequest::new(node_name.clone(), node_tag.clone()),
        messenger,
        core_node_name,
        CALLER_INSTANCE_ID,
        core_node_name,
        timeout,
    )
    .await
    .map_err(|e| {
        Error::ExecutionFailed(format!("Failed to check node info before adding: {}", e))
    })?;

    // `NotInStack` is a normal first-time-add case; there is nothing to
    // confirm. The daemon no longer reports this as an error, so we match
    // on the response variant directly instead of sniffing error strings.
    let info = match response {
        NodeInfoResponse::NotInStack => return Ok(None),
        NodeInfoResponse::Found(info) => info,
    };

    let active: Vec<String> = info
        .instances
        .iter()
        .map(|inst| inst.instance_id.clone())
        .collect();

    if active.is_empty() {
        Ok(None)
    } else {
        Ok(Some((node_name, node_tag, active)))
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
        "Node `{node_name}:{tag}` already exists with {count} active {suffix} ({ids}). \
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
            parse_git_repo_url_and_path("https://github.com/Peppy-bot/nodes-hub.git/uvc_camera")
                .expect("should parse git source");

        assert_eq!(
            repo_url.to_bstring().to_string(),
            "https://github.com/Peppy-bot/nodes-hub.git"
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
            "https://github.com/Peppy-bot/nodes-hub.git/uvc_camera",
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
                    "https://github.com/Peppy-bot/nodes-hub.git"
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
            "https://github.com/Peppy-bot/nodes-hub.git/uvc_camera",
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
