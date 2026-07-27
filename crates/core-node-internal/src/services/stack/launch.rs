mod feedback;
mod orchestrate;
mod phases;
mod federated;
mod resolve;

pub(crate) use resolve::portable_node_source;

use self::feedback::{publish_stderr, publish_stdout};
use self::orchestrate::{
    add_node_directly, build_node_directly, fail_and_clear_stack, start_node_directly,
    teardown_and_reset_stack, validate_and_order_dependencies,
};
use self::resolve::{parse_launcher_config, resolve_deployments};
use crate::Result;
use crate::services::action_loop::{GoalHandler, accept_goal, reject_goal, run_action_loop};
use crate::services::node::common::panic_message;
use crate::services::node::gate::{Admission, ConcurrencyGate};
use crate::services::node::{
    DaemonDefaults, RelationshipCoordinators, create_action_log_file, resolve_mount_path_parameters,
};
use chrono::Local;
use config::apply_parameter_defaults;
use containers::is_host_provided_mount_source;
use core_node_api::ActionId;
use core_node_api::encoding::LaunchIdentity;
use core_node_api::encoding::{
    LaunchFeedbackStep, LaunchGoal, LaunchGoalResponse, LaunchResult, NodeAddGoal, NodeAddLogEntry,
    NodeBuildGoal, NodeBuildLogEntry, NodeRunGoal, NodeRunLogEntry, NodeSource, ObservationTarget,
    PairTarget,
};
use core_node_api::names;
use daemon_config::consts::PeppyDirs;
use daemon_config::launcher::Deployment;
use futures::FutureExt;
use node_stack::NodeStack;
use parking_lot::Mutex as StdMutex;
use peppylib::messaging::SenderTarget;
use peppylib::messaging::{ActionFeedbackPublisher, ConcurrentAction, PendingGoal};
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyResult};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::debug;

/// Per-phase idle timeouts, sourced from the `LaunchGoal` payload. Each phase's clock resets
/// only on genuine subprocess/git/http activity (see `spawn_feedback_forwarder`).
#[derive(Clone, Copy)]
struct IdleTimeouts {
    add: Duration,
    build: Duration,
    run: Duration,
}

#[derive(Clone, Copy)]
pub struct StackLaunchTimeouts {
    pub node_startup: Duration,
    pub node_start_health: Duration,
    pub health_monitor_interval: Duration,
    pub health_monitor_timeout: Duration,
}

/// Daemon-wide defaults the stack launcher applies to every spawned
/// instance. Pairs with launcher overrides (`FrameworkOverrides`) and the
/// per-instance resolved values (`ResolvedFramework`); this struct is the
/// "what the daemon would pick when the instance omits the override" half.
pub struct StackLaunchDefaults {
    pub timeouts: StackLaunchTimeouts,
    /// Daemon-resolved defaults (messaging mode, subscriber buffers, liveness
    /// grace, and the `use_sim_time` default) injected into every launched
    /// node. The launch never resolves `use_sim_time` itself: it can place a
    /// node on a machine whose default differs, so the resolution belongs to
    /// whichever daemon spawns the node.
    pub daemon_defaults: DaemonDefaults,
    /// Daemon-shutdown signal, forwarded to each launched node's health monitor
    /// so it stops probing the instant a clean shutdown begins.
    pub shutdown_token: CancellationToken,
    /// Which launch this daemon's slice belongs to. Shared with the federation
    /// endpoints so a reservation and the slice it produces are one authority.
    pub(crate) slice_ownership: Arc<crate::services::federation::SliceOwnership>,
    /// This daemon's peppy version, compared against each participant's during
    /// a federated preflight.
    pub peppy_version: String,
}

#[allow(clippy::too_many_arguments)] // Mirrors the other listeners' identity args + two shared handles.
pub async fn listen_for_stack_launch(
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    peppy_dirs: PeppyDirs,
    defaults: StackLaunchDefaults,
    relationships: RelationshipCoordinators,
) -> Result<JoinHandle<Result<()>>> {
    let action = ConcurrentAction::expose(
        messenger,
        core_node_name,
        instance_id,
        SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
        ActionId::StackLaunch.name(),
        true,
    )
    .await?;

    let StackLaunchDefaults {
        timeouts,
        daemon_defaults,
        shutdown_token,
        slice_ownership,
        peppy_version,
    } = defaults;
    let handler = LaunchGoalHandler {
        context: LaunchActionContext {
            node_stack,
            messenger: messenger.clone(),
            bound_core_node: core_node_name.to_string(),
            core_instance_id: instance_id.to_string(),
            peppy_dirs,
            timeouts,
            slice_ownership,
            peppy_version,
            daemon_defaults,
            shutdown_token,
            relationships,
        },
        gate: ConcurrencyGate::new(),
    };

    let handle = tokio::spawn(async move { run_action_loop(action, handler).await });

    Ok(handle)
}

#[derive(Clone)]
struct LaunchGoalHandler {
    context: LaunchActionContext,
    gate: ConcurrencyGate,
}

fn encode_launch_rejected(reason: impl Into<String>) -> PeppyResult<Payload> {
    LaunchGoalResponse::rejected(reason).encode().map_err(|e| {
        peppylib::PeppyError::InvalidServiceRequest {
            identifier: "launch_goal".to_string(),
            reason: format!("Failed to encode response: {}", e),
        }
    })
}

impl GoalHandler for LaunchGoalHandler {
    async fn handle_goal(&self, pending: PendingGoal) {
        handle_goal_request(pending, self.context.clone(), self.gate.clone()).await
    }
}

struct ProcessLaunchContext {
    messenger: MessengerHandle,
    bound_core_node: String,
    core_instance_id: String,
    node_stack: Arc<NodeStack>,
    peppy_dirs: PeppyDirs,
    feedback_publisher: ActionFeedbackPublisher,
    log_file: Arc<StdMutex<File>>,
    log_path: PathBuf,
    env_vars: Vec<(String, String)>,
    timeouts: StackLaunchTimeouts,
    /// Whole-launch deadline. `None` means the user did not opt into a max; only idle timeouts
    /// are enforced.
    launch_deadline: Option<Instant>,
    idle_timeouts: IdleTimeouts,
    /// Daemon-resolved defaults (messaging mode, subscriber buffers, liveness grace)
    /// injected into every launched node.
    daemon_defaults: DaemonDefaults,
    /// Daemon-shutdown signal, forwarded to each launched node's health monitor.
    shutdown_token: CancellationToken,
    /// The daemon authorities forwarded into each instance's relationship
    /// lifecycle work.
    relationships: RelationshipCoordinators,
    /// Which launch this daemon's slice belongs to. Recorded once the launch
    /// commits, so `stack list` reports it and a coordinator can rediscover
    /// every participant by query.
    slice_ownership: Arc<crate::services::federation::SliceOwnership>,
    /// This daemon's peppy version, compared against each participant's during
    /// preflight so a mixed-version federation is refused before any stack is
    /// touched.
    peppy_version: String,
}

