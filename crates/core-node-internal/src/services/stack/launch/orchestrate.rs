use super::feedback::{publish_stderr, publish_stdout, spawn_feedback_forwarder};
use super::phases::run_phase;
use super::{NodeKey, PlannedDeployment, ProcessLaunchContext};
use crate::services::node::{
    NodeAddActionContext, NodeBuildActionContext, NodeRunActionContext, create_action_log_file,
    log_label_from_source, run_node_add, run_node_build_for_entity, run_node_run,
    teardown_all_instances,
};
use chrono::Local;
use config::runtime::RuntimeConfig;
use core_node_api::encoding::{
    LaunchFeedbackStep, LaunchResult, NodeAddGoal, NodeAddResult, NodeRunGoal, NodeRunResult,
};
use parking_lot::Mutex as StdMutex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

pub(super) async fn add_node_directly(
    ctx: &ProcessLaunchContext,
    node_add_goal: NodeAddGoal,
) -> (std::result::Result<NodeAddResult, String>, Option<PathBuf>) {
    // Create log file before source resolution so clone/download output is captured.
    let log_label = log_label_from_source(&node_add_goal.source);
    let log_dir = ctx.peppy_dirs.logs_dir_add();
    let timestamp = Local::now().format("%Y%m%d_%H%M%S_%3f").to_string();
    let log_filename = format!("{}_{}.log", log_label, timestamp);
    let (log_file, log_path) = match create_action_log_file(&log_dir, &log_filename) {
        Ok(r) => r,
        Err(e) => return (Err(e), None),
    };

    let activity_notify = Arc::new(Notify::new());
    let (feedback_tx, forwarder_handle) = spawn_feedback_forwarder(
        &ctx.feedback_publisher,
        LaunchFeedbackStep::AddingNode,
        &ctx.log_file,
        Some(Arc::clone(&activity_notify)),
    );

    let action_context = NodeAddActionContext {
        node_stack: Arc::clone(&ctx.node_stack),
        messenger: ctx.messenger.clone(),
        bound_core_node: ctx.bound_core_node.clone(),
        core_instance_id: ctx.core_instance_id.clone(),
        peppy_dirs: ctx.peppy_dirs.clone(),
        pairing: Arc::clone(&ctx.pairing),
        observation: Arc::clone(&ctx.observation),
    };

    let log_file_for_timeout = log_file.clone();
    let log_path_for_timeout = log_path.clone();

    let result = run_phase(
        run_node_add(
            node_add_goal,
            action_context,
            feedback_tx,
            log_file,
            log_path,
            timestamp,
        ),
        activity_notify,
        ctx.idle_timeouts.add,
        ctx.launch_deadline,
        &log_file_for_timeout,
        LaunchFeedbackStep::AddingNode,
        |reason| NodeAddResult::failure(&log_path_for_timeout, reason),
        None,
    )
    .await;

    // Wait for feedback forwarder to drain.
    let _ = forwarder_handle.await;

    let final_log_path = Some(result.log_path.clone());
    if result.success {
        (Ok(result), final_log_path)
    } else {
        let err = result
            .error_message
            .clone()
            .unwrap_or_else(|| "node_add failed".to_string());
        (Err(err), final_log_path)
    }
}

