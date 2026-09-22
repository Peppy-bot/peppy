//! The start phase of a stack change: every planned instance spawned in
//! dependency order, with the pairs, observations and watchers its plan gave it.

use super::feedback::publish_stdout;
use super::orchestrate::start_node_directly;
use super::watchers::LifecycleWatchers;
use super::{NodeKey, PhaseChange, PhaseGoal, PlannedDeployment, federated};
use crate::services::node::create_action_log_file;
use crate::services::stack::action::StackChangeContext;
use core_node_api::encoding::{
    InstanceEndpoints, LaunchFeedbackStep, NodeRunGoal, NodeRunLogEntry, NodeRunResult,
    ObservationTarget, ObservationTargets, PairTarget, RemotePeerPairing,
};
use std::collections::{BTreeMap, HashMap};

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
    instance_env: &BTreeMap<String, String>,
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

/// Starts instances in dependency order, awaiting Running before each next start.
#[allow(clippy::too_many_arguments)] // Distinct inputs; bundling them would only move the list.
pub(in crate::services::stack) async fn start_node_instances(
    ctx: &StackChangeContext,
    phase: &PhaseGoal,
    change: PhaseChange<'_>,
    ordered: &[NodeKey],
    planned_by_key: &HashMap<NodeKey, PlannedDeployment>,
    run_log_paths: &mut Vec<NodeRunLogEntry>,
    // The endpoints of every started instance that serves one, appended in
    // start order beside its run log entry.
    instance_endpoints: &mut Vec<InstanceEndpoints>,
    resolved_slot_bindings: &BTreeMap<String, config::runtime::SlotBindings>,
    planned_pairings: &[daemon_config::launcher::PlannedPairing],
    planned_observations: &[daemon_config::launcher::PlannedObservation],
    // Where each started source's lifecycle transitions go: every machine
    // running an observer of it anywhere in the plan, including observers
    // this change leaves running.
    watchers: &LifecycleWatchers,
    placements: &daemon_config::launcher::Placements,
    // The clock every instance of the change reads, as it was resolved: the
    // plan each one starts with carries its own.
    clocks: &daemon_config::launcher::ResolvedClocks,
) -> std::result::Result<(), String> {
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
    if change.appends() {
        ctx.relationships
            .observation()
            .extend_planned(&local_observations);
    } else {
        ctx.relationships
            .observation()
            .register_planned(&local_observations);
    }

    // Accumulated per observer slot, in plan order: a slot with N members
    // contributes N entries to one list, and that list becomes the slot's
    // `ObservationTargets` below.
    let mut observations_by_instance: HashMap<&str, BTreeMap<String, Vec<ObservationTarget>>> =
        HashMap::new();
    let mut watchers_by_source: HashMap<_, _> = watchers
        .iter()
        .map(|(instance, hosts)| {
            (
                instance.to_string(),
                hosts.iter().map(|host| host.to_string()).collect(),
            )
        })
        .collect();

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
            .push(observation.target());
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
    let mut requested_by_instance: HashMap<&str, BTreeMap<String, Vec<PairTarget>>> =
        HashMap::new();
    let mut covered_by_instance: HashMap<&str, BTreeMap<String, Vec<PairTarget>>> = HashMap::new();
    let mut vacant_by_instance: HashMap<&str, BTreeMap<String, String>> = HashMap::new();
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
                    peer_cardinality: peer.cardinality,
                    peer_copy: change.copy_of(peer.instance_id.as_str()),
                })
            };

        requested_by_instance
            .entry(later.instance_id.as_str())
            .or_default()
            .entry(later.link_id.clone())
            .or_default()
            .push(pair_target(later, earlier));
        covered_by_instance
            .entry(earlier.instance_id.as_str())
            .or_default()
            .entry(earlier.link_id.clone())
            .or_default()
            .push(pair_target(earlier, later));
    }

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
                copy: change.copy_of(instance.instance_id.as_str()),
                arguments: instance.arguments.clone(),
                clock: clocks.binding_for(instance_id),
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
            // Every instance of a launch carries its launch id wherever it
            // runs: the daemon spawning it admits it as part of that launch,
            // and records that launch as the owner of a domain it publishes.
            .with_launch_id(&phase.launch_id)
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
                    &core_node,
                    key,
                    instance_id,
                    node_run_goal,
                    &item.config_sha256,
                    run_log_paths,
                )
                .await
            };

            let result = match outcome {
                Ok(result) => result,
                Err(reason) => {
                    return Err(format!(
                        "failed to start node {} instance {instance_id} on `{core_node}`: {reason}",
                        key.label()
                    ));
                }
            };
            // The daemon hosting the instance expanded the URLs against its
            // own interfaces; the coordinator only forwards them with that
            // daemon's name.
            if !result.endpoints.is_empty() {
                instance_endpoints.push(InstanceEndpoints {
                    instance_id: instance_id.to_string(),
                    node_label: key.label(),
                    core_node: core_node.clone(),
                    endpoints: result.endpoints,
                });
            }
        }
    }

    Ok(())
}

/// Starts one instance on this daemon, in process, recording its log entry
/// and answering with what the start produced.
async fn start_locally(
    ctx: &StackChangeContext,
    key: &NodeKey,
    instance_id: &str,
    node_run_goal: NodeRunGoal,
    run_log_paths: &mut Vec<NodeRunLogEntry>,
) -> std::result::Result<NodeRunResult, String> {
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
        Ok(result) if result.success => Ok(result),
        Ok(result) => Err(result
            .error_message
            .unwrap_or_else(|| "node_run failed".to_string())),
        Err(err) => Err(err),
    }
}

/// Starts one instance on a peer, pinning the manifest this coordinator
/// resolved for its deployment, recording its log entry stamped with the
/// peer's core node, and answering with what the peer's start produced.
///
/// The hash closes the window between add and start: the peer compares it
/// against the entity now in its stack and refuses if the two no longer
/// hash the same, so an entity replaced out from under the plan fails
/// loudly instead of starting a node the plan was never checked against.
/// Every remote instance carries it, straddling deployments included,
/// because the coordinator resolved every deployment itself.
#[allow(clippy::too_many_arguments)] // Distinct inputs; bundling them would only move the list.
async fn start_remotely(
    ctx: &StackChangeContext,
    core_node: &str,
    key: &NodeKey,
    instance_id: &str,
    node_run_goal: NodeRunGoal,
    config_sha256: &str,
    run_log_paths: &mut Vec<NodeRunLogEntry>,
) -> std::result::Result<NodeRunResult, String> {
    let node_run_goal = node_run_goal.with_manifest_sha256(config_sha256);
    match federated::run_remote_goal(ctx, core_node, &node_run_goal, ctx.idle_timeouts.run).await {
        Ok(run) => {
            run_log_paths.push(NodeRunLogEntry {
                instance_id: instance_id.to_string(),
                node_label: key.label(),
                log_path: run.log_path,
                failed: run.outcome.is_err(),
                core_node: core_node.to_owned(),
            });
            run.outcome.map_err(|failure| failure.to_string())
        }
        Err(reason) => Err(reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