#[derive(Clone)]
struct LaunchActionContext {
    node_stack: Arc<NodeStack>,
    messenger: MessengerHandle,
    bound_core_node: String,
    core_instance_id: String,
    peppy_dirs: PeppyDirs,
    timeouts: StackLaunchTimeouts,
    daemon_defaults: DaemonDefaults,
    shutdown_token: CancellationToken,
    relationships: RelationshipCoordinators,
    slice_ownership: Arc<crate::services::federation::SliceOwnership>,
    peppy_version: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct NodeKey {
    name: String,
    tag: String,
}

impl NodeKey {
    fn new(name: impl Into<String>, tag: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            tag: tag.into(),
        }
    }

    fn label(&self) -> String {
        format!("{}:{}", self.name, self.tag)
    }
}

#[derive(Clone)]
struct PlannedDeployment {
    deployment: Deployment,
    source: NodeSource,
    node_name: String,
    node_tag: String,
    config: config::node::NodeConfig,
    /// Where this deployment sat in the launcher's list. Carried so a
    /// participant's per-deployment answers (its manifest, its hash) can be
    /// matched back to the entry they belong to.
    deployment_index: usize,
    /// This daemon's own fingerprint of the manifest, when this daemon read it.
    /// `None` for a deployment whose manifest a participant resolved instead,
    /// which is what keeps the straddle cross-check from comparing a peer's
    /// answer against itself.
    manifest_sha256: Option<String>,
}

/// Marker git_hash used for stack-launch operations.
/// When this marker is used, the node_add service skips git hash verification
/// and generates fresh peppygen files. This allows stack_launch to work with
/// local filesystem sources without requiring `peppy node sync` beforehand.
pub const STACK_LAUNCH_GIT_HASH: &str = "stack-launch";

/// Which core nodes host at least one instance of `key`, in a stable order.
///
/// A node is added and built on every machine that runs part of it, which is
/// what "several placed instances under one deployment" means operationally:
/// each daemon has to have the node present before it can start its share.
fn hosts_of(item: &PlannedDeployment, placements: &daemon_config::launcher::Placements) -> Vec<String> {
    let mut hosts: Vec<String> = item
        .deployment
        .instances
        .iter()
        .map(|instance| placements.of(instance.instance_id.as_str()).to_owned())
        .collect();
    hosts.sort();
    hosts.dedup();
    hosts
}

/// Step 6: Add and build every node, grouped by the machine that will run it.
///
/// The groups run CONCURRENTLY and each group runs in dependency order. That
/// split is deliberate: nothing orders one machine's fetch-and-build against
/// another's, and fetching plus building is where a launch spends nearly all
/// of its wall clock, so serializing across machines would make a two-machine
/// launch twice as slow for no invariant. Within a group the order is exactly
/// what a single-machine launch does, because that is where the ordering
/// actually matters (a node's transitive dependencies).
#[allow(clippy::too_many_arguments)] // Distinct inputs; bundling them would only move the list.
async fn add_nodes_to_stack(
    ctx: &ProcessLaunchContext,
    goal: &LaunchGoal,
    ordered: &[NodeKey],
    planned_by_key: &HashMap<NodeKey, PlannedDeployment>,
    placements: &daemon_config::launcher::Placements,
    federated: &federated::FederatedLaunch,
    add_log_paths: &mut Vec<NodeAddLogEntry>,
    build_log_paths: &mut Vec<NodeBuildLogEntry>,
) -> std::result::Result<(), LaunchResult> {
    publish_stdout(
        ctx,
        "Adding nodes to the stack...",
        LaunchFeedbackStep::LauncherStep,
    )
    .await;

    let mut by_core_node: BTreeMap<String, Vec<&NodeKey>> = BTreeMap::new();
    for key in ordered {
        let Some(item) = planned_by_key.get(key) else {
            continue;
        };
        for host in hosts_of(item, placements) {
            by_core_node.entry(host).or_default().push(key);
        }
    }

    let local = ctx.bound_core_node.as_str();
    let groups = futures::future::join_all(
        by_core_node
            .iter()
            .map(|(core_node, keys)| {
                add_and_build_group(ctx, goal, core_node, keys, planned_by_key, local)
            }),
    )
    .await;

    let mut failure: Option<String> = None;
    for group in groups {
        add_log_paths.extend(group.add_logs);
        build_log_paths.extend(group.build_logs);
        // Report the FIRST failure but keep collecting every group's logs: a
        // launch that failed on one machine still produced logs on the others,
        // and those are usually what explains it.
        if let Some(reason) = group.failure {
            failure.get_or_insert(reason);
        }
    }

    match failure {
        Some(reason) => Err(fail_and_clear_stack(ctx, reason, &federated.core_nodes()).await),
        None => Ok(()),
    }
}

/// What one machine's add-and-build group produced.
#[derive(Default)]
struct GroupOutcome {
    add_logs: Vec<NodeAddLogEntry>,
    build_logs: Vec<NodeBuildLogEntry>,
    failure: Option<String>,
}

async fn add_and_build_group(
    ctx: &ProcessLaunchContext,
    goal: &LaunchGoal,
    core_node: &str,
    keys: &[&NodeKey],
    planned_by_key: &HashMap<NodeKey, PlannedDeployment>,
    local: &str,
) -> GroupOutcome {
    let mut outcome = GroupOutcome::default();

    for key in keys {
        let Some(item) = planned_by_key.get(key) else {
            continue;
        };

        publish_stdout(
            ctx,
            format!("Adding {} on `{core_node}`", key.label()),
            LaunchFeedbackStep::AddingNode,
        )
        .await;

        if core_node != local {
            if let Err(reason) = add_and_build_remotely(ctx, goal, core_node, item).await {
                outcome.failure = Some(reason);
                return outcome;
            }
            continue;
        }

        let node_add_goal =
            NodeAddGoal::for_internal_execution(item.source.clone(), STACK_LAUNCH_GIT_HASH)
                .with_env_vars(ctx.env_vars.clone());

        let (result, log_path) = add_node_directly(ctx, node_add_goal).await;

        let failed = result.as_ref().map(|r| !r.success).unwrap_or(true);
        if let Some(path) = log_path {
            outcome.add_logs.push(NodeAddLogEntry {
                node_label: key.label(),
                log_path: path,
                failed,
            });
        }

        let added = match result {
            Ok(result) if result.success => result,
            Ok(result) => {
                outcome.failure = Some(format!(
                    "failed to add node {}: {}",
                    key.label(),
                    result
                        .error_message
                        .unwrap_or_else(|| "node_add failed".to_string())
                ));
                return outcome;
            }
            Err(err) => {
                outcome.failure = Some(format!("failed to add node {}: {err}", key.label()));
                return outcome;
            }
        };

        let node_name = added.node_name.clone().unwrap_or_else(|| key.name.clone());
        let node_tag = added.node_tag.clone().unwrap_or_else(|| key.tag.clone());

        // Stack launch chains directly from add into build, since the
        // launcher's contract is "the stack is up and running"; an
        // `Added` entity isn't actually buildable from the user's
        // perspective until `node build` has run.
        let (build_result, build_log_path) =
            build_node_directly(ctx, node_name, node_tag, ctx.env_vars.clone()).await;

        let build_failed = build_result.is_err();
        if let Some(path) = build_log_path {
            outcome.build_logs.push(NodeBuildLogEntry {
                node_label: key.label(),
                log_path: path,
                failed: build_failed,
            });
        }

        if let Err(err) = build_result {
            outcome.failure = Some(format!("failed to build node {}: {err}", key.label()));
            return outcome;
        }
    }

    outcome
}