pub(super) async fn build_node_directly(
    ctx: &ProcessLaunchContext,
    node_name: String,
    node_tag: String,
    env_vars: Vec<(String, String)>,
) -> (std::result::Result<(), String>, Option<PathBuf>) {
    let log_dir = ctx.peppy_dirs.logs_dir_build();
    let timestamp = Local::now().format("%Y%m%d_%H%M%S_%3f").to_string();
    let log_filename = format!("{}_{}_{}.log", node_name, node_tag, timestamp);
    let (log_file, log_path) = match create_action_log_file(&log_dir, &log_filename) {
        Ok(pair) => pair,
        Err(e) => return (Err(e.to_string()), None),
    };

    let final_log_path = log_path.clone();

    let activity_notify = Arc::new(Notify::new());
    let (feedback_tx, forwarder_handle) = spawn_feedback_forwarder(
        &ctx.feedback_publisher,
        LaunchFeedbackStep::BuildingNode,
        &ctx.log_file,
        Some(Arc::clone(&activity_notify)),
    );

    let action_context = NodeBuildActionContext {
        node_stack: Arc::clone(&ctx.node_stack),
        peppy_dirs: ctx.peppy_dirs.clone(),
    };

    let log_file_for_timeout = log_file.clone();
    let log_path_for_timeout = log_path.clone();

    let result = run_phase(
        run_node_build_for_entity(
            node_name.clone(),
            node_tag.clone(),
            env_vars,
            action_context,
            feedback_tx,
            log_file,
            log_path,
        ),
        activity_notify,
        ctx.idle_timeouts.build,
        ctx.launch_deadline,
        &log_file_for_timeout,
        LaunchFeedbackStep::BuildingNode,
        |reason| core_node_api::encoding::NodeBuildResult::failure(&log_path_for_timeout, reason),
        None,
    )
    .await;

    let _ = forwarder_handle.await;

    if result.success {
        (Ok(()), Some(final_log_path))
    } else {
        (
            Err(result
                .error_message
                .unwrap_or_else(|| "node_build failed".to_string())),
            Some(final_log_path),
        )
    }
}

pub(super) async fn start_node_directly(
    ctx: &ProcessLaunchContext,
    node_run_goal: NodeRunGoal,
    runtime_config: RuntimeConfig,
    log_path: PathBuf,
    log_file: Arc<StdMutex<File>>,
) -> (std::result::Result<NodeRunResult, String>, Option<PathBuf>) {
    let activity_notify = Arc::new(Notify::new());
    let (feedback_tx, _forwarder_handle) = spawn_feedback_forwarder(
        &ctx.feedback_publisher,
        LaunchFeedbackStep::RunningNode,
        &ctx.log_file,
        Some(Arc::clone(&activity_notify)),
    );

    let action_context = NodeRunActionContext {
        node_stack: Arc::clone(&ctx.node_stack),
        messenger: ctx.messenger.clone(),
        core_node_name: ctx.bound_core_node.clone(),
        caller_instance_id: ctx.core_instance_id.clone(),
        node_startup_timeout: ctx.timeouts.node_startup,
        node_start_health_timeout: ctx.timeouts.node_start_health,
        peppy_dirs: ctx.peppy_dirs.clone(),
        health_monitor_interval: ctx.timeouts.health_monitor_interval,
        health_monitor_timeout: ctx.timeouts.health_monitor_timeout,
        daemon_defaults: ctx.daemon_defaults.clone(),
        shutdown_token: ctx.shutdown_token.clone(),
        pairing: Arc::clone(&ctx.pairing),
        observation: Arc::clone(&ctx.observation),
    };

    let log_file_for_timeout = log_file.clone();

    // Token triggered by `run_phase_with_timeouts` on idle/max timeout; observed
    // inside `run_node_run` to abort a half-spawned node instance (SIGKILL the
    // child + unregister its `Starting` entry) before we return the failure.
    let run_cancel_token = CancellationToken::new();

    let result = run_phase(
        run_node_run(
            node_run_goal,
            runtime_config,
            action_context,
            feedback_tx,
            log_file,
            ctx.core_instance_id.clone(),
            run_cancel_token.clone(),
        ),
        activity_notify,
        ctx.idle_timeouts.run,
        ctx.launch_deadline,
        &log_file_for_timeout,
        LaunchFeedbackStep::RunningNode,
        NodeRunResult::failure,
        Some(run_cancel_token),
    )
    .await;

    // Don't await _forwarder_handle: the node process is still running and
    // output readers keep the internal channel alive.

    let node_log_path = Some(log_path);
    if result.success {
        (Ok(result), node_log_path)
    } else {
        let err = result
            .error_message
            .clone()
            .unwrap_or_else(|| "node_run failed".to_string());
        (Err(err), node_log_path)
    }
}

/// Launch failure path: tear down whatever partial stack got started and clear
/// it, then return the failure. A launch replaces the previous stack by tearing
/// it down at the clear step, so on failure there is nothing to roll back to;
/// the honest end state is an empty stack rather than orphaned half-started
/// instances.
pub(super) async fn fail_and_clear_stack(
    ctx: &ProcessLaunchContext,
    reason: String,
) -> LaunchResult {
    publish_stderr(
        ctx,
        format!("Launch failed: {reason}"),
        LaunchFeedbackStep::LauncherStep,
    )
    .await;

    teardown_and_reset_stack(ctx).await;

    LaunchResult::failure(&ctx.log_path, reason)
}

