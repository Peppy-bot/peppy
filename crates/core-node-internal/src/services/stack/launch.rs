mod feedback;
mod orchestrate;
mod phases;
mod resolve;

use self::feedback::{publish_stderr, publish_stdout};
use self::orchestrate::{
    add_node_directly, build_node_directly, fail_and_clear_stack, start_node_directly,
    teardown_and_reset_stack, validate_and_order_dependencies,
};
use self::resolve::{parse_launcher_config, resolve_deployments, resolve_framework};
use crate::Result;
use crate::services::action_loop::{GoalHandler, accept_goal, reject_goal, run_action_loop};
use crate::services::node::common::panic_message;
use crate::services::node::gate::{Admission, ConcurrencyGate};
use crate::services::node::pairing::PairingCoordinator;
use crate::services::node::{
    DaemonDefaults, create_action_log_file, resolve_mount_path_parameters,
};
use chrono::Local;
use config::apply_parameter_defaults;
use config::consts::{DEFAULT_MESSAGING_HOST, DEFAULT_MESSAGING_PORT};
use config::runtime::RuntimeConfig;
use core_node_api::ActionId;
use core_node_api::encoding::{
    LaunchFeedbackStep, LaunchGoal, LaunchGoalResponse, LaunchResult, NodeAddGoal, NodeAddLogEntry,
    NodeBuildLogEntry, NodeRunGoal, NodeRunLogEntry, NodeSource, PairTarget,
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
use std::collections::{HashMap, HashSet};
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
    pub use_sim_time: bool,
    /// Daemon-resolved defaults (messaging mode, peer buffers, liveness grace)
    /// injected into every launched node.
    pub daemon_defaults: DaemonDefaults,
    /// Daemon-shutdown signal, forwarded to each launched node's health monitor
    /// so it stops probing the instant a clean shutdown begins.
    pub shutdown_token: CancellationToken,
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
    pairing: Arc<PairingCoordinator>,
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
        use_sim_time: daemon_use_sim_time,
        daemon_defaults,
        shutdown_token,
    } = defaults;
    let handler = LaunchGoalHandler {
        context: LaunchActionContext {
            node_stack,
            messenger: messenger.clone(),
            bound_core_node: core_node_name.to_string(),
            core_instance_id: instance_id.to_string(),
            peppy_dirs,
            timeouts,
            daemon_use_sim_time,
            daemon_defaults,
            shutdown_token,
            pairing,
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
    /// Daemon-wide default for `framework.use_sim_time` applied to instances
    /// that omit the per-instance override.
    daemon_use_sim_time: bool,
    /// Daemon-resolved defaults (messaging mode, peer buffers, liveness grace)
    /// injected into every launched node.
    daemon_defaults: DaemonDefaults,
    /// Daemon-shutdown signal, forwarded to each launched node's health monitor.
    shutdown_token: CancellationToken,
    /// The daemon's single pairing authority, forwarded into each instance's
    /// `node_run` flow (the launcher's `pairings:` map rides the per-instance
    /// `NodeRunGoal`).
    pairing: Arc<PairingCoordinator>,
}

#[derive(Clone)]
struct LaunchActionContext {
    node_stack: Arc<NodeStack>,
    messenger: MessengerHandle,
    bound_core_node: String,
    core_instance_id: String,
    peppy_dirs: PeppyDirs,
    timeouts: StackLaunchTimeouts,
    daemon_use_sim_time: bool,
    daemon_defaults: DaemonDefaults,
    shutdown_token: CancellationToken,
    pairing: Arc<PairingCoordinator>,
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
}

/// Marker git_hash used for stack-launch operations.
/// When this marker is used, the node_add service skips git hash verification
/// and generates fresh peppygen files. This allows stack_launch to work with
/// local filesystem sources without requiring `peppy node sync` beforehand.
pub const STACK_LAUNCH_GIT_HASH: &str = "stack-launch";

/// Step 5: Add every node to the node stack in dependency order.
async fn add_nodes_to_stack(
    ctx: &ProcessLaunchContext,
    ordered: &[NodeKey],
    planned_by_key: &HashMap<NodeKey, PlannedDeployment>,
    add_log_paths: &mut Vec<NodeAddLogEntry>,
    build_log_paths: &mut Vec<NodeBuildLogEntry>,
) -> std::result::Result<(), LaunchResult> {
    publish_stdout(
        ctx,
        "Adding nodes to the stack...",
        LaunchFeedbackStep::LauncherStep,
    )
    .await;

    for key in ordered {
        let Some(item) = planned_by_key.get(key) else {
            continue;
        };

        publish_stdout(
            ctx,
            format!("Adding {}", key.label()),
            LaunchFeedbackStep::AddingNode,
        )
        .await;

        let node_add_goal =
            NodeAddGoal::for_internal_execution(item.source.clone(), STACK_LAUNCH_GIT_HASH)
                .with_env_vars(ctx.env_vars.clone());

        let (result, log_path) = add_node_directly(ctx, node_add_goal).await;

        let failed = result.as_ref().map(|r| !r.success).unwrap_or(true);
        if let Some(path) = log_path {
            add_log_paths.push(NodeAddLogEntry {
                node_label: key.label(),
                log_path: path,
                failed,
            });
        }

        match result {
            Ok(result) => {
                if !result.success {
                    let inner = result
                        .error_message
                        .unwrap_or_else(|| "node_add failed".to_string());
                    let reason = format!("failed to add node {}: {}", key.label(), inner);
                    return Err(fail_and_clear_stack(ctx, reason).await);
                }
                let node_name = result.node_name.clone().unwrap_or_else(|| key.name.clone());
                let node_tag = result.node_tag.clone().unwrap_or_else(|| key.tag.clone());

                // Stack launch chains directly from add into build, since the
                // launcher's contract is "the stack is up and running"; an
                // `Added` entity isn't actually buildable from the user's
                // perspective until `node build` has run.
                let (build_result, build_log_path) =
                    build_node_directly(ctx, node_name, node_tag, ctx.env_vars.clone()).await;

                let build_failed = build_result.is_err();
                if let Some(path) = build_log_path {
                    build_log_paths.push(NodeBuildLogEntry {
                        node_label: key.label(),
                        log_path: path,
                        failed: build_failed,
                    });
                }

                if let Err(err) = build_result {
                    let reason = format!("failed to build node {}: {}", key.label(), err);
                    return Err(fail_and_clear_stack(ctx, reason).await);
                }
            }
            Err(err) => {
                let reason = format!("failed to add node {}: {}", key.label(), err);
                return Err(fail_and_clear_stack(ctx, reason).await);
            }
        }
    }

    Ok(())
}

/// Step 6: Prepare all host paths that containers in this stack will bind.
async fn prepare_container_host_mounts(
    ctx: &ProcessLaunchContext,
    ordered: &[NodeKey],
    planned_by_key: &HashMap<NodeKey, PlannedDeployment>,
) -> std::result::Result<(), LaunchResult> {
    let mut mount_sources = match collect_container_mount_sources(ordered, planned_by_key) {
        Ok(paths) => paths,
        Err(reason) => return Err(fail_and_clear_stack(ctx, reason).await),
    };

    if let Err(reason) = ensure_launch_bind_sources(ctx, &mount_sources).await {
        return Err(fail_and_clear_stack(ctx, reason).await);
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
                return Err(fail_and_clear_stack(ctx, reason).await);
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
        return Err(fail_and_clear_stack(ctx, reason).await);
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
        if src_path.exists() || is_kernel_managed_mount_source(src_path) {
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
            !is_kernel_managed_mount_source(&src_path)
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

fn is_kernel_managed_mount_source(path: &Path) -> bool {
    path.starts_with("/dev") || path.starts_with("/proc") || path.starts_with("/sys")
}

/// Step 7: Start every instance in dependency order.
async fn start_node_instances(
    ctx: &ProcessLaunchContext,
    ordered: &[NodeKey],
    planned_by_key: &HashMap<NodeKey, PlannedDeployment>,
    run_log_paths: &mut Vec<NodeRunLogEntry>,
    resolved_slot_bindings: &std::collections::BTreeMap<String, config::runtime::SlotBindings>,
    planned_pairings: &[daemon_config::launcher::PlannedPairing],
) -> std::result::Result<(), LaunchResult> {
    publish_stdout(ctx, "Running nodes...", LaunchFeedbackStep::LauncherStep).await;

    // Each planned pair is established by the LATER-started endpoint's
    // `node_run` (instances start strictly sequentially in `ordered`, so at
    // that point the earlier endpoint is already Running and unpaired). The
    // later endpoint carries the fully-pinned pair request; the earlier
    // endpoint's slot rides `covered_pairs` — naming that future peer — so
    // its own coverage re-check passes and its feedback states the plan.
    // Only explicit `defer_pairings:` entries ride `deferred_pairs`.
    let mut start_index: HashMap<&str, usize> = HashMap::new();
    let mut requested_by_instance: HashMap<&str, std::collections::BTreeMap<String, PairTarget>> =
        HashMap::new();
    let mut covered_by_instance: HashMap<&str, std::collections::BTreeMap<String, PairTarget>> =
        HashMap::new();
    let mut deferred_by_instance: HashMap<&str, Vec<String>> = HashMap::new();
    for instance in ordered
        .iter()
        .filter_map(|key| planned_by_key.get(key))
        .flat_map(|item| &item.deployment.instances)
    {
        start_index.insert(instance.instance_id.as_str(), start_index.len());
        // Explicit `defer_pairings:` entries start unpaired on purpose.
        if !instance.defer_pairings.is_empty() {
            deferred_by_instance
                .entry(instance.instance_id.as_str())
                .or_default()
                .extend(instance.defer_pairings.iter().cloned());
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
                PairTarget::pinned(earlier.instance_id.clone(), earlier.link_id.clone()),
            );
        covered_by_instance
            .entry(earlier.instance_id.as_str())
            .or_default()
            .insert(
                earlier.link_id.clone(),
                PairTarget::pinned(later.instance_id.clone(), later.link_id.clone()),
            );
    }

    // Compute runtime config host/port.
    let (messaging_host, messaging_port) = ctx
        .messenger
        .messaging_endpoint()
        .await
        .unwrap_or((DEFAULT_MESSAGING_HOST.to_string(), DEFAULT_MESSAGING_PORT));

    for key in ordered {
        let Some(item) = planned_by_key.get(key) else {
            continue;
        };

        for instance in &item.deployment.instances {
            let instance_id = instance.instance_id.as_str();
            publish_stdout(
                ctx,
                format!("Starting {} instance {}", key.label(), instance_id),
                LaunchFeedbackStep::RunningNode,
            )
            .await;

            let slot_bindings = resolved_slot_bindings
                .get(instance.instance_id.as_str())
                .cloned()
                .unwrap_or_default();
            let node_instance = config::runtime::NodeInstanceConfig {
                arguments: instance.arguments.clone(),
                framework: resolve_framework(&instance.framework, ctx.daemon_use_sim_time),
                slot_bindings,
                ..config::runtime::NodeInstanceConfig::new(instance.instance_id.clone())
            };
            let runtime_config = match RuntimeConfig::new(
                messaging_host.as_str(),
                messaging_port,
                node_instance,
                item.node_name.as_str(),
                item.node_tag.as_str(),
                ctx.bound_core_node.as_str(),
            ) {
                Ok(cfg) => cfg,
                Err(e) => {
                    return Err(fail_and_clear_stack(ctx, e.to_string()).await);
                }
            };

            let runtime_config_json5 = match serde_json5::to_string(&runtime_config) {
                Ok(json) => json,
                Err(e) => {
                    return Err(fail_and_clear_stack(
                        ctx,
                        format!("failed to serialize runtime config: {e}"),
                    )
                    .await);
                }
            };

            let node_run_goal = NodeRunGoal::for_internal_execution(
                &runtime_config_json5,
                item.node_name.as_str(),
                item.node_tag.as_str(),
            )
            .with_env_vars(ctx.env_vars.clone())
            .with_requested_pairs(
                requested_by_instance
                    .remove(instance_id)
                    .unwrap_or_default(),
            )
            .with_deferred_pairs(deferred_by_instance.remove(instance_id).unwrap_or_default())
            .with_covered_pairs(covered_by_instance.remove(instance_id).unwrap_or_default());

            // Create log file for this node start
            let log_dir = ctx.peppy_dirs.logs_dir_run();
            let log_filename = format!("{}.log", instance_id);
            let (log_file, log_path) = match create_action_log_file(&log_dir, &log_filename) {
                Ok(r) => r,
                Err(e) => {
                    return Err(fail_and_clear_stack(ctx, e).await);
                }
            };

            let (result, log_path) =
                start_node_directly(ctx, node_run_goal, runtime_config, log_path, log_file).await;

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
                Ok(result) => {
                    if !result.success {
                        let inner = result
                            .error_message
                            .unwrap_or_else(|| "node_run failed".to_string());
                        let reason = format!(
                            "failed to start node {} instance {}: {}",
                            key.label(),
                            instance_id,
                            inner
                        );
                        return Err(fail_and_clear_stack(ctx, reason).await);
                    }
                }
                Err(err) => {
                    let reason = format!(
                        "failed to start node {} instance {}: {}",
                        key.label(),
                        instance_id,
                        err
                    );
                    return Err(fail_and_clear_stack(ctx, reason).await);
                }
            }
        }
    }

    Ok(())
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
            daemon_use_sim_time,
            daemon_defaults,
            shutdown_token,
            pairing,
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
            daemon_use_sim_time,
            daemon_defaults,
            shutdown_token,
            pairing,
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
    // Step 1: Parse launcher configuration
    let (deployments, nodes_directory) = match parse_launcher_config(&ctx, &goal).await {
        Ok(result) => result,
        Err(launch_result) => return launch_result,
    };

    // Step 2: Resolve deployments
    let planned = match resolve_deployments(&ctx, deployments, &nodes_directory).await {
        Ok(result) => result,
        Err(launch_result) => return launch_result,
    };

    // Step 3: Validate dependencies and compute topological order
    let root_config = ctx.node_stack.root().read().config().clone();
    let (ordered, resolved_slot_bindings, planned_pairings) =
        match validate_and_order_dependencies(&ctx, &planned, &root_config).await {
            Ok(result) => result,
            Err(launch_result) => return launch_result,
        };

    // Step 4: Stop and clear the currently-running stack. A launch replaces it,
    // so the old instances are torn down here before the new ones are built.
    teardown_and_reset_stack(&ctx).await;

    // Build lookup map
    let planned_by_key: HashMap<NodeKey, PlannedDeployment> = planned
        .into_iter()
        .map(|item| (NodeKey::new(&item.node_name, &item.node_tag), item))
        .collect();

    let mut add_log_paths: Vec<NodeAddLogEntry> = Vec::new();
    let mut build_log_paths: Vec<NodeBuildLogEntry> = Vec::new();
    let mut run_log_paths: Vec<NodeRunLogEntry> = Vec::new();

    // Step 5: Add nodes in dependency order
    let add_result = add_nodes_to_stack(
        &ctx,
        &ordered,
        &planned_by_key,
        &mut add_log_paths,
        &mut build_log_paths,
    )
    .await;

    // Step 6: Prepare any Lima host mounts before the first container starts.
    // Updating Lima's mount table can restart the VM; doing it lazily during
    // a later instance start would kill containers already launched by this
    // stack operation.
    let mount_result = if add_result.is_ok() {
        Some(prepare_container_host_mounts(&ctx, &ordered, &planned_by_key).await)
    } else {
        None
    };

    // Step 7: Start instances in dependency order (only if add and mount
    // preparation succeeded)
    let start_result = if add_result.is_ok() && mount_result.as_ref().is_none_or(|r| r.is_ok()) {
        Some(
            start_node_instances(
                &ctx,
                &ordered,
                &planned_by_key,
                &mut run_log_paths,
                &resolved_slot_bindings,
                &planned_pairings,
            )
            .await,
        )
    } else {
        None
    };

    if let Err(mut launch_result) = add_result {
        launch_result.node_add_logs = add_log_paths;
        launch_result.node_build_logs = build_log_paths;
        return launch_result;
    }
    if let Some(Err(mut launch_result)) = mount_result {
        launch_result.node_add_logs = add_log_paths;
        launch_result.node_build_logs = build_log_paths;
        return launch_result;
    }
    if let Some(Err(mut launch_result)) = start_result {
        launch_result.node_add_logs = add_log_paths;
        launch_result.node_build_logs = build_log_paths;
        launch_result.node_run_logs = run_log_paths;
        return launch_result;
    }

    publish_stdout(&ctx, "Launch complete", LaunchFeedbackStep::LauncherStep).await;
    LaunchResult::success(&ctx.log_path).with_node_logs(
        add_log_paths,
        build_log_paths,
        run_log_paths,
    )
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

    /// Per-instance override beats the daemon default in either direction.
    /// `Some(true)` forces sim even when the daemon default is wall;
    /// `Some(false)` forces wall even when the daemon default is sim.
    #[test]
    fn resolve_framework_per_instance_wins() {
        let force_sim = daemon_config::launcher::FrameworkOverrides {
            use_sim_time: Some(true),
        };
        assert!(resolve_framework(&force_sim, false).use_sim_time);

        let force_wall = daemon_config::launcher::FrameworkOverrides {
            use_sim_time: Some(false),
        };
        assert!(!resolve_framework(&force_wall, true).use_sim_time);
    }

    /// When the instance omits the override, the daemon default decides.
    #[test]
    fn resolve_framework_falls_through_to_daemon_default() {
        let none = daemon_config::launcher::FrameworkOverrides::default();
        assert!(!resolve_framework(&none, false).use_sim_time);
        assert!(resolve_framework(&none, true).use_sim_time);
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