/// Adds and builds one node on a peer, over the wire.
///
/// The peer's own log paths are not folded into this launch's log lists: they
/// name files on that machine, and a path the operator cannot open is worse
/// than no path. What the operator gets instead is the peer's output, relayed
/// live into this launch's feedback stream and attributed to its core node.
async fn add_and_build_remotely(
    ctx: &ProcessLaunchContext,
    goal: &LaunchGoal,
    core_node: &str,
    item: &PlannedDeployment,
) -> std::result::Result<(), String> {
    let source = crate::services::stack::portable_node_source(&item.deployment.source)?;
    // A real budget, unlike the in-process path's zero: this goal passes
    // through the peer's own concurrency gate, which reports the remaining time
    // when it refuses a second caller.
    let add_goal = NodeAddGoal::from_source(
        source,
        STACK_LAUNCH_GIT_HASH,
        ctx.idle_timeouts.add.as_secs(),
    )
    .with_env_vars(ctx.env_vars.clone())
    .with_launch_id(&goal.launch_id);
    let added = federated::run_remote_goal(ctx, core_node, &add_goal, ctx.idle_timeouts.add)
        .await
        .map_err(|reason| format!("failed to add node {}: {reason}", item.node_name))?;

    let build_goal = NodeBuildGoal::new(
        added.node_name.unwrap_or_else(|| item.node_name.clone()),
        added.node_tag.unwrap_or_else(|| item.node_tag.clone()),
        ctx.idle_timeouts.build.as_secs(),
    )
    .with_env_vars(ctx.env_vars.clone())
    .with_launch_id(&goal.launch_id);
    federated::run_remote_goal(ctx, core_node, &build_goal, ctx.idle_timeouts.build)
        .await
        .map_err(|reason| format!("failed to build node {}: {reason}", item.node_name))
}

/// Step 7: Prepare the host paths that containers on THIS machine will bind.
///
/// Scoped to the coordinator's own instances. A peer's bind sources live on the
/// peer's filesystem, so creating them here would make directories on the wrong
/// machine and still leave the peer's missing. See the Federation guide's
/// Limits section: a container node placed on a peer needs its bind sources to
/// already exist there.
async fn prepare_container_host_mounts(
    ctx: &ProcessLaunchContext,
    ordered: &[NodeKey],
    planned_by_key: &HashMap<NodeKey, PlannedDeployment>,
    placements: &daemon_config::launcher::Placements,
) -> std::result::Result<(), LaunchResult> {
    let mut mount_sources =
        match collect_container_mount_sources(ordered, planned_by_key, placements, ctx.bound_core_node.as_str()) {
            Ok(paths) => paths,
            Err(reason) => return Err(fail_and_clear_stack(ctx, reason, &[]).await),
        };

    if let Err(reason) = ensure_launch_bind_sources(ctx, &mount_sources).await {
        return Err(fail_and_clear_stack(ctx, reason, &[]).await);
    }

    // The peppy data root hosts the container build working dirs (`tmp/`),
    // built images (`built_nodes/`), and instance dirs. When it sits outside
    // `$HOME` (dev roots at `$TMPDIR/.peppy`) the Lima guest cannot see it,
    // so register it here whenever the stack has container nodes. Doing it in
    // this step front-loads the one-time VM restart a new mount triggers,
    // instead of paying it mid-build. Added after `ensure_launch_bind_sources`
    // on purpose: the root always exists and must not hit the auto-create
    // warning path. `external_lima_mount_sources` filters it out on Linux and
    // for home-relative roots (prod).
    if stack_has_container_nodes(ordered, planned_by_key) {
        match ctx.peppy_dirs.root().to_str() {
            Some(root) => mount_sources.push(root.to_owned()),
            None => {
                let reason = "peppy root path is not valid UTF-8".to_string();
                return Err(fail_and_clear_stack(ctx, reason, &[]).await);
            }
        }
    }

    let lima_mount_sources = external_lima_mount_sources(&mount_sources);
    if lima_mount_sources.is_empty() {
        return Ok(());
    }

    publish_stdout(
        ctx,
        "Preparing container host mounts",
        LaunchFeedbackStep::LauncherStep,
    )
    .await;

    let result = tokio::task::spawn_blocking(move || {
        let mut apptainer = containers::Apptainer::new()
            .map_err(|e| format!("Failed to initialize Apptainer: {e}"))?;
        let refs = lima_mount_sources
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        apptainer
            .ensure_host_mounts(&refs)
            .map_err(|e| format!("Failed to prepare container host mounts: {e}"))
    })
    .await
    .map_err(|e| format!("Failed to prepare container host mounts: {e}"))
    .and_then(|result| result);

    if let Err(reason) = result {
        return Err(fail_and_clear_stack(ctx, reason, &[]).await);
    }

    Ok(())
}

fn stack_has_container_nodes(
    ordered: &[NodeKey],
    planned_by_key: &HashMap<NodeKey, PlannedDeployment>,
) -> bool {
    ordered.iter().any(|key| {
        planned_by_key
            .get(key)
            .is_some_and(|item| item.config.execution.container.is_some())
    })
}

fn collect_container_mount_sources(
    ordered: &[NodeKey],
    planned_by_key: &HashMap<NodeKey, PlannedDeployment>,
    placements: &daemon_config::launcher::Placements,
    coordinator: &str,
) -> std::result::Result<Vec<String>, String> {
    let mut seen = HashSet::new();
    let mut mount_sources = Vec::new();

    for key in ordered {
        let Some(item) = planned_by_key.get(key) else {
            continue;
        };
        let Some(container) = item.config.execution.container.as_ref() else {
            continue;
        };
        let raw_mount_paths = container.mount_paths.as_deref().unwrap_or_default();
        if raw_mount_paths.is_empty() {
            continue;
        }

        for instance in &item.deployment.instances {
            if placements.of(instance.instance_id.as_str()) != coordinator {
                continue;
            }
            let mut arguments = instance.arguments.clone();
            let missing =
                apply_parameter_defaults(&mut arguments, &item.config.execution.parameters);
            if !missing.is_empty() {
                return Err(format!(
                    "failed to prepare container mounts for {} instance {}: Missing required parameters: {}",
                    key.label(),
                    instance.instance_id,
                    missing.join(", ")
                ));
            }

            let resolved_mount_paths =
                match resolve_mount_path_parameters(raw_mount_paths, &arguments) {
                    Ok(paths) => paths,
                    Err(msg) => {
                        return Err(format!(
                            "failed to prepare container mounts for {} instance {}: {msg}",
                            key.label(),
                            instance.instance_id,
                        ));
                    }
                };
            for mount in resolved_mount_paths {
                let src = mount_source(&mount).to_string();
                if seen.insert(src.clone()) {
                    mount_sources.push(src);
                }
            }
        }
    }

    Ok(mount_sources)
}

