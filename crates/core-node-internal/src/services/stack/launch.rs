mod federated;
mod feedback;
mod orchestrate;
mod phases;
mod resolve;

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
use core_node_api::ActionId;
use core_node_api::encoding::LaunchIdentity;
use core_node_api::encoding::{
    LaunchFeedbackStep, LaunchGoal, LaunchGoalResponse, LaunchResult, NodeAddGoal, NodeAddLogEntry,
    NodeBuildGoal, NodeBuildLogEntry, NodeRunGoal, NodeRunLogEntry, NodeSource, ObservationTarget,
    ObservationTargets, PairTarget, RemotePeerPairing,
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
use std::path::PathBuf;
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
    /// The caller's forwarded environment. It describes the machine the launch
    /// was typed on, so it reaches goals executed on this daemon and stays off
    /// every goal dispatched to a peer.
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
    node_name: String,
    node_tag: String,
    config: config::node::NodeConfig,
    /// This daemon's fingerprint of the resolved manifest, echoed onto every
    /// instance dispatched to a peer so a peer whose entity moved between
    /// dispatch and start refuses rather than running a config the plan was
    /// never checked against.
    config_sha256: String,
    /// The root pin of the deployment's resolved closure.
    root_pin: daemon_config::repository::PinnedItem,
    /// The rest of the closure: dependency-node pins plus, once
    /// `mint_doc_pins` has run, the contract and pairing document pins every
    /// add of this deployment carries.
    closure_pins: Vec<daemon_config::repository::PinnedItem>,
    /// Every manifest in the deployment's closure, root first. What the
    /// doc-pin minting walks after the graph validation has had first say.
    pin_manifests: Vec<config::node::Manifest>,
}

/// The add source a deployment dispatches with: its root pin, encoded at the
/// point of dispatch beside the closure pins, so the local arm and the goal a
/// peer receives cannot disagree on how the pin travels.
fn pinned_source(
    key: &NodeKey,
    item: &PlannedDeployment,
) -> std::result::Result<NodeSource, String> {
    serde_json5::to_string(&item.root_pin)
        .map(|pin_json5| NodeSource::Pinned { pin_json5 })
        .map_err(|e| format!("deployment {}: could not encode its pin: {e}", key.label()))
}

/// Marker git_hash for an add whose bytes a pin already vouched for: the ones
/// stack launch issues, and the per-node sub-goals `add_batch` issues for any
/// pinned add (`peppy node add <name>:<tag>` included). The node_add service
/// skips git hash and codegen-fingerprint verification for it and generates
/// fresh peppygen files: those checks belong to `peppy node sync` workflows,
/// and these adds operate on a tree materialized from an already-verified pin.
pub const STACK_LAUNCH_GIT_HASH: &str = "stack-launch";

