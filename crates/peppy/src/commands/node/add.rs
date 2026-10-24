use crate::commands::confirm::has_or_awaits_process;
use config::node::NodeConfigParser;
use core_node_api::InstanceState;
use core_node_api::encoding::{
    NodeAddFeedback, NodeAddGoal, NodeAddGoalResponse, NodeAddResult, NodeSource,
};
use daemon_config::launcher::LinkValue;
use std::collections::BTreeMap;
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
use crate::context::{AppContext, DaemonConnection};
use crate::error::{Error, Result};

use peppylib::core_node::transport::send_goal;
/// Options that only apply when `peppy node add` chains a run after the add
/// (`--run` / `-r`). Grouping them into an `Option<RunAfterAddOptions>` on
/// [`AddNodeParams`] makes the invariant explicit in the type: positional
/// `args`, `--instance-id`, and `--link` simply do not exist on an add that
/// stops at "added" (or at "added + built"). The clap surface enforces the
/// same rule with `requires = "run"` so an invocation like `peppy node add .
/// --link feed@cam_a` is rejected at parse time rather than silently
/// dropped.
pub struct RunAfterAddOptions {
    pub args: Vec<(String, String)>,
    pub instance_id: Option<String>,
    /// The clock the chained instance reads, exactly as `peppy node run`
    /// takes it.
    pub clock: super::run::ClockChoice,
    /// The restart policy of the chained instance, parsed from the flags
    /// `peppy node run` takes.
    pub restart: config::node::RestartRequest,
    /// The `--link KEY@TARGET` and `--vacant-link SLOT=<why>` flags merged
    /// into one slot map, unifying producer bindings, pairings, observer
    /// sources and deliberate vacancies under one container. Classified by
    /// slot kind and validated by the same launcher rules that gate
    /// `peppy node run` via [`validate_and_run_instance`].
    pub links: BTreeMap<String, LinkValue>,
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

    // A local filesystem source is a caller-local path the daemon snapshots
    // on its own machine; a remote target would resolve it on the wrong
    // machine's filesystem.
    if matches!(node_source, NodeSource::Fs(_)) {
        crate::commands::reject_remote_target_for_local_path(&conn, "peppy node add")?;
    }

    // A chained run spawns an instance whose runtime config embeds this
    // machine's messaging endpoint; that is only valid on the local daemon.
    if run_options.is_some() {
        crate::commands::reject_remote_target_for_local_routing(&conn, "peppy node add --run")?;
    }

    info!(
        "Adding node '{}' on daemon '{}'...",
        source, conn.target_core_node
    );