async fn ensure_launch_bind_sources(
    ctx: &ProcessLaunchContext,
    mount_sources: &[String],
) -> std::result::Result<(), String> {
    for src in mount_sources {
        let src_path = Path::new(src);
        if src_path.exists() || is_host_provided_mount_source(src_path) {
            continue;
        }

        std::fs::create_dir_all(src_path)
            .map_err(|e| format!("failed to create bind mount source {src}: {e}"))?;
        publish_stderr(
            ctx,
            format!(
                "auto-created missing bind mount source: {src} (if you intended to bind an existing file, this is a typo)"
            ),
            LaunchFeedbackStep::LauncherStep,
        )
        .await;
    }

    Ok(())
}

fn external_lima_mount_sources(mount_sources: &[String]) -> Vec<String> {
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }

    let home = std::env::var_os("HOME").map(PathBuf::from);
    mount_sources
        .iter()
        .filter(|src| {
            let src_path = absolute_mount_source(src);
            !is_host_provided_mount_source(&src_path)
                && home
                    .as_ref()
                    .is_none_or(|home_path| !src_path.starts_with(home_path))
        })
        .cloned()
        .collect()
}

fn absolute_mount_source(src: &str) -> PathBuf {
    let path = Path::new(src);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

fn mount_source(mount: &str) -> &str {
    mount.split(':').next().unwrap_or(mount)
}

/// Step 8: Start every instance in dependency order.
///
/// STRICTLY sequential, across machines as well as within one. The order is a
/// single global topological order and each start waits for the previous one to
/// reach Running, which is what makes "consumers start after their producers"
/// hold across a daemon boundary with no extra machinery: the coordinator is
/// the only thing sequencing, and it is sequencing one list.
///
/// It also keeps the pairing protocol intact. Each planned pair is established
/// by the LATER-started endpoint, which relies on the earlier one already being
/// Running and unpaired. Starting two instances of one wave concurrently would
/// break that for any pair inside the wave, on one machine or across two.
#[allow(clippy::too_many_arguments)] // Distinct inputs; bundling them would only move the list.
async fn start_node_instances(
    ctx: &ProcessLaunchContext,
    goal: &LaunchGoal,
    ordered: &[NodeKey],
    planned_by_key: &HashMap<NodeKey, PlannedDeployment>,
    run_log_paths: &mut Vec<NodeRunLogEntry>,
    resolved_slot_bindings: &std::collections::BTreeMap<String, config::runtime::SlotBindings>,
    planned_pairings: &[daemon_config::launcher::PlannedPairing],
    planned_observations: &[daemon_config::launcher::PlannedObservation],
    placements: &daemon_config::launcher::Placements,
    federated: &federated::FederatedLaunch,
) -> std::result::Result<(), LaunchResult> {
    // Register the planned observations whose OBSERVER runs on this daemon,
    // keyed by observer instance. As each instance reaches Running its
    // `node_run` notifies the coordinator, which delivers the source pin to
    // observers whose source is live (and re-delivers to all observers of a
    // source when that source reaches Running). Registering before any instance
    // starts means a source that comes up first still finds its observers
    // waiting.
    //
    // An observer placed on a peer is registered by THAT daemon instead, from
    // the `planned_observations` riding its own `node_run` goal: an observation
    // is a fact about the observing daemon's subscriptions, so it has to be
    // recorded where the observer actually runs.
    let (local_observations, remote_observations): (Vec<_>, Vec<_>) = planned_observations
        .iter()
        .cloned()
        .partition(|observation| {
            placements.of(observation.observer_instance_id.as_str()) == ctx.bound_core_node
        });
    ctx.relationships
        .observation()
        .register_planned(&local_observations);

    let mut observations_by_instance: HashMap<
        &str,
        std::collections::BTreeMap<String, core_node_api::encoding::ObservationTarget>,
    > = HashMap::new();
    // Which daemons must hear about each source's lifecycle. Only the planner
    // can answer this: an observer claims no slot on its source and the source
    // is deliberately unaware of it, so the daemon that owns the source has no
    // record it could consult. Computed over EVERY planned observation, not
    // just the remote-observer ones, because what matters is whether the
    // observer and its source sit on different machines.
    let mut watchers_by_source: HashMap<&str, Vec<String>> = HashMap::new();
    for observation in planned_observations {
        let observer_core_node = placements.of(observation.observer_instance_id.as_str());
        if observer_core_node == observation.source.core_node {
            continue;
        }
        let watchers = watchers_by_source
            .entry(observation.source.instance_id.as_str())
            .or_default();
        if !watchers.iter().any(|existing| existing == observer_core_node) {
            watchers.push(observer_core_node.to_owned());
        }
    }

    for observation in &remote_observations {
        observations_by_instance
            .entry(observation.observer_instance_id.as_str())
            .or_default()
            .insert(
                observation.observer_link_id.clone(),
                ObservationTarget::new(
                    observation.source.instance_id.clone(),
                    observation.source_link_id.clone(),
                    observation.source.core_node.clone(),
                ),
            );
    }
    publish_stdout(ctx, "Running nodes...", LaunchFeedbackStep::LauncherStep).await;

    // Each planned pair is established by the LATER-started endpoint's
    // `node_run` (instances start strictly sequentially in `ordered`, so at
    // that point the earlier endpoint is already Running and unpaired). The
    // later endpoint carries the fully-pinned pair request; the earlier
    // endpoint's slot rides `covered_pairs`, naming that future peer, so
    // its own coverage re-check passes and its feedback states the plan.
    // Only explicit `defer_links:` entries ride `deferred_pairs`.
    let mut start_index: HashMap<&str, usize> = HashMap::new();
    let mut requested_by_instance: HashMap<&str, std::collections::BTreeMap<String, PairTarget>> =
        HashMap::new();
    let mut covered_by_instance: HashMap<&str, std::collections::BTreeMap<String, PairTarget>> =
        HashMap::new();
    let mut deferred_by_instance: HashMap<&str, Vec<String>> = HashMap::new();
    for key in ordered {
        let Some(item) = planned_by_key.get(key) else {
            continue;
        };
        let participant_links: std::collections::BTreeSet<&str> = item
            .config
            .manifest
            .depends_on
            .as_ref()
            .into_iter()
            .flat_map(|depends_on| &depends_on.pairings)
            .filter(|dependency| dependency.is_participant())
            .map(|dependency| dependency.link_id())
            .collect();
        for instance in &item.deployment.instances {
            start_index.insert(instance.instance_id.as_str(), start_index.len());
            // Only participant slots ride the pair-specific goal field;
            // observer defers were already validated by their own family.
            deferred_by_instance
                .entry(instance.instance_id.as_str())
                .or_default()
                .extend(
                    instance
                        .defer_links
                        .iter()
                        .filter(|link_id| participant_links.contains(link_id.as_str()))
                        .cloned(),
                );
        }
    }
    for pairing in planned_pairings {
        let idx_a = start_index.get(pairing.a.instance_id.as_str()).copied();
        let idx_b = start_index.get(pairing.b.instance_id.as_str()).copied();
        let (earlier, later) = if idx_a <= idx_b {
            (&pairing.a, &pairing.b)
        } else {
            (&pairing.b, &pairing.a)
        };
        requested_by_instance
            .entry(later.instance_id.as_str())
            .or_default()
            .insert(
                later.link_id.clone(),
                PairTarget::pinned(
                    earlier.instance_id.clone(),
                    earlier.link_id.clone(),
                    placements.of(earlier.instance_id.as_str()),
                ),
            );
        covered_by_instance
            .entry(earlier.instance_id.as_str())
            .or_default()
            .insert(
                earlier.link_id.clone(),
                PairTarget::pinned(
                    later.instance_id.clone(),
                    later.link_id.clone(),
                    placements.of(later.instance_id.as_str()),
                ),
            );
    }

    let participants = federated.core_nodes();
    for key in ordered {
        let Some(item) = planned_by_key.get(key) else {
            continue;
        };

        for instance in &item.deployment.instances {
            let instance_id = instance.instance_id.as_str();
            let core_node = placements.of(instance_id).to_owned();
            publish_stdout(
                ctx,
                format!(
                    "Starting {} instance {instance_id} on `{core_node}`",
                    key.label()
                ),
                LaunchFeedbackStep::RunningNode,
            )
            .await;

            let slot_bindings = resolved_slot_bindings
                .get(instance.instance_id.as_str())
                .cloned()
                .unwrap_or_default();
            // A PLAN, not an assembled config. `node_run` supplies the
            // messaging endpoint, the bound core node, and the resolved
            // framework values from the daemon that actually spawns the node,
            // on this path exactly as on every other. One assembly site, and it
            // is what lets a peer start a node this daemon planned.
            let instance_plan = config::runtime::NodeInstancePlan {
                arguments: instance.arguments.clone(),
                use_sim_time: instance.framework.use_sim_time,
                slot_bindings,
                ..config::runtime::NodeInstancePlan::new(instance.instance_id.clone())
            };

            let local = core_node == ctx.bound_core_node;
            let node_run_goal = if local {
                NodeRunGoal::for_internal_execution(
                    instance_plan,
                    item.node_name.as_str(),
                    item.node_tag.as_str(),
                )
            } else {
                // Dispatched goals pass through the peer's concurrency gate,
                // which reports remaining time from this budget.
                NodeRunGoal::new(
                    instance_plan,
                    item.node_name.as_str(),
                    item.node_tag.as_str(),
                    ctx.idle_timeouts.run.as_secs(),
                )
            }
            .with_env_vars(ctx.env_vars.clone())
            .with_requested_pairs(
                requested_by_instance
                    .remove(instance_id)
                    .unwrap_or_default(),
            )
            .with_deferred_pairs(deferred_by_instance.remove(instance_id).unwrap_or_default())
            .with_covered_pairs(covered_by_instance.remove(instance_id).unwrap_or_default())
            .with_planned_observations(
                observations_by_instance
                    .remove(instance_id)
                    .unwrap_or_default(),
            )
            .with_lifecycle_watchers(watchers_by_source.remove(instance_id).unwrap_or_default());

            let outcome = if local {
                start_locally(ctx, key, instance_id, node_run_goal, run_log_paths).await
            } else {
                start_remotely(
                    ctx,
                    goal,
                    &core_node,
                    node_run_goal,
                    federated.manifest_sha256(&core_node, item.deployment_index),
                )
                .await
            };

            if let Err(reason) = outcome {
                let reason = format!(
                    "failed to start node {} instance {instance_id} on `{core_node}`: {reason}",
                    key.label()
                );
                return Err(fail_and_clear_stack(ctx, reason, &participants).await);
            }
        }
    }

    Ok(())
}

/// Starts one instance on this daemon, in process, recording its log entry.
async fn start_locally(
    ctx: &ProcessLaunchContext,
    key: &NodeKey,
    instance_id: &str,
    node_run_goal: NodeRunGoal,
    run_log_paths: &mut Vec<NodeRunLogEntry>,
) -> std::result::Result<(), String> {
    let log_dir = ctx.peppy_dirs.logs_dir_run();
    let log_filename = format!("{}.log", instance_id);
    let (log_file, log_path) = create_action_log_file(&log_dir, &log_filename)?;

    let (result, log_path) = start_node_directly(ctx, node_run_goal, log_path, log_file).await;

    let failed = result.as_ref().map(|r| !r.success).unwrap_or(true);
    if let Some(path) = log_path {
        run_log_paths.push(NodeRunLogEntry {
            instance_id: instance_id.to_string(),
            node_label: key.label(),
            log_path: path,
            failed,
        });
    }

    match result {
        Ok(result) if result.success => Ok(()),
        Ok(result) => Err(result
            .error_message
            .unwrap_or_else(|| "node_run failed".to_string())),
        Err(err) => Err(err),
    }
}

/// Starts one instance on a peer, pinning the manifest that peer reported
/// during preflight.
///
/// The hash is the whole point of pinning: the peer re-resolves the manifest
/// from its own cache when the goal lands, and refuses if it no longer hashes
/// the same. That closes the window between preflight and dispatch in which a
/// `repo refresh` on the peer could have moved the node out from under a plan
/// that was validated against the old one.
async fn start_remotely(
    ctx: &ProcessLaunchContext,
    goal: &LaunchGoal,
    core_node: &str,
    node_run_goal: NodeRunGoal,
    manifest_sha256: Option<&str>,
) -> std::result::Result<(), String> {
    let mut node_run_goal = node_run_goal.with_launch_id(&goal.launch_id);
    if let Some(sha) = manifest_sha256 {
        node_run_goal = node_run_goal.with_manifest_sha256(sha);
    }
    federated::run_remote_goal(ctx, core_node, &node_run_goal, ctx.idle_timeouts.run).await
}

async fn handle_goal_request(
    pending: PendingGoal,
    action_context: LaunchActionContext,
    gate: ConcurrencyGate,
) {
    let sender_instance_id = pending.instance_id().to_string();

    // Decode the goal before admission so we can capture the user-supplied timeouts.
    let goal = match LaunchGoal::decode(pending.request_bytes()) {
        Ok(g) => g,
        Err(e) => {
            reject_goal(
                pending,
                encode_launch_rejected(format!("invalid payload: {e}")),
            )
            .await;
            return;
        }
    };

    // `timeout_secs` is gate-reporting only; 0 indicates "no enforced budget"
    // (when --max-timeout-secs is unset).
    let generation = match gate.try_admit(goal.max_timeout_secs.unwrap_or(0), false) {
        // `stack_launch` never forces, so nothing is ever superseded here.
        Admission::Admitted { generation, .. } => generation,
        Admission::AlreadyRunning { .. } => {
            reject_goal(
                pending,
                encode_launch_rejected("action already in progress"),
            )
            .await;
            return;
        }
    };

    debug!("Received `stack_launch` goal from {sender_instance_id}");

    // Create log file with timestamp-based filename
    let log_dir = action_context.peppy_dirs.logs_dir_launch();
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        let error_msg = format!("Failed to create logs directory: {e}");
        debug!("Failed to create logs directory {:?}: {}", log_dir, e);
        gate.clear_running();
        reject_goal(pending, encode_launch_rejected(&error_msg)).await;
        return;
    }

    let timestamp = Local::now().format("%Y%m%d_%H%M%S_%3f");
    let log_filename = format!("launch_{}.log", timestamp);
    let log_path = log_dir.join(&log_filename);
    let log_file = match File::create(&log_path) {
        Ok(file) => Arc::new(StdMutex::new(file)),
        Err(e) => {
            let error_msg = format!("Failed to create log file: {e}");
            debug!("Failed to create log file {:?}: {}", log_path, e);
            gate.clear_running();
            reject_goal(pending, encode_launch_rejected(&error_msg)).await;
            return;
        }
    };

    debug!("Created log file for stack launch: {}", log_path.display());

    // `accept` registers the per-goal context before replying accepted.
    let Some(goal_ctx) = accept_goal(
        pending,
        LaunchGoalResponse::accepted(&log_path)
            .encode()
            .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
                identifier: "launch_goal".to_string(),
                reason: format!("Failed to encode response: {e}"),
            }),
    )
    .await
    else {
        gate.clear_running();
        return;
    };

    // Process the launch operation in a separate task to not block the loop.
    let feedback_publisher = goal_ctx
        .feedback_publisher()
        .expect("stack_launch declares a feedback topic");
    let log_path_clone = log_path.clone();
    let gate_for_task = gate.clone();
    tokio::spawn(async move {
        // Frees the gate slot on every exit: explicitly before completion on the
        // normal path (via `release_then_complete` below), or on unwind for a
        // panic. A no-op if a later goal already took over.
        let slot = gate_for_task.into_slot_guard(generation);
        let LaunchActionContext {
            messenger,
            bound_core_node,
            core_instance_id,
            node_stack,
            peppy_dirs,
            timeouts,
            slice_ownership,
            peppy_version,
            daemon_defaults,
            shutdown_token,
            relationships,
        } = action_context;
        let env_vars = goal.env_vars.clone();
        // Compute the launch deadline once. `None` => no overall deadline (idle-only).
        let launch_deadline = goal
            .max_timeout_secs
            .map(|n| Instant::now() + Duration::from_secs(n));
        let ctx = ProcessLaunchContext {
            messenger,
            bound_core_node,
            core_instance_id,
            node_stack,
            peppy_dirs,
            feedback_publisher,
            log_file,
            log_path: log_path_clone.clone(),
            env_vars,
            timeouts,
            launch_deadline,
            idle_timeouts: IdleTimeouts {
                add: Duration::from_secs(goal.node_add_idle_timeout_secs),
                build: Duration::from_secs(goal.node_build_idle_timeout_secs),
                run: Duration::from_secs(goal.node_run_idle_timeout_secs),
            },
            daemon_defaults,
            shutdown_token,
            relationships,
            slice_ownership,
            peppy_version,
        };
        // Catch panics so a panic inside the launch sequence still completes the
        // goal with a failure result, rather than leaving the client to wait out
        // the SDK's retention window for a result that never arrives. Releasing
        // the gate on panic is handled by `slot` above. Mirrors the panic
        // handling in `run_node_run` / `run_node_add` / `run_node_build`.
        let result = match AssertUnwindSafe(process_launch(goal, ctx))
            .catch_unwind()
            .await
        {
            Ok(result) => result,
            Err(panic_payload) => {
                let msg = format!(
                    "stack_launch task panicked: {}",
                    panic_message(&*panic_payload)
                );
                tracing::error!("{}", msg);
                LaunchResult::failure(&log_path_clone, msg)
            }
        };
        if let Ok(payload) = result.encode() {
            slot.release_then_complete(&goal_ctx, payload).await;
        }
    });
}