/// Which core nodes host at least one instance of `key`, in a stable order.
///
/// A node is added and built on every machine that runs part of it, which is
/// what "several placed instances under one deployment" means operationally:
/// each daemon has to have the node present before it can start its share.
fn hosts_of(
    item: &PlannedDeployment,
    placements: &daemon_config::launcher::Placements,
) -> Vec<String> {
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
    let groups = futures::future::join_all(by_core_node.iter().map(|(core_node, keys)| {
        add_and_build_group(ctx, goal, core_node, keys, planned_by_key, local)
    }))
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
            if let Err(reason) =
                add_and_build_remotely(ctx, goal, core_node, key, item, &mut outcome).await
            {
                outcome.failure = Some(reason);
                return outcome;
            }
            continue;
        }

        // The identical source and pins a peer would receive: a launch adds
        // one set of bytes wherever a deployment lands, so the local arm
        // must not get to differ from the dispatched one. The environment is
        // the one deliberate difference: the caller's env vars describe this
        // machine, so they apply here and stay off the goals a peer receives.
        let encoded = pinned_source(key, item).and_then(|source| {
            crate::services::node::pins::encode_pins(&item.closure_pins).map(|pins| (source, pins))
        });
        let node_add_goal = match encoded {
            Ok((source, pins)) => {
                NodeAddGoal::for_internal_execution(source, STACK_LAUNCH_GIT_HASH)
                    .with_env_vars(ctx.env_vars.clone())
                    .with_pins(pins)
            }
            Err(reason) => {
                outcome.failure = Some(reason);
                return outcome;
            }
        };

        let (result, log_path) = add_node_directly(ctx, node_add_goal).await;

        let failed = result.as_ref().map(|r| !r.success).unwrap_or(true);
        if let Some(path) = log_path {
            outcome.add_logs.push(NodeAddLogEntry {
                node_label: key.label(),
                log_path: path,
                failed,
                core_node: local.to_owned(),
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
                core_node: local.to_owned(),
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
/// The goal carries the same pinned source and closure pins the local arm
/// uses: the peer materializes the coordinator's decision, reusing its own
/// content on a fingerprint match and fetching the pinned commit otherwise,
/// and never resolves a name against its own cache.
///
/// Each accepted goal's log entry lands in this launch's log lists exactly as
/// a local one does, stamped with the peer's core node because the path names
/// a file on that machine's filesystem. The peer's output is also relayed
/// live into this launch's feedback stream, attributed to its core node.
///
/// Neither goal carries the caller's forwarded environment. Those values
/// (PATH first among them) describe the coordinator's machine, and a build
/// that resolves its tools through another machine's PATH fails on hosts
/// that have the toolchain installed. The peer's daemon supplies its own
/// environment to whatever these goals spawn.
async fn add_and_build_remotely(
    ctx: &ProcessLaunchContext,
    goal: &LaunchGoal,
    core_node: &str,
    key: &NodeKey,
    item: &PlannedDeployment,
    outcome: &mut GroupOutcome,
) -> std::result::Result<(), String> {
    // A real budget, unlike the in-process path's zero: this goal passes
    // through the peer's own concurrency gate, which reports the remaining time
    // when it refuses a second caller.
    let add_goal = NodeAddGoal::from_source(
        pinned_source(key, item)?,
        STACK_LAUNCH_GIT_HASH,
        ctx.idle_timeouts.add.as_secs(),
    )
    .with_launch_id(&goal.launch_id)
    .with_pins(crate::services::node::pins::encode_pins(
        &item.closure_pins,
    )?);
    let added =
        match federated::run_remote_goal(ctx, core_node, &add_goal, ctx.idle_timeouts.add).await {
            Ok(run) => {
                outcome.add_logs.push(NodeAddLogEntry {
                    node_label: key.label(),
                    log_path: run.log_path,
                    failed: run.outcome.is_err(),
                    core_node: core_node.to_owned(),
                });
                run.outcome
            }
            Err(reason) => Err(reason),
        }
        .map_err(|reason| format!("failed to add node {}: {reason}", item.node_name))?;

    let build_goal = NodeBuildGoal::new(
        added.node_name.unwrap_or_else(|| item.node_name.clone()),
        added.node_tag.unwrap_or_else(|| item.node_tag.clone()),
        ctx.idle_timeouts.build.as_secs(),
    )
    .with_launch_id(&goal.launch_id);
    match federated::run_remote_goal(ctx, core_node, &build_goal, ctx.idle_timeouts.build).await {
        Ok(run) => {
            outcome.build_logs.push(NodeBuildLogEntry {
                node_label: key.label(),
                log_path: run.log_path,
                failed: run.outcome.is_err(),
                core_node: core_node.to_owned(),
            });
            run.outcome
        }
        Err(reason) => Err(reason),
    }
    .map_err(|reason| format!("failed to build node {}: {reason}", item.node_name))
}

/// Step 7: Prepare the host paths that containers on THIS machine will bind.
///
/// Scoped to the coordinator's own instances: every machine prepares what its
/// own containers bind, and a participant was handed its share when it was told
/// to replace its slice. Creating a peer's paths here would make directories on
/// the wrong machine.
///
/// Its CLEANUP is not so scoped. This step runs after step 6, by which point
/// every participant has had its stack replaced and its nodes added and built,
/// so a failure here has to clear their slices too; only the preparation work
/// belongs to the coordinator alone.
async fn prepare_container_host_mounts(
    ctx: &ProcessLaunchContext,
    ordered: &[NodeKey],
    planned_by_key: &HashMap<NodeKey, PlannedDeployment>,
    mut mount_sources: Vec<String>,
    participants: &[String],
) -> std::result::Result<(), LaunchResult> {
    // The peppy data root hosts the container build working dirs (`tmp/`),
    // built images (`built_nodes/`), and instance dirs. When it sits outside
    // `$HOME` (dev roots at `$TMPDIR/.peppy`) the Lima guest cannot see it,
    // so register it here whenever the stack has container nodes. It always
    // exists, so it never reaches the auto-create warning path, and
    // `external_lima_mount_sources` filters it out on Linux and for
    // home-relative roots (prod).
    if stack_has_container_nodes(ordered, planned_by_key) {
        match ctx.peppy_dirs.root().to_str() {
            Some(root) => mount_sources.push(root.to_owned()),
            None => {
                let reason = "peppy root path is not valid UTF-8".to_string();
                return Err(fail_and_clear_stack(ctx, reason, participants).await);
            }
        }
    }

    if mount_sources.is_empty() {
        return Ok(());
    }

    publish_stdout(
        ctx,
        "Preparing container host mounts",
        LaunchFeedbackStep::LauncherStep,
    )
    .await;

    match super::container_mounts::prepare_container_mounts(&mount_sources).await {
        Ok(auto_created) => {
            for src in auto_created {
                publish_stderr(
                    ctx,
                    containers::auto_created_warning(&src),
                    LaunchFeedbackStep::LauncherStep,
                )
                .await;
            }
            Ok(())
        }
        Err(reason) => Err(fail_and_clear_stack(ctx, reason, participants).await),
    }
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

/// The host paths each machine's container instances bind, keyed by core node.
///
/// Resolved here, on the coordinator, because only the coordinator holds the
/// whole plan: a mount path may name an instance parameter, and a machine is
/// handed one instance at a time. Machines with nothing to bind are absent
/// rather than present-and-empty, so a caller iterating this map is iterating
/// the machines that have work to do.
///
/// Called before the launch turns destructive, which is what makes an
/// unresolvable mount path (a parameter with no value) cost nobody their stack.
fn container_mount_sources_by_machine(
    planned: &[PlannedDeployment],
    placements: &daemon_config::launcher::Placements,
) -> std::result::Result<HashMap<String, Vec<String>>, String> {
    let mut by_machine: HashMap<String, Vec<String>> = HashMap::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();

    for item in planned {
        let Some(container) = item.config.execution.container.as_ref() else {
            continue;
        };
        let raw_mount_paths = container.mount_paths.as_deref().unwrap_or_default();
        if raw_mount_paths.is_empty() {
            continue;
        }
        let label = NodeKey::new(&item.node_name, &item.node_tag).label();

        for instance in &item.deployment.instances {
            let machine = placements.of(instance.instance_id.as_str());
            let mut arguments = instance.arguments.clone();
            let missing =
                apply_parameter_defaults(&mut arguments, &item.config.execution.parameters);
            if !missing.is_empty() {
                return Err(format!(
                    "failed to prepare container mounts for {label} instance {}: Missing required parameters: {}",
                    instance.instance_id,
                    missing.join(", ")
                ));
            }

            let resolved_mount_paths = resolve_mount_path_parameters(raw_mount_paths, &arguments)
                .map_err(|msg| {
                format!(
                    "failed to prepare container mounts for {label} instance {}: {msg}",
                    instance.instance_id,
                )
            })?;
            for mount in resolved_mount_paths {
                let src = containers::mount_spec_source(&mount).to_string();
                if seen.insert((machine.to_owned(), src.clone())) {
                    by_machine.entry(machine.to_owned()).or_default().push(src);
                }
            }
        }
    }

    Ok(by_machine)
}

/// The environment one launched instance is started with: the forwarded
/// caller environment, when the instance runs on the coordinator's own
/// machine, with the instance's own `env_vars` layered on top so a deployment
/// can pin what differs per instance (a device path, a board id) without
/// depending on whoever ran the launch. An instance placed on a peer starts
/// from an empty forwarded set, because the caller's environment describes
/// the caller's machine and no other.
///
/// A key declared by the instance replaces the forwarded one rather than
/// appearing twice: the spawn paths differ on duplicates (a process node's
/// `Command::env` keeps the last, apptainer's `--env` flags are order-dependent
/// in their own way), so the ambiguity is resolved here, once. Forwarded order
/// is preserved and the instance's own entries follow in name order, which
/// keeps the resulting command line stable for a given launcher file.
///
/// Only `node_run` takes this: `env_vars` belong to an instance, while adding
/// and building a node happen once for every instance that deploys it.
fn instance_environment(
    forwarded: &[(String, String)],
    instance_env: &std::collections::BTreeMap<String, String>,
) -> Vec<(String, String)> {
    forwarded
        .iter()
        .filter(|(key, _)| !instance_env.contains_key(key))
        .cloned()
        .chain(
            instance_env
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        )
        .collect()
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
    let local_observations: Vec<_> = planned_observations
        .iter()
        .filter(|observation| {
            placements.of(observation.observer_instance_id.as_str()) == ctx.bound_core_node
        })
        .cloned()
        .collect();
    ctx.relationships
        .observation()
        .register_planned(&local_observations);

    // Accumulated per observer slot, in plan order: a slot with N members
    // contributes N entries to one list, and that list becomes the slot's
    // `ObservationTargets` below.
    let mut observations_by_instance: HashMap<
        &str,
        std::collections::BTreeMap<String, Vec<core_node_api::encoding::ObservationTarget>>,
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
        if !watchers
            .iter()
            .any(|existing| existing == observer_core_node)
        {
            watchers.push(observer_core_node.to_owned());
        }
    }

    // Every observer's own slots ride its `node_run` goal, wherever the plan
    // placed it. A peer daemon needs them to register the observation at all; a
    // local observer is already registered above, and re-registering it merges
    // the same records back over themselves. Both need them for the goal map's
    // other job: stamping the spawning instance's boot config with each slot's
    // member set, which is what lets a node read its observed membership during
    // setup instead of waiting for the delivery that follows Running.
    for observation in planned_observations {
        observations_by_instance
            .entry(observation.observer_instance_id.as_str())
            .or_default()
            .entry(observation.observer_link_id.clone())
            .or_default()
            .push(ObservationTarget::new(
                observation.source.instance_id.clone(),
                observation.source_link_id.clone(),
                observation.source.core_node.clone(),
            ));
    }
    publish_stdout(ctx, "Running nodes...", LaunchFeedbackStep::LauncherStep).await;

    // Each planned pair is established by the LATER-started endpoint's
    // `node_run` (instances start strictly sequentially in `ordered`, so at
    // that point the earlier endpoint is already Running and unpaired). The
    // later endpoint carries the fully-pinned pair request; the earlier
    // endpoint's slot rides `covered_pairs`, naming that future peer, so
    // its own coverage re-check passes and its feedback states the plan.
    // Only slots the launcher declared `{ vacant: "<why>" }` ride
    // `vacant_pairs`.
    let mut start_index: HashMap<&str, usize> = HashMap::new();
    let mut requested_by_instance: HashMap<&str, std::collections::BTreeMap<String, PairTarget>> =
        HashMap::new();
    let mut covered_by_instance: HashMap<&str, std::collections::BTreeMap<String, PairTarget>> =
        HashMap::new();
    let mut vacant_by_instance: HashMap<&str, std::collections::BTreeMap<String, String>> =
        HashMap::new();
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
            .map(|dependency| dependency.link_id.as_str())
            .collect();
        for instance in &item.deployment.instances {
            start_index.insert(instance.instance_id.as_str(), start_index.len());
            // Only participant slots ride the pair-specific goal field; an
            // observer vacancy was already validated by its own family and
            // produces no goal state, exactly as an observer link does.
            vacant_by_instance
                .entry(instance.instance_id.as_str())
                .or_default()
                .extend(daemon_config::launcher::participant_vacancies(
                    &instance.links,
                    &participant_links,
                ));
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
        // A pair whose two endpoints sit on different machines cannot be
        // validated by either daemon: each holds one manifest. This
        // coordinator holds both and has already checked them against each
        // other, so it sends that verdict along with the request. A
        // same-daemon pair carries none, and the receiver's own manifests
        // decide as before.
        // `host` is the machine running the instance that receives the goal;
        // `peer` is the endpoint on the other end of the pair.
        let pair_target =
            |host: &daemon_config::launcher::PlannedPairEndpoint,
             peer: &daemon_config::launcher::PlannedPairEndpoint| {
                let peer_core_node = placements.of(peer.instance_id.as_str());
                let target = PairTarget::pinned(
                    peer.instance_id.clone(),
                    peer.link_id.clone(),
                    peer_core_node,
                );
                if peer_core_node == placements.of(host.instance_id.as_str()) {
                    return target;
                }
                target.with_remote_peer(RemotePeerPairing {
                    pairing_name: pairing.pairing_name.clone(),
                    pairing_tag: pairing.pairing_tag.clone(),
                    peer_role: peer.role.clone(),
                })
            };

        requested_by_instance
            .entry(later.instance_id.as_str())
            .or_default()
            .insert(later.link_id.clone(), pair_target(later, earlier));
        covered_by_instance
            .entry(earlier.instance_id.as_str())
            .or_default()
            .insert(earlier.link_id.clone(), pair_target(earlier, later));
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
            // The caller's forwarded environment applies to instances started
            // on this machine, which it describes. An instance on a peer gets
            // only the env_vars its launcher file declares for it.
            let forwarded_env: &[(String, String)] = if local { &ctx.env_vars } else { &[] };
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
            .with_env_vars(instance_environment(forwarded_env, &instance.env_vars))
            .with_requested_pairs(
                requested_by_instance
                    .remove(instance_id)
                    .unwrap_or_default(),
            )
            .with_vacant_pairs(vacant_by_instance.remove(instance_id).unwrap_or_default())
            .with_covered_pairs(covered_by_instance.remove(instance_id).unwrap_or_default())
            .with_planned_observations(ObservationTargets::slots_from_plan(
                observations_by_instance
                    .remove(instance_id)
                    .unwrap_or_default(),
            ))
            .with_lifecycle_watchers(watchers_by_source.remove(instance_id).unwrap_or_default());

            let outcome = if local {
                start_locally(ctx, key, instance_id, node_run_goal, run_log_paths).await
            } else {
                start_remotely(
                    ctx,
                    goal,
                    &core_node,
                    key,
                    instance_id,
                    node_run_goal,
                    &item.config_sha256,
                    run_log_paths,
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
            core_node: ctx.bound_core_node.clone(),
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

/// Starts one instance on a peer, pinning the manifest this coordinator
/// resolved for its deployment, and recording its log entry stamped with the
/// peer's core node.
///
/// The hash closes the window between add and start: the peer compares it
/// against the entity now in its stack and refuses if the two no longer
/// hash the same, so an entity replaced out from under the plan fails
/// loudly instead of starting a node the plan was never checked against.
/// Every remote instance carries it, straddling deployments included,
/// because the coordinator resolved every deployment itself.
#[allow(clippy::too_many_arguments)] // Distinct inputs; bundling them would only move the list.
async fn start_remotely(
    ctx: &ProcessLaunchContext,
    goal: &LaunchGoal,
    core_node: &str,
    key: &NodeKey,
    instance_id: &str,
    node_run_goal: NodeRunGoal,
    config_sha256: &str,
    run_log_paths: &mut Vec<NodeRunLogEntry>,
) -> std::result::Result<(), String> {
    let node_run_goal = node_run_goal
        .with_launch_id(&goal.launch_id)
        .with_manifest_sha256(config_sha256);
    match federated::run_remote_goal(ctx, core_node, &node_run_goal, ctx.idle_timeouts.run).await {
        Ok(run) => {
            run_log_paths.push(NodeRunLogEntry {
                instance_id: instance_id.to_string(),
                node_label: key.label(),
                log_path: run.log_path,
                failed: run.outcome.is_err(),
                core_node: core_node.to_owned(),
            });
            run.outcome
        }
        Err(reason) => Err(reason),
    }
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
/// 2. Resolve deployments and mint their node pins
/// 3. Validate dependencies and compute order, then mint the doc pins
/// 4. Federated preflight, carrying the pins
/// 5. Snapshot and clear stack
/// 6. Add nodes in dependency order
/// 7. Prepare stack-wide container host mounts
/// 8. Start instances in dependency order
async fn process_launch(goal: LaunchGoal, ctx: ProcessLaunchContext) -> LaunchResult {
    // Step 1: Parse the launcher and bind its core node links to machines.
    let (deployments, placements) = match parse_launcher_config(&ctx, &goal).await {
        Ok(result) => result,
        Err(launch_result) => return launch_result,
    };

    // Step 2: Resolve every deployment, once, on this daemon, minting the
    // node pins the whole launch runs. Resolution touches no other machine
    // and tears nothing down, so refusing here is free, and the
    // reservations below need the pins to carry.
    let mut planned = match resolve_deployments(&ctx, deployments, &placements).await {
        Ok(result) => result,
        Err(launch_result) => return launch_result,
    };

    // Step 3: Validate dependencies and compute one global topological order,
    // across every machine. There is exactly one planner. Runs before the
    // doc pins are minted so a graph refusal, which points at the launcher
    // and the manifests, has first say over a document missing from this
    // machine's caches.
    let root_config = ctx.node_stack.root().read().config().clone();
    let (ordered, resolved_slot_bindings, planned_pairings, planned_observations) =
        match validate_and_order_dependencies(&ctx, &planned, &root_config, &placements).await {
            Ok(result) => result,
            Err(launch_result) => return launch_result,
        };

    // Step 3b: Pin the contract and pairing documents every manifest in the
    // launch names. Still before any reservation, so a document this
    // machine cannot pin refuses the launch while it has cost nothing.
    if let Err(launch_result) = resolve::mint_doc_pins(&ctx, &mut planned, &placements).await {
        return launch_result;
    }

    // Step 4: Federated preflight. Reachability and reservations, each
    // carrying its participant's pins, all BEFORE anything is torn down: a
    // refusal at this point has cost no machine, including this one, its
    // stack.
    let federated = match federated::preflight(&ctx, &goal.launch_id, &planned, &placements).await {
        Ok(federated) => federated,
        Err(reason) => {
            publish_stderr(&ctx, reason.clone(), LaunchFeedbackStep::LauncherStep).await;
            return LaunchResult::failure(&ctx.log_path, reason);
        }
    };
    let participants = federated.core_nodes();

    // Step 4b: A planned instance id must not collide with a participant's
    // own root entity, which occupies that machine's namespace before this
    // launch touches it.
    let refusals = federated.root_instance_collisions(
        &planned
            .iter()
            .flat_map(|item| &item.deployment.instances)
            .map(|instance| instance.instance_id.as_str())
            .collect(),
    );
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

    // Step 4c: Work out what each machine's containers will bind, while a
    // failure is still free. A mount path that names a parameter with no value
    // fails here, before any stack is replaced, rather than on the machine that
    // would have had to bind it.
    let mount_sources_by_machine = match container_mount_sources_by_machine(&planned, &placements) {
        Ok(by_machine) => by_machine,
        Err(reason) => {
            publish_stderr(&ctx, reason.clone(), LaunchFeedbackStep::LauncherStep).await;
            return release_and_fail(
                &ctx,
                &goal,
                &participants,
                LaunchResult::failure(&ctx.log_path, reason),
            )
            .await;
        }
    };

    // Step 5: The commit point. Every participant is reserved and the whole
    // plan is validated, so now, and only now, do stacks get replaced. Peers
    // first: if one refuses, this daemon still has its own stack. Each one is
    // handed the bind sources its slice needs, to prepare while it is empty.
    if let Err(reason) = federated::begin_participant_slices(
        &ctx,
        &goal.launch_id,
        &participants,
        &mount_sources_by_machine,
    )
    .await
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
    //
    // Steps 6-8 short-circuit: each phase appends to the log vectors before
    // returning `Err`, so the logs collected up to a failure are reported
    // whichever phase failed.
    let outcome = async {
        add_nodes_to_stack(
            &ctx,
            &goal,
            &ordered,
            &planned_by_key,
            &placements,
            &federated,
            &mut add_log_paths,
            &mut build_log_paths,
        )
        .await?;

        // Step 7: Prepare any Lima host mounts before the first container
        // starts. Updating Lima's mount table can restart the VM; doing it
        // lazily during a later instance start would kill containers already
        // launched by this stack operation.
        prepare_container_host_mounts(
            &ctx,
            &ordered,
            &planned_by_key,
            mount_sources_by_machine
                .get(ctx.bound_core_node.as_str())
                .cloned()
                .unwrap_or_default(),
            &participants,
        )
        .await?;

        // Step 8: Start instances in dependency order.
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
        .await
    }
    .await;

    if let Err(mut launch_result) = outcome {
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

    /// A parameter a mount path can reference and an instance can leave out.
    const DEFAULTED_OUTPUT_DIR: &str =
        r#"output_dir: { $type: "string", $default: "/var/lib/peppy_default" }"#;
    /// The same parameter with nothing to fall back to, so an instance that
    /// omits it cannot resolve its mount path.
    const REQUIRED_OUTPUT_DIR: &str = r#"output_dir: "string""#;

    /// A planned deployment of one container node, with the parameter schema,
    /// mount paths and instances a test cares about and defaults everywhere
    /// else.
    fn planned_container_deployment(
        node_name: &str,
        parameters: &str,
        mount_paths: &[&str],
        instances: &[(&str, Option<&str>, BTreeMap<String, AnyType>)],
    ) -> PlannedDeployment {
        let mounts = mount_paths
            .iter()
            .map(|path| format!("\"{path}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let config = config::node::NodeConfigParser::from_content(&format!(
            r#"{{
                peppy_schema: "node/v1",
                manifest: {{ name: "{node_name}", tag: "v1" }},
                execution: {{
                    language: "python",
                    container: {{ def_file: "apptainer.def", mount_paths: [{mounts}] }},
                    parameters: {{ {parameters} }},
                }},
                interfaces: {{}},
            }}"#
        ))
        .expect("parse node config");

        PlannedDeployment {
            deployment: Deployment {
                source: daemon_config::launcher::DeploymentSource {
                    name: node_name.to_owned(),
                    tag: "v1".to_owned(),
                },
                instances: instances
                    .iter()
                    .map(|(instance_id, core_node, arguments)| {
                        let mut instance = daemon_config::launcher::DeploymentInstance::empty(
                            config::runtime::Name::new(*instance_id).expect("valid instance id"),
                        );
                        instance.arguments = arguments.clone();
                        instance.core_node = core_node.map(str::to_owned);
                        instance
                    })
                    .collect(),
            },
            node_name: node_name.to_owned(),
            node_tag: "v1".to_owned(),
            config,
            config_sha256: String::new(),
            root_pin: test_root_pin(node_name),
            closure_pins: Vec::new(),
            pin_manifests: Vec::new(),
        }
    }

    /// A git-backed node pin, the portable shape. Shared with the `federated`
    /// child module so both sides of a dispatch test pin the same bytes.
    pub(super) fn test_root_pin(node_name: &str) -> daemon_config::repository::PinnedItem {
        use daemon_config::repository::{
            EntryOrigin, GitCommit, ItemName, ItemTag, ManifestFingerprint, PinKind, PinnedItem,
            RepoRelativePath,
        };
        PinnedItem {
            kind: PinKind::Node,
            name: ItemName::parse(node_name).expect("valid pin name"),
            tag: ItemTag::parse("v1").expect("valid pin tag"),
            sha256: ManifestFingerprint::parse(&"a".repeat(64)).expect("valid sha"),
            origin: EntryOrigin::Git {
                repo_url: "https://example.com/hub".to_owned(),
                repo_ref: Some("main".to_owned()),
                commit: GitCommit::parse(&"b".repeat(40)).expect("valid commit"),
                path: RepoRelativePath::parse(&format!("{node_name}/peppy.json5"))
                    .expect("valid path"),
            },
        }
    }

    fn placements_with(
        coordinator: &str,
        by_instance: &[(&str, &str)],
    ) -> daemon_config::launcher::Placements {
        daemon_config::launcher::Placements::new(
            daemon_config::core_node_name::CoreNodeName::new(coordinator).expect("valid name"),
            by_instance
                .iter()
                .map(|(instance, core_node)| {
                    (
                        (*instance).to_owned(),
                        daemon_config::core_node_name::CoreNodeName::new(*core_node)
                            .expect("valid name"),
                    )
                })
                .collect(),
        )
    }

    /// Each machine gets its own instances' sources and nobody else's. This is
    /// what a participant is handed to prepare, so a source landing on the
    /// wrong machine would make a directory there and still leave the binding
    /// machine without one.
    #[test]
    fn mount_sources_are_grouped_by_the_machine_that_binds_them() {
        let planned = vec![planned_container_deployment(
            "recorder",
            DEFAULTED_OUTPUT_DIR,
            &["/data/episodes:/episodes:rw"],
            &[
                ("robot_inst", None, BTreeMap::new()),
                ("cloud_inst", Some("cn-cloud"), BTreeMap::new()),
            ],
        )];
        let placements = placements_with("cn-robot", &[("cloud_inst", "cn-cloud")]);

        let by_machine = container_mount_sources_by_machine(&planned, &placements)
            .expect("every mount path resolves");

        assert_eq!(
            by_machine.get("cn-robot").map(Vec::as_slice),
            Some(["/data/episodes".to_owned()].as_slice())
        );
        assert_eq!(
            by_machine.get("cn-cloud").map(Vec::as_slice),
            Some(["/data/episodes".to_owned()].as_slice())
        );
    }

    /// A machine with nothing to bind is absent, not present-and-empty: the
    /// caller iterates this map to decide who has preparation to do.
    #[test]
    fn a_machine_running_no_container_bind_is_absent_from_the_grouping() {
        let planned = vec![
            planned_container_deployment(
                "recorder",
                DEFAULTED_OUTPUT_DIR,
                &["/data/episodes"],
                &[("cloud_inst", Some("cn-cloud"), BTreeMap::new())],
            ),
            planned_container_deployment(
                "camera",
                DEFAULTED_OUTPUT_DIR,
                &[],
                &[("robot_inst", None, BTreeMap::new())],
            ),
        ];
        let placements = placements_with("cn-robot", &[("cloud_inst", "cn-cloud")]);

        let by_machine = container_mount_sources_by_machine(&planned, &placements)
            .expect("every mount path resolves");

        assert_eq!(by_machine.len(), 1);
        assert!(by_machine.contains_key("cn-cloud"));
    }

    /// Two instances of one node on one machine binding the same path is one
    /// source, and the same path on two machines is one source each: the map
    /// dedupes per machine, not globally.
    #[test]
    fn a_repeated_source_is_listed_once_per_machine() {
        let mut arguments = BTreeMap::new();
        arguments.insert(
            "output_dir".to_owned(),
            AnyType::String("/data/shared".to_owned()),
        );
        let planned = vec![planned_container_deployment(
            "recorder",
            DEFAULTED_OUTPUT_DIR,
            &["${parameters:output_dir}:/out:rw"],
            &[
                ("first_inst", None, arguments.clone()),
                ("second_inst", None, arguments),
            ],
        )];

        let by_machine =
            container_mount_sources_by_machine(&planned, &placements_with("cn-robot", &[]))
                .expect("every mount path resolves");

        assert_eq!(
            by_machine.get("cn-robot").map(Vec::as_slice),
            Some(["/data/shared".to_owned()].as_slice())
        );
    }

    /// A parameter the instance never supplies falls back to the node's
    /// default, so the machine that runs it still knows what to prepare.
    #[test]
    fn a_defaulted_mount_parameter_resolves_to_the_nodes_default() {
        let planned = vec![planned_container_deployment(
            "recorder",
            DEFAULTED_OUTPUT_DIR,
            &["${parameters:output_dir}:/out:rw"],
            &[("robot_inst", None, BTreeMap::new())],
        )];

        let by_machine =
            container_mount_sources_by_machine(&planned, &placements_with("cn-robot", &[]))
                .expect("the parameter default resolves");

        assert_eq!(
            by_machine.get("cn-robot").map(Vec::as_slice),
            Some(["/var/lib/peppy_default".to_owned()].as_slice())
        );
    }

    /// An unresolvable mount path names the instance it belongs to. This runs
    /// before the launch turns destructive, so it is the operator's whole
    /// description of what went wrong.
    #[test]
    fn an_unresolvable_mount_path_names_its_instance() {
        let planned = vec![planned_container_deployment(
            "recorder",
            REQUIRED_OUTPUT_DIR,
            &["${parameters:output_dir}:/out:rw"],
            &[("robot_inst", None, BTreeMap::new())],
        )];

        let error = container_mount_sources_by_machine(&planned, &placements_with("cn-robot", &[]))
            .expect_err("an unknown parameter cannot resolve");
        assert!(error.contains("recorder:v1"), "got: {error}");
        assert!(error.contains("robot_inst"), "got: {error}");
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
        assert_eq!(
            containers::mount_spec_source(&resolved[0]),
            "/tmp/video_reconstruction"
        );
    }

    /// An instance's `env_vars` are added to the forwarded caller environment
    /// and win on a shared key, leaving exactly one entry per key so the spawn
    /// path has nothing to disambiguate.
    #[test]
    fn instance_env_vars_override_the_forwarded_caller_environment() {
        let forwarded = vec![
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("ESP32_DEVICE".to_string(), "/dev/from_caller".to_string()),
        ];
        let mut instance_env = BTreeMap::new();
        instance_env.insert("ESP32_DEVICE".to_string(), "/dev/ttyUSB0".to_string());
        instance_env.insert("BOARD_ID".to_string(), "3".to_string());

        let merged = instance_environment(&forwarded, &instance_env);

        assert_eq!(
            merged,
            vec![
                ("PATH".to_string(), "/usr/bin".to_string()),
                ("BOARD_ID".to_string(), "3".to_string()),
                ("ESP32_DEVICE".to_string(), "/dev/ttyUSB0".to_string()),
            ]
        );
    }

    /// An instance on a peer starts from no forwarded environment, so the
    /// env_vars its launcher file declares are the whole environment its run
    /// goal carries.
    #[test]
    fn a_peer_instance_carries_only_its_declared_env_vars() {
        let mut instance_env = BTreeMap::new();
        instance_env.insert("ESP32_DEVICE".to_string(), "/dev/ttyUSB0".to_string());

        assert_eq!(
            instance_environment(&[], &instance_env),
            vec![("ESP32_DEVICE".to_string(), "/dev/ttyUSB0".to_string())]
        );
    }

    /// An instance that declares nothing is started with the forwarded
    /// environment unchanged, in the order it arrived.
    #[test]
    fn instance_without_env_vars_keeps_the_forwarded_environment() {
        let forwarded = vec![
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("HOME".to_string(), "/home/user".to_string()),
        ];

        assert_eq!(
            instance_environment(&forwarded, &BTreeMap::new()),
            forwarded
        );
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