/// Output of [`validate_and_order_dependencies`]: a topological order
/// of deployments to spawn, plus the resolved per-instance slot
/// producers produced by the launcher's binding validator. The map is
/// keyed by `consumer_instance_id`; each inner map is keyed by the
/// consumer's manifest `link_id` and carries the slot's bound producers
/// (validation guarantees every declared slot is bound, so an inner
/// entry is never empty).
type ResolvedSlotBindings = std::collections::BTreeMap<String, config::runtime::SlotBindings>;

/// Step 3: Validate dependencies and compute a stable topological order.
pub(super) async fn validate_and_order_dependencies(
    ctx: &ProcessLaunchContext,
    planned: &[PlannedDeployment],
    root_config: &config::node::NodeConfig,
) -> std::result::Result<
    (
        Vec<NodeKey>,
        ResolvedSlotBindings,
        Vec<daemon_config::launcher::PlannedPairing>,
        Vec<daemon_config::launcher::PlannedObservation>,
    ),
    LaunchResult,
> {
    publish_stdout(
        ctx,
        "Validating dependencies",
        LaunchFeedbackStep::LauncherStep,
    )
    .await;

    let root_key = NodeKey::new(
        root_config.manifest.name.as_str(),
        root_config.manifest.tag.as_str(),
    );

    let mut configs_by_key: HashMap<NodeKey, config::node::NodeConfig> = HashMap::new();
    configs_by_key.insert(root_key.clone(), root_config.clone());
    for item in planned {
        configs_by_key.insert(
            NodeKey::new(&item.node_name, &item.node_tag),
            item.config.clone(),
        );
    }

    let planned_keys: HashSet<NodeKey> = planned
        .iter()
        .map(|p| NodeKey::new(&p.node_name, &p.node_tag))
        .collect();

    // Validate all dependencies exist and expose the required interfaces.
    let dependency_errors: Vec<String> = planned
        .iter()
        .flat_map(|item| {
            config::node::validate_dependency_specs(
                &item.config.manifest,
                &item.config.interfaces,
                &item.node_name,
                &item.node_tag,
                |name, tag| configs_by_key.get(&NodeKey::new(name, tag)).cloned(),
            )
        })
        .map(|e| e.to_string())
        .collect();

    if !dependency_errors.is_empty() {
        let msg = daemon_config::format_bulleted(&dependency_errors);
        publish_stderr(ctx, msg.clone(), LaunchFeedbackStep::LauncherStep).await;
        return Err(LaunchResult::failure(&ctx.log_path, msg));
    }

    // The root entity stays in the stack across launches (teardown_and_reset_stack
    // preserves it), so its instance_id must participate in stack-wide uniqueness
    // checks. Synthesize a single-instance DeploymentInstance for it, but pass
    // `depends_on: None` in the binding item below so the per-instance binding
    // rules treat the root as inert (no declared slots, so the every-slot-bound
    // rule never fires on it) and only
    // check_stack_wide_instance_id_uniqueness (which reads name/tag/instance_id)
    // acts on it. Forwarding the root's real depends_on would pit rule 5 against
    // the synthesized instance's empty bindings and reject a launch whose root
    // already resolved its slots at its own spawn. The root's real `implements`
    // IS forwarded: it only says which contract slots of other nodes the root
    // can satisfy, so it never makes the root itself less inert.
    let root_instance_id_str = ctx
        .node_stack
        .root()
        .read()
        .instances()
        .first()
        .map(|inst| inst.instance_id().as_str().to_owned());
    let root_instances: Vec<daemon_config::launcher::DeploymentInstance> = root_instance_id_str
        .and_then(|id_str| config::runtime::Name::new(id_str).ok())
        .map(|instance_id| daemon_config::launcher::DeploymentInstance {
            instance_id,
            arguments: Default::default(),
            env_vars: Default::default(),
            framework: Default::default(),
            links: Default::default(),
            defer_links: Default::default(),
        })
        .into_iter()
        .collect();

    let mut binding_items: Vec<daemon_config::launcher::BindingValidationItem<'_>> = planned
        .iter()
        .map(|p| daemon_config::launcher::BindingValidationItem {
            node_name: &p.node_name,
            node_tag: &p.node_tag,
            instances: &p.deployment.instances,
            depends_on: p.config.manifest.depends_on.as_ref(),
            implements: &p.config.manifest.implements,
        })
        .collect();
    if !root_instances.is_empty() {
        binding_items.push(daemon_config::launcher::BindingValidationItem {
            node_name: root_config.manifest.name.as_str(),
            node_tag: root_config.manifest.tag.as_str(),
            instances: &root_instances,
            depends_on: None,
            implements: &root_config.manifest.implements,
        });
    }
    // Cross-family check first: every `links` key must name a declared slot
    // in some family, and every `defer_links` entry must be a deferrable
    // (pairing/observer) slot. This is the single pass that sees all slot
    // kinds, so it owns unknown-key and structural-defer reporting; the
    // per-mechanism validators below skip keys that are not theirs.
    let link_slot_errors = daemon_config::launcher::validate_link_slots(&binding_items);
    if !link_slot_errors.is_empty() {
        let errors: Vec<String> = link_slot_errors.iter().map(|e| e.to_string()).collect();
        let msg = daemon_config::format_bulleted(&errors);
        publish_stderr(ctx, msg.clone(), LaunchFeedbackStep::LauncherStep).await;
        return Err(LaunchResult::failure(&ctx.log_path, msg));
    }

    // Stamp every resolved producer reference with this daemon's core_node:
    // stacks are daemon-scoped, so the launching daemon is where every
    // producer instance in the snapshot lives.
    let validated =
        daemon_config::launcher::validate_bindings(&binding_items, ctx.bound_core_node.as_str());
    if !validated.errors.is_empty() {
        let msg = daemon_config::format_bulleted(&validated.errors);
        publish_stderr(ctx, msg.clone(), LaunchFeedbackStep::LauncherStep).await;
        return Err(LaunchResult::failure(&ctx.log_path, msg));
    }
    let resolved_slot_bindings = validated.slot_bindings;

    // Pairing plan: every participant-slot `links:` entry (and `defer_links:`)
    // validated against the declared slots, coverage of required slots
    // enforced, each target resolved to one concrete peer slot. Observer plan:
    // every observer-slot `links:` entry resolved to its source participant
    // slot. Both read the same per-node `depends_on.pairings`, so they share
    // one item set. A launch replaces the previous stack (torn down at the
    // clear step), so there are no preexisting instances or already-claimed
    // slots to fold in.
    let pairing_items: Vec<daemon_config::launcher::PairingValidationItem<'_>> = planned
        .iter()
        .map(|p| daemon_config::launcher::PairingValidationItem {
            node_name: &p.node_name,
            node_tag: &p.node_tag,
            instances: &p.deployment.instances,
            pairing_deps: p
                .config
                .manifest
                .depends_on
                .as_ref()
                .map(|d| d.pairings.as_slice())
                .unwrap_or_default(),
            preexisting: false,
        })
        .collect();
    let validated_pairings = daemon_config::launcher::validate_pairings(
        &pairing_items,
        &daemon_config::launcher::AlreadyPairedSlots::new(),
    );
    let validated_observations = daemon_config::launcher::validate_observations(
        &pairing_items,
        ctx.bound_core_node.as_str(),
    );
    let pairing_errors: Vec<String> = validated_pairings
        .errors
        .iter()
        .chain(validated_observations.errors.iter())
        .map(|e| e.to_string())
        .collect();
    if !pairing_errors.is_empty() {
        let msg = daemon_config::format_bulleted(&pairing_errors);
        publish_stderr(ctx, msg.clone(), LaunchFeedbackStep::LauncherStep).await;
        return Err(LaunchResult::failure(&ctx.log_path, msg));
    }
    let planned_pairings = validated_pairings.planned;
    let planned_observations = validated_observations.planned;

    // Build the dependency graph for topological ordering.
    let mut deps_for: HashMap<NodeKey, HashSet<NodeKey>> = HashMap::new();
    for item in planned {
        let dependant_key = NodeKey::new(&item.node_name, &item.node_tag);
        let mut deps = HashSet::new();
        for spec in config::node::collect_dependency_specs(&item.config) {
            let dep_key = NodeKey::new(&spec.node_name, &spec.node_tag);
            if dep_key != root_key && planned_keys.contains(&dep_key) {
                deps.insert(dep_key);
            }
        }
        deps_for.insert(dependant_key, deps);
    }

    // Stable topological sort using original plan order as tie-breaker.
    let ordered = topological_sort(planned, &deps_for, &ctx.log_path).map_err(|e| *e)?;

    publish_stdout(
        ctx,
        format!(
            "Dependency order: {}",
            ordered
                .iter()
                .map(|k| k.label())
                .collect::<Vec<_>>()
                .join(" -> ")
        ),
        LaunchFeedbackStep::LauncherStep,
    )
    .await;

    Ok((
        ordered,
        resolved_slot_bindings,
        planned_pairings,
        planned_observations,
    ))
}