/// Process a stack launch request.
///
/// This function orchestrates the complete launch sequence:
/// 1. Parse launcher configuration
/// 2. Resolve deployments
/// 3. Validate dependencies and compute order
/// 4. Snapshot and clear stack
/// 5. Add nodes in dependency order
/// 6. Prepare stack-wide container host mounts
/// 7. Start instances in dependency order
async fn process_launch(goal: LaunchGoal, ctx: ProcessLaunchContext) -> LaunchResult {
    // Step 1: Parse the launcher and bind its core node links to machines.
    let (deployments, nodes_directory, placements) = match parse_launcher_config(&ctx, &goal).await {
        Ok(result) => result,
        Err(launch_result) => return launch_result,
    };

    // Step 2: Federated preflight. Reachability, reservations, and each
    // participant's own manifests, all BEFORE anything is resolved or torn
    // down. Reserving first is what makes every later refusal free: no machine,
    // including this one, has been touched yet.
    let federated = match federated::preflight(&ctx, &goal.launch_id, &deployments, &placements)
        .await
    {
        Ok(federated) => federated,
        Err(reason) => {
            publish_stderr(&ctx, reason.clone(), LaunchFeedbackStep::LauncherStep).await;
            return LaunchResult::failure(&ctx.log_path, reason);
        }
    };
    let participants = federated.core_nodes();

    // Step 3: Resolve deployments. Anything placed wholly on a peer takes the
    // manifest that peer resolved; the rest this daemon resolves itself.
    let planned = match resolve_deployments(
        &ctx,
        deployments,
        &nodes_directory,
        &federated.delegated_manifests(),
    )
    .await
    {
        Ok(result) => result,
        Err(launch_result) => {
            return release_and_fail(&ctx, &goal, &participants, launch_result).await;
        }
    };

    // Step 3b: A node whose instances straddle two machines must be the same
    // node on both, or the graph validated below describes neither. A planned
    // instance id must also not collide with a participant's own root entity,
    // which occupies that machine's namespace before this launch touches it.
    let mut refusals = federated.root_instance_collisions(
        &planned
            .iter()
            .flat_map(|item| &item.deployment.instances)
            .map(|instance| instance.instance_id.as_str())
            .collect(),
    );
    refusals.extend(federated.disagreeing_manifests(
        ctx.bound_core_node.as_str(),
        &planned
            .iter()
            .filter_map(|item| {
                item.manifest_sha256
                    .as_ref()
                    .map(|sha| (item.deployment_index, sha.clone()))
            })
            .collect(),
    ));
    if !refusals.is_empty() {
        let msg = daemon_config::format_bulleted(&refusals);
        publish_stderr(&ctx, msg.clone(), LaunchFeedbackStep::LauncherStep).await;
        return release_and_fail(
            &ctx,
            &goal,
            &participants,
            LaunchResult::failure(&ctx.log_path, msg),
        )
        .await;
    }

    // Step 4: Validate dependencies and compute one global topological order,
    // across every machine. There is exactly one planner.
    let root_config = ctx.node_stack.root().read().config().clone();
    let (ordered, resolved_slot_bindings, planned_pairings, planned_observations) =
        match validate_and_order_dependencies(&ctx, &planned, &root_config, &placements).await {
            Ok(result) => result,
            Err(launch_result) => {
                return release_and_fail(&ctx, &goal, &participants, launch_result).await;
            }
        };

    // Step 5: The commit point. Every participant is reserved and the whole
    // plan is validated, so now, and only now, do stacks get replaced. Peers
    // first: if one refuses, this daemon still has its own stack.
    if let Err(reason) =
        federated::begin_participant_slices(&ctx, &goal.launch_id, &participants).await
    {
        publish_stderr(&ctx, reason.clone(), LaunchFeedbackStep::LauncherStep).await;
        federated::clear_participant_slices(&ctx, &participants).await;
        return release_and_fail(
            &ctx,
            &goal,
            &participants,
            LaunchResult::failure(&ctx.log_path, reason),
        )
        .await;
    }
    teardown_and_reset_stack(&ctx).await;

    // Record which launch this daemon's slice belongs to, so the slice is
    // self-describing from here on and `stack reset` / a relaunch can
    // rediscover the whole launch by query. Each participant recorded its own
    // when it began its slice.
    ctx.slice_ownership.record_slice(LaunchIdentity::new(
        goal.launch_id.clone(),
        ctx.bound_core_node.as_str(),
    ));

    // Build lookup map
    let planned_by_key: HashMap<NodeKey, PlannedDeployment> = planned
        .into_iter()
        .map(|item| (NodeKey::new(&item.node_name, &item.node_tag), item))
        .collect();

    let mut add_log_paths: Vec<NodeAddLogEntry> = Vec::new();
    let mut build_log_paths: Vec<NodeBuildLogEntry> = Vec::new();
    let mut run_log_paths: Vec<NodeRunLogEntry> = Vec::new();

    // Step 6: Add and build, one group per machine. The groups run
    // concurrently because nothing orders one machine's add against another's,
    // and fetching plus building is where a launch spends its time; within a
    // group the dependency order is preserved exactly as on a single machine.
    let add_result = add_nodes_to_stack(
        &ctx,
        &goal,
        &ordered,
        &planned_by_key,
        &placements,
        &federated,
        &mut add_log_paths,
        &mut build_log_paths,
    )
    .await;

    // Step 7: Prepare any Lima host mounts before the first container starts.
    // Updating Lima's mount table can restart the VM; doing it lazily during
    // a later instance start would kill containers already launched by this
    // stack operation.
    let mount_result = if add_result.is_ok() {
        Some(prepare_container_host_mounts(&ctx, &ordered, &planned_by_key, &placements).await)
    } else {
        None
    };

    // Step 8: Start instances in dependency order (only if add and mount
    // preparation succeeded)
    let start_result = if add_result.is_ok() && mount_result.as_ref().is_none_or(|r| r.is_ok()) {
        Some(
            start_node_instances(
                &ctx,
                &goal,
                &ordered,
                &planned_by_key,
                &mut run_log_paths,
                &resolved_slot_bindings,
                &planned_pairings,
                &planned_observations,
                &placements,
                &federated,
            )
            .await,
        )
    } else {
        None
    };

    for result in [add_result, mount_result.unwrap_or(Ok(())), start_result.unwrap_or(Ok(()))] {
        let Err(mut launch_result) = result else {
            continue;
        };
        launch_result.node_add_logs = add_log_paths;
        launch_result.node_build_logs = build_log_paths;
        launch_result.node_run_logs = run_log_paths;
        return release_and_fail(&ctx, &goal, &participants, launch_result).await;
    }

    publish_stdout(&ctx, "Launch complete", LaunchFeedbackStep::LauncherStep).await;
    // Release every participant now that the launch is done. The SLICE record
    // stays: the reservation guards the launch, the slice describes its result,
    // and rediscovery needs the latter long after the former is gone.
    federated::release_participants(
        &ctx.messenger,
        ctx.bound_core_node.as_str(),
        ctx.core_instance_id.as_str(),
        &goal.launch_id,
        &participants,
    )
    .await;
    LaunchResult::success(&ctx.log_path).with_node_logs(
        add_log_paths,
        build_log_paths,
        run_log_paths,
    )
}

