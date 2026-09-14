//! The add-and-build phase of a stack change: every node of the plan present
//! and built on each machine that runs part of it, before any instance starts.

use super::federated::RemoteGoalFailure;
use super::feedback::publish_stdout;
use super::orchestrate::{add_node_directly, build_node_directly};
use super::{
    HostedNode, NodeKey, PhaseChange, PhaseGoal, PlannedDeployment, UnresolvedAdd, federated,
};
use crate::services::node::pins::encode_pins;
use crate::services::stack::action::StackChangeContext;
use config::runtime::CoreNodeName;
use core_node_api::encoding::{
    LaunchFeedbackStep, NodeAddGoal, NodeAddLogEntry, NodeBuildGoal, NodeBuildLogEntry, NodeSource,
};
use daemon_config::repository::DeploymentRoot;
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

/// Marker git_hash for an add whose bytes a pin already vouched for: the ones
/// stack launch issues, and the per-node sub-goals `add_batch` issues for any
/// pinned add (`peppy node add <name>:<tag>` included). The node_add service
/// skips git hash and codegen-fingerprint verification for it and generates
/// fresh peppygen files: those checks belong to `peppy node sync` workflows,
/// and these adds operate on a tree materialized from an already-verified pin.
pub(crate) const STACK_LAUNCH_GIT_HASH: &str = "stack-launch";

/// The build goal a peer receives for one deployment of `goal`: the launch's
/// identity, so the peer accepts it while reserved for that launch, and the
/// launch's `rebuild` switch, so a `--rebuild` launch ignores cached
/// artifacts on every machine, not only on the coordinator.
fn remote_build_goal(
    phase: &PhaseGoal,
    node_name: String,
    node_tag: String,
    budget: Duration,
) -> NodeBuildGoal {
    NodeBuildGoal::new(node_name, node_tag, budget.as_secs())
        .with_launch_id(&phase.launch_id)
        .with_rebuild(phase.rebuild)
}

/// The add source a deployment dispatches with: its root, encoded at the
/// point of dispatch beside the closure pins, so the local arm and the goal a
/// peer receives cannot disagree on how the pins travel.
fn pinned_source(
    key: &NodeKey,
    item: &PlannedDeployment,
) -> std::result::Result<NodeSource, String> {
    match &item.root {
        DeploymentRoot::Node(pin) => serde_json5::to_string(pin)
            .map(|pin_json5| NodeSource::Pinned { pin_json5 })
            .map_err(|e| format!("deployment {}: could not encode its pin: {e}", key.label())),
        DeploymentRoot::Exposures(pins) => encode_pins(pins)
            .map(|pins_json5| NodeSource::Exposures { pins_json5 })
            .map_err(|e| format!("deployment {}: {e}", key.label())),
    }
}

/// Whether a deployment is the built-in MCP server, which is registered by
/// its add and has no build stage.
fn is_built_in(item: &PlannedDeployment) -> bool {
    matches!(item.root, DeploymentRoot::Exposures(_))
}

/// Which core nodes host at least one instance of `key`, in a stable order.
///
/// A node is added and built on every machine that runs part of it, which is
/// what "several placed instances under one deployment" means operationally:
/// each daemon has to have the node present before it can start its share.
fn hosts_of(
    item: &PlannedDeployment,
    placements: &daemon_config::launcher::Placements,
) -> Vec<CoreNodeName> {
    let mut hosts: Vec<CoreNodeName> = item
        .deployment
        .instances
        .iter()
        .map(|instance| {
            placements
                .core_node_of(instance.instance_id.as_str())
                .clone()
        })
        .collect();
    hosts.sort();
    hosts.dedup();
    hosts
}

/// Adds and builds every node of the change, grouped by the machine that
/// will run it.
///
/// The groups run CONCURRENTLY and each group runs in dependency order. That
/// split is deliberate: nothing orders one machine's fetch-and-build against
/// another's, and fetching plus building is where a launch spends nearly all
/// of its wall clock, so serializing across machines would make a two-machine
/// launch twice as slow for no invariant. Within a group the order is exactly
/// what a single-machine launch does, because that is where the ordering
/// actually matters (a node's transitive dependencies).
///
/// `unresolved` collects the nodes whose add on a peer outlived its budget:
/// whether each landed is known only by asking that peer.
#[allow(clippy::too_many_arguments)] // Distinct inputs; bundling them would only move the list.
pub(in crate::services::stack) async fn add_nodes_to_stack(
    ctx: &StackChangeContext,
    phase: &PhaseGoal,
    change: PhaseChange<'_>,
    ordered: &[NodeKey],
    planned_by_key: &HashMap<NodeKey, PlannedDeployment>,
    placements: &daemon_config::launcher::Placements,
    add_log_paths: &mut Vec<NodeAddLogEntry>,
    build_log_paths: &mut Vec<NodeBuildLogEntry>,
    unresolved: &mut Vec<UnresolvedAdd>,
) -> std::result::Result<(), String> {
    publish_stdout(
        ctx,
        "Adding nodes to the stack...",
        LaunchFeedbackStep::LauncherStep,
    )
    .await;

    let mut by_core_node: BTreeMap<CoreNodeName, Vec<&NodeKey>> = BTreeMap::new();
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
        add_and_build_group(ctx, phase, change, core_node, keys, planned_by_key, local)
    }))
    .await;

    let mut failure: Option<String> = None;
    for group in groups {
        add_log_paths.extend(group.add_logs);
        build_log_paths.extend(group.build_logs);
        unresolved.extend(group.unresolved);
        // Report the FIRST failure but keep collecting every group's logs: a
        // launch that failed on one machine still produced logs on the others,
        // and those are usually what explains it.
        if let Some(reason) = group.failure {
            failure.get_or_insert(reason);
        }
    }

    match failure {
        Some(reason) => Err(reason),
        None => Ok(()),
    }
}