    // Preflight conflict check: if we can cheaply determine the root
    // `(name, tag)` without network I/O, ask the daemon whether that node
    // is already in the stack with instances.
    //
    // - `NodeSource::Fs`: read `(name, tag)` from the local `peppy.json5`.
    // - `NodeSource::ResolveRef`: use the user-supplied `(name, tag)`
    //   directly; no parsing needed. (Validated at parse time by
    //   `NodeSource::resolve_ref`.)
    //
    // `Git`/`Http` root sources skip the preflight since we'd need to
    // fetch them to read the manifest; in that path the daemon's add action
    // stops the existing instances and clears the exited ones without
    // asking, and refuses while one is starting. `Pinned` and
    // `Exposures` skip it for a different reason: `parse_node_source` never
    // produces them, so the arms exist only for exhaustiveness.
    if !force {
        let tracked_instances = match &node_source {
            NodeSource::Fs(path) => {
                fetch_tracked_instances_for_local_source(
                    &conn,
                    path,
                    Duration::from_secs(timeouts.max_secs),
                )
                .await?
            }
            NodeSource::ResolveRef { name, tag } => {
                fetch_tracked_instances_for_name_tag(
                    &conn,
                    name.clone(),
                    tag.clone(),
                    Duration::from_secs(timeouts.max_secs),
                )
                .await?
            }
            NodeSource::Git { .. }
            | NodeSource::Http { .. }
            | NodeSource::Pinned { .. }
            | NodeSource::Exposures { .. } => None,
        };

        if let Some((node_name, node_tag, instances)) = tracked_instances {
            let confirm =
                confirm_overwrite(&node_name, &node_tag, &instances, confirm_reader.take())?;
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
    let mut action_handle = send_goal(
        &add_goal,
        conn.messenger,
        &conn.core_node_name,
        CALLER_INSTANCE_ID,
        Some(&conn.target_core_node),
        GOAL_TIMEOUT,
    )
    .await
    .map_err(|e| Error::ExecutionFailed(format!("Failed to send node_add goal: {}", e)))?;

    let add_result = crate::commands::action_poll::run_action_with_feedback::<
        NodeAddGoalResponse,
        NodeAddFeedback,
        NodeAddResult,
    >(conn.messenger, &mut action_handle, &timeouts, "Node add")
    .await?;

    info!(
        "Added node {}:{} to the node stack",
        add_result.node_name, add_result.node_tag
    );

    if !chain_build && run_options.is_none() {
        return Ok(());
    }

    let node_name = add_result.node_name.as_str();
    let node_tag = add_result.node_tag.as_str();

    crate::commands::node::builder::build_node_on(
        conn.messenger,
        &conn.core_node_name,
        &conn.target_core_node,
        node_name,
        node_tag,
        &timeouts,
        crate::commands::node::BuildOptions {
            force,
            rebuild: false,
        },
    )
    .await?;

    let Some(run_options) = run_options else {
        return Ok(());
    };

    let clock = super::run::resolve_clock_choice(
        conn.messenger,
        &conn.core_node_name,
        &conn.target_core_node,
        &run_options.clock,
    )
    .await?;
    validate_and_run_instance(
        conn.messenger,
        &conn.core_node_name,
        node_name,
        node_tag,
        &run_options.args,
        run_options.instance_id,
        &run_options.links,
        clock,
        run_options.restart,
        &timeouts,
    )
    .await?;

    Ok(())
}

/// Preflight check for the overwrite confirmation prompt.
///
/// Parses the node's `peppy.json5` locally to extract `(name, tag)`, then asks
/// the daemon whether that entity is currently in the stack. Returns
/// `Some((name, tag, instances))` only when the node exists in the stack AND
/// tracks an instance, each with its state: exactly the case that needs a
/// user confirmation before overwrite.
///
/// Returns `None` when:
/// - The local config can't be parsed (the add action itself will surface a
///   clearer error once it tries to use the source).
/// - The node isn't in the stack yet (the daemon answers with
///   `NodeInfoResponse::NotInStack`, a successful negative lookup).
/// - The node is in the stack but none of its instances has a process or
///   awaits a restart (the add action overwrites it without prompting and
///   clears the exited instances).
async fn fetch_tracked_instances_for_local_source(
    conn: &DaemonConnection<'_>,
    source_path: &Path,
    timeout: Duration,
) -> Result<Option<(String, String, Vec<(String, InstanceState)>)>> {
    let config_path = source_path.join(config::consts::NODE_CONFIG_FILE);
    let Ok(parsed) = NodeConfigParser::from_path(&config_path) else {
        return Ok(None);
    };
    let node_name = parsed.manifest.name.as_str().to_owned();
    let node_tag = parsed.manifest.tag.clone();
    fetch_tracked_instances_for_name_tag(conn, node_name, node_tag, timeout).await
}

/// The node `(name, tag)` with the instances the overwrite stops or whose
/// restart it cancels, each with its state: those with a process or awaiting
/// a restart, since the daemon clears an exited instance without asking.
/// `None` when the daemon has no such node or none of its instances is such.
/// Used by the `ResolveRef` preflight, which already has `(name, tag)` in
/// hand, and by the `Fs` wrapper above after it reads them from
/// `peppy.json5`.
async fn fetch_tracked_instances_for_name_tag(
    conn: &DaemonConnection<'_>,
    node_name: String,
    node_tag: String,
    timeout: Duration,
) -> Result<Option<(String, String, Vec<(String, InstanceState)>)>> {
    let tracked: Vec<_> = super::fetch_tracked_instances(conn, &node_name, &node_tag, timeout)
        .await
        .map_err(|e| {
            Error::ExecutionFailed(format!("Failed to check node info before adding: {}", e))
        })?
        .into_iter()
        .filter(|(_, state)| has_or_awaits_process(*state))
        .collect();
    Ok((!tracked.is_empty()).then_some((node_name, node_tag, tracked)))
}

fn confirm_overwrite(
    node_name: &str,
    tag: &str,
    instances: &[(String, InstanceState)],
    mut reader: Option<Box<dyn BufRead>>,
) -> Result<bool> {
    crate::commands::confirm::confirm_prompt(
        &overwrite_question(node_name, tag, instances),
        reader.as_mut().map(|r| r.as_mut() as &mut dyn BufRead),
    )
}

/// The question an overwrite of `node_name:tag` asks while `instances`, each
/// with a process or awaiting a restart, are tracked, naming each with its
/// state. Any of them may be starting when the add reaches the daemon, which
/// refuses to overwrite a node while one of its instances starts, so the
/// question says so.
fn overwrite_question(node_name: &str, tag: &str, instances: &[(String, InstanceState)]) -> String {
    use crate::commands::confirm::TrackedInstances;
    let tracked = TrackedInstances(instances);
    format!(
        "Node `{node_name}:{tag}` already exists with {tracked}. The add is refused if an \
         instance is starting when the add reaches it; otherwise adding this node stops and \
         clears {} and overwrites the existing node. Continue? [y/n] ",
        tracked.pronoun()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The overwrite question names each instance with its state, what the
    /// overwrite does to them and the refusal of an add that meets a start.
    #[test]
    fn the_overwrite_question_names_each_instance_and_its_state() {
        let instances = [
            ("cam_a".to_owned(), InstanceState::Running),
            ("cam_b".to_owned(), InstanceState::Restarting),
        ];
        assert_eq!(
            overwrite_question("cam", "v1", &instances),
            "Node `cam:v1` already exists with 2 instances (\"cam_a\" running, \"cam_b\" \
             restarting). The add is refused if an instance is starting when the add reaches it; \
             otherwise adding this node stops and clears them and overwrites the existing node. \
             Continue? [y/n] "
        );
        let restarting = [("cam_a".to_owned(), InstanceState::Restarting)];
        assert_eq!(
            overwrite_question("cam", "v1", &restarting),
            "Node `cam:v1` already exists with 1 instance (\"cam_a\" restarting). The add is \
             refused if an instance is starting when the add reaches it; otherwise adding this \
             node stops and clears it and overwrites the existing node. Continue? [y/n] "
        );
        let running = [("cam_a".to_owned(), InstanceState::Running)];
        assert_eq!(
            overwrite_question("cam", "v1", &running),
            "Node `cam:v1` already exists with 1 instance (\"cam_a\" running). The add is \
             refused if an instance is starting when the add reaches it; otherwise adding this \
             node stops and clears it and overwrites the existing node. Continue? [y/n] "
        );
        let starting = [("cam_a".to_owned(), InstanceState::Starting)];
        assert_eq!(
            overwrite_question("cam", "v1", &starting),
            "Node `cam:v1` already exists with 1 instance (\"cam_a\" starting). The add is \
             refused if an instance is starting when the add reaches it; otherwise adding this \
             node stops and clears it and overwrites the existing node. Continue? [y/n] "
        );
        let mixed = [
            ("cam_a".to_owned(), InstanceState::Running),
            ("cam_b".to_owned(), InstanceState::Restarting),
        ];
        assert_eq!(
            overwrite_question("cam", "v1", &mixed),
            "Node `cam:v1` already exists with 2 instances (\"cam_a\" running, \"cam_b\" \
             restarting). The add is refused if an instance is starting when the add reaches it; \
             otherwise adding this node stops and clears them and overwrites the existing node. \
             Continue? [y/n] "
        );
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