/// Releases every participant and returns the failure.
///
/// Every failure path funnels through here so a launch can never end while
/// still holding a machine. Whether the participants' stacks were also cleared
/// is a separate question the caller answers, because it depends on whether
/// anything had been dispatched to them yet.
async fn release_and_fail(
    ctx: &ProcessLaunchContext,
    goal: &LaunchGoal,
    participants: &[String],
    launch_result: LaunchResult,
) -> LaunchResult {
    federated::release_participants(
        &ctx.messenger,
        ctx.bound_core_node.as_str(),
        ctx.core_instance_id.as_str(),
        &goal.launch_id,
        participants,
    )
    .await;
    launch_result
}

/// Regression tests for the run-phase cancel-and-drain contract.
///
/// The invariant under test: when a run-phase timeout fires,
/// `run_phase_cancel_on_timeout` must signal the cancel token *and* drive the
/// phase future to completion so its cleanup runs, not drop it. Using
/// `tokio::time::pause()` + manual advancement so these tests are
/// deterministic (no wall-clock dependency, no risk of CI flake).
#[cfg(test)]
mod tests {
    use super::phases::{PhaseOutcome, run_phase_cancel_on_timeout};
    use super::*;
    use config::AnyType;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::Notify;

    /// A host-provided source must never be handed to Lima as an extra mount.
    /// Registering `/run/user` would mount the macOS side (which does not even
    /// exist) over the guest's own runtime tmpfs, and would restart the VM to
    /// do it. The guest resolves these paths itself.
    ///
    /// macOS-gated like its companion below: off macOS
    /// `external_lima_mount_sources` returns empty before consulting the
    /// filter at all, so an ungated assertion would hold with the filter
    /// deleted and prove nothing. The predicate itself is platform-independent
    /// and covered in `containers::mount_source`.
    #[test]
    #[cfg(target_os = "macos")]
    fn host_provided_sources_are_not_forwarded_to_lima() {
        let forwarded = external_lima_mount_sources(&[
            "/run/user".to_string(),
            "/dev/ttyUSB0".to_string(),
            "/proc/self".to_string(),
            "/sys/class".to_string(),
        ]);
        assert!(
            forwarded.is_empty(),
            "host-provided trees must stay out of the Lima mount list, got: {forwarded:?}"
        );
    }