/// Perform a stable topological sort.
fn topological_sort(
    planned: &[PlannedDeployment],
    deps_for: &HashMap<NodeKey, HashSet<NodeKey>>,
    log_path: &PathBuf,
) -> std::result::Result<Vec<NodeKey>, Box<LaunchResult>> {
    let mut in_degree: HashMap<NodeKey, usize> = HashMap::new();
    let mut dependents: HashMap<NodeKey, Vec<NodeKey>> = HashMap::new();

    for key in planned
        .iter()
        .map(|p| NodeKey::new(&p.node_name, &p.node_tag))
    {
        in_degree.entry(key.clone()).or_insert(0);
        dependents.entry(key).or_default();
    }

    for (dependant, deps) in deps_for {
        in_degree.insert(dependant.clone(), deps.len());
        for dep in deps {
            dependents
                .entry(dep.clone())
                .or_default()
                .push(dependant.clone());
        }
    }

    let order_index: HashMap<NodeKey, usize> = planned
        .iter()
        .enumerate()
        .map(|(idx, p)| (NodeKey::new(&p.node_name, &p.node_tag), idx))
        .collect();

    let mut ready: Vec<NodeKey> = in_degree
        .iter()
        .filter(|(_, deg)| **deg == 0)
        .map(|(k, _)| k.clone())
        .collect();
    ready.sort_by_key(|k| order_index.get(k).copied().unwrap_or(usize::MAX));

    let mut queue: VecDeque<NodeKey> = ready.into();
    let mut ordered: Vec<NodeKey> = Vec::new();

    while let Some(node) = queue.pop_front() {
        ordered.push(node.clone());
        let Some(children) = dependents.get(&node) else {
            continue;
        };
        for child in children {
            if let Some(deg) = in_degree.get_mut(child) {
                *deg = deg.saturating_sub(1);
                if *deg == 0 {
                    queue.push_back(child.clone());
                }
            }
        }
        // Keep stable ordering when multiple nodes become ready at once.
        let mut drained: Vec<NodeKey> = queue.drain(..).collect();
        drained.sort_by_key(|k| order_index.get(k).copied().unwrap_or(usize::MAX));
        queue = drained.into();
    }

    if ordered.len() != planned.len() {
        let mut remaining: Vec<String> = in_degree
            .into_iter()
            .filter(|(_, deg)| *deg > 0)
            .map(|(k, _)| k.label())
            .collect();
        remaining.sort();
        let msg = format!(
            "unable to resolve dependency order (cycle suspected). Remaining nodes: {}",
            remaining.join(", ")
        );
        return Err(Box::new(LaunchResult::failure(log_path, msg)));
    }

    Ok(ordered)
}

/// Step 4: Stop the currently-running stack and clear it.
///
/// Cooperatively shuts down every running instance, force-killing the process
/// group of any straggler, before dropping them from the stack, so a relaunch
/// never orphans the previous stack's processes. Also reused by the launch
/// failure path to tear down a partial new stack. Infallible.
pub(super) async fn teardown_and_reset_stack(ctx: &ProcessLaunchContext) {
    publish_stdout(
        ctx,
        "Stopping current node stack",
        LaunchFeedbackStep::LauncherStep,
    )
    .await;
    teardown_all_instances(
        &ctx.messenger,
        &ctx.bound_core_node,
        &ctx.core_instance_id,
        &ctx.node_stack,
    )
    .await;
    ctx.node_stack.reset();
}