/// What one machine's add-and-build group produced.
#[derive(Default)]
struct GroupOutcome {
    add_logs: Vec<NodeAddLogEntry>,
    build_logs: Vec<NodeBuildLogEntry>,
    unresolved: Vec<UnresolvedAdd>,
    failure: Option<String>,
}

async fn add_and_build_group(
    ctx: &StackChangeContext,
    phase: &PhaseGoal,
    change: PhaseChange<'_>,
    core_node: &CoreNodeName,
    keys: &[&NodeKey],
    planned_by_key: &HashMap<NodeKey, PlannedDeployment>,
    local: &str,
) -> GroupOutcome {
    let mut outcome = GroupOutcome::default();

    for key in keys {
        let Some(item) = planned_by_key.get(key) else {
            continue;
        };

        if change.reuses(key, core_node.as_str()) {
            continue;
        }

        publish_stdout(
            ctx,
            format!("Adding {} on `{core_node}`", key.label()),
            LaunchFeedbackStep::AddingNode,
        )
        .await;

        if core_node.as_str() != local {
            if let Err(reason) =
                add_and_build_remotely(ctx, phase, core_node, key, item, &mut outcome).await
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
        let encoded = pinned_source(key, item)
            .and_then(|source| encode_pins(&item.closure_pins).map(|pins| (source, pins)));
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

        // The built-in server is registered ready by its add: nothing to
        // build.
        if is_built_in(item) {
            continue;
        }

        let node_name = added.node_name.clone().unwrap_or_else(|| key.name.clone());
        let node_tag = added.node_tag.clone().unwrap_or_else(|| key.tag.clone());

        // Stack launch chains directly from add into build, since the
        // launcher's contract is "the stack is up and running"; an
        // `Added` entity isn't actually buildable from the user's
        // perspective until `node build` has run.
        let (build_result, build_log_path) = build_node_directly(
            ctx,
            node_name,
            node_tag,
            ctx.env_vars.clone(),
            phase.rebuild,
        )
        .await;

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
    ctx: &StackChangeContext,
    phase: &PhaseGoal,
    core_node: &CoreNodeName,
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
    .with_launch_id(&phase.launch_id)
    .with_pins(encode_pins(&item.closure_pins)?);
    let added =
        match federated::run_remote_goal(ctx, core_node.as_str(), &add_goal, ctx.idle_timeouts.add)
            .await
        {
            Ok(run) => {
                outcome.add_logs.push(NodeAddLogEntry {
                    node_label: key.label(),
                    log_path: run.log_path,
                    failed: run.outcome.is_err(),
                    core_node: core_node.to_string(),
                });
                if matches!(run.outcome, Err(RemoteGoalFailure::Unresolved(_))) {
                    outcome.unresolved.push(UnresolvedAdd {
                        hosted: HostedNode {
                            node: key.clone(),
                            core_node: core_node.clone(),
                        },
                        config_sha256: item.config_sha256.clone(),
                    });
                }
                run.outcome.map_err(|failure| failure.to_string())
            }
            Err(reason) => Err(reason),
        }
        .map_err(|reason| format!("failed to add node {}: {reason}", item.node_name))?;

    if is_built_in(item) {
        return Ok(());
    }

    let build_goal = remote_build_goal(
        phase,
        added.node_name.unwrap_or_else(|| item.node_name.clone()),
        added.node_tag.unwrap_or_else(|| item.node_tag.clone()),
        ctx.idle_timeouts.build,
    );
    match federated::run_remote_goal(
        ctx,
        core_node.as_str(),
        &build_goal,
        ctx.idle_timeouts.build,
    )
    .await
    {
        Ok(run) => {
            outcome.build_logs.push(NodeBuildLogEntry {
                node_label: key.label(),
                log_path: run.log_path,
                failed: run.outcome.is_err(),
                core_node: core_node.to_string(),
            });
            run.outcome.map_err(|failure| failure.to_string())
        }
        Err(reason) => Err(reason),
    }
    .map_err(|reason| format!("failed to build node {}: {reason}", item.node_name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_node_api::encoding::LaunchGoal;

    #[test]
    fn remote_build_goal_carries_the_launch_rebuild_switch() {
        let launch = LaunchGoal::new(
            core_node_api::encoding::LauncherOrigin::repository("openarm_v2"),
            "launch-abc123",
            core_node_api::encoding::StackBudgets::new(1, 1, 1, None),
        )
        .with_rebuild(true);

        let build = remote_build_goal(
            &PhaseGoal {
                launch_id: launch.launch_id.clone(),
                rebuild: launch.rebuild,
            },
            "waldo".to_string(),
            "v1".to_string(),
            Duration::from_secs(90),
        );

        assert_eq!(build.node_name, "waldo");
        assert_eq!(build.node_tag, "v1");
        assert_eq!(build.timeout_secs, 90);
        assert_eq!(build.launch_id.as_deref(), Some("launch-abc123"));
        assert!(build.rebuild, "the launch's --rebuild reaches the peer");
        assert!(
            !build.force,
            "a dispatched build never supersedes a peer's own build"
        );

        let plain = remote_build_goal(
            &PhaseGoal {
                launch_id: launch.launch_id.clone(),
                rebuild: false,
            },
            "waldo".to_string(),
            "v1".to_string(),
            Duration::from_secs(90),
        );
        assert!(!plain.rebuild);
    }
}