    /// The complement of the test above: the filter is a carve-out, not a
    /// blanket opt-out. An ordinary path outside `$HOME` still has to reach
    /// Lima or the guest could not see it. Only meaningful on macOS, where
    /// `external_lima_mount_sources` does its work.
    #[test]
    #[cfg(target_os = "macos")]
    fn ordinary_external_sources_are_still_forwarded_to_lima() {
        let forwarded = external_lima_mount_sources(&["/opt/robot_assets".to_string()]);
        assert_eq!(
            forwarded,
            vec!["/opt/robot_assets".to_string()],
            "a non-home path the guest cannot otherwise see must be registered",
        );
    }

    /// Builds a phase future that signals `cleanup_ran` if it observes the
    /// cancel token, simulating `run_node_run`'s `abort_started` branch.
    /// If instead the outer runner drops this future, the flag stays false
    /// and the test fails, matching the real-world orphan bug.
    async fn cancellable_phase(
        cancel: CancellationToken,
        cleanup_ran: Arc<AtomicBool>,
    ) -> &'static str {
        tokio::select! {
            _ = cancel.cancelled() => {
                cleanup_ran.store(true, Ordering::SeqCst);
                "cleaned_up"
            }
            _ = std::future::pending::<()>() => unreachable!("phase should never complete on its own in these tests"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_on_timeout_awaits_cleanup_on_idle_timeout() {
        let notify = Arc::new(Notify::new());
        let token = CancellationToken::new();
        let cleanup_ran = Arc::new(AtomicBool::new(false));

        let outcome = run_phase_cancel_on_timeout(
            cancellable_phase(token.clone(), Arc::clone(&cleanup_ran)),
            Arc::clone(&notify),
            Duration::from_millis(100),
            None,
            token,
        )
        .await;

        assert!(
            matches!(outcome, PhaseOutcome::IdleTimeout),
            "idle timeout branch expected",
        );
        assert!(
            cleanup_ran.load(Ordering::SeqCst),
            "phase future must be awaited after cancel so cleanup runs; \
             dropping it would leave this flag false (the orphan-process bug)",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_on_timeout_awaits_cleanup_on_max_deadline() {
        let notify = Arc::new(Notify::new());
        let token = CancellationToken::new();
        let cleanup_ran = Arc::new(AtomicBool::new(false));

        let deadline = Instant::now() + Duration::from_millis(50);
        let outcome = run_phase_cancel_on_timeout(
            cancellable_phase(token.clone(), Arc::clone(&cleanup_ran)),
            Arc::clone(&notify),
            // Idle much larger than max so only the deadline branch can fire.
            Duration::from_secs(600),
            Some(deadline),
            token,
        )
        .await;

        assert!(
            matches!(outcome, PhaseOutcome::MaxTimeout),
            "max timeout branch expected",
        );
        assert!(
            cleanup_ran.load(Ordering::SeqCst),
            "phase future must be awaited after max-deadline cancel so cleanup runs",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_on_timeout_returns_value_when_phase_completes_first() {
        let notify = Arc::new(Notify::new());
        let token = CancellationToken::new();
        let cleanup_ran = Arc::new(AtomicBool::new(false));
        let cleanup_ran_for_phase = Arc::clone(&cleanup_ran);

        // Phase completes immediately with a value; no timeout should fire.
        let phase = async move {
            // Reset-like ping proves we keep the happy-path contract (no cancel signal).
            let _ = cleanup_ran_for_phase;
            "ok"
        };

        let outcome = run_phase_cancel_on_timeout(
            phase,
            Arc::clone(&notify),
            Duration::from_millis(100),
            Some(Instant::now() + Duration::from_millis(100)),
            token.clone(),
        )
        .await;

        match outcome {
            PhaseOutcome::Completed(v) => assert_eq!(v, "ok"),
            _ => panic!("phase should complete before any timeout fires"),
        }
        assert!(
            !token.is_cancelled(),
            "happy path must not cancel the token",
        );
    }

    fn plan_with(use_sim_time: Option<bool>) -> config::runtime::NodeInstancePlan {
        config::runtime::NodeInstancePlan {
            use_sim_time,
            ..config::runtime::NodeInstancePlan::new(
                config::runtime::Name::new("inst_1").expect("valid name"),
            )
        }
    }

    /// Per-instance override beats the daemon default in either direction.
    /// `Some(true)` forces sim even when the daemon default is wall;
    /// `Some(false)` forces wall even when the daemon default is sim.
    ///
    /// Resolution happens on the daemon that spawns the node, because only it
    /// knows its own default. A plan shipped from another machine carries the
    /// override unresolved, which is why this is tested on the plan rather than
    /// on a launcher-side helper.
    #[test]
    fn a_per_instance_use_sim_time_override_wins_over_the_daemon_default() {
        assert!(plan_with(Some(true)).resolve(false).framework.use_sim_time);
        assert!(!plan_with(Some(false)).resolve(true).framework.use_sim_time);
    }

    /// When the instance omits the override, the spawning daemon decides.
    #[test]
    fn an_absent_override_falls_through_to_the_daemon_default() {
        assert!(!plan_with(None).resolve(false).framework.use_sim_time);
        assert!(plan_with(None).resolve(true).framework.use_sim_time);
    }

    #[test]
    fn launch_mount_preflight_resolves_parameterized_sources() {
        let mut video = BTreeMap::new();
        video.insert(
            "output_dir".to_string(),
            AnyType::String("/tmp/video_reconstruction".to_string()),
        );
        let mut arguments = BTreeMap::new();
        arguments.insert("video".to_string(), AnyType::Object(video));

        let resolved = resolve_mount_path_parameters(
            &["${parameters:video.output_dir}:/frames:rw".to_string()],
            &arguments,
        )
        .expect("parameterized mount should resolve");

        assert_eq!(resolved, vec!["/tmp/video_reconstruction:/frames:rw"]);
        assert_eq!(mount_source(&resolved[0]), "/tmp/video_reconstruction");
    }

    #[test]
    fn launch_mount_preflight_rejects_non_string_parameter_sources() {
        let mut arguments = BTreeMap::new();
        arguments.insert("frame_rate".to_string(), AnyType::UInt(30));

        let err = resolve_mount_path_parameters(
            &["${parameters:frame_rate}:/frames:rw".to_string()],
            &arguments,
        )
        .expect_err("non-string mount parameter should be rejected");

        assert!(err.contains("must be a string"));
    }
}
