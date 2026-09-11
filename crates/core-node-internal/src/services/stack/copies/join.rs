//! Adding one copy to the running stack: composing it against the launch,
//! reserving the machines it touches, adding and starting its nodes, and
//! undoing exactly the stages it reached when any of that fails.

use super::super::action::StackChangeContext;
use super::super::container_mounts::{
    LocalMounts, hosts_container_nodes, prepare_local_container_mounts,
};
use super::super::launch::clock::{TimeSource, plan_fleet, warn_when_no_time_source};
use super::super::launch::feedback::publish_stdout;
use super::super::launch::nodes::add_nodes_to_stack;
use super::super::launch::orchestrate::validate_and_order_dependencies;
use super::super::launch::preflight::preflight_change;
use super::super::launch::start::start_node_instances;
use super::super::launch::watchers::{lifecycle_watchers, watchers_replacing};
use super::super::launch::{
    HostedNode, JoinScope, NodeKey, PhaseChange, PhaseGoal, UnresolvedAdd, federated,
};
use super::super::state::{ActiveLaunch, StackCopy, instance_ids_in_start_order};
use super::super::{ChangeResult, STACK_QUERY_TIMEOUT};
use super::{
    change_active_launch,
    live::{LiveCheck, check_live_stack, node_info_on},
    plan::{ResolvedJoin, join_dependencies, selected_instances},
    reason,
    remove::stop_copy,
};
use crate::services::node::common::panic_message;
use core_node_api::encoding::{
    LaunchFeedbackStep, LaunchResult, NodeAddLogEntry, NodeBuildLogEntry, NodeRunLogEntry,
    StackJoinGoal,
};
use daemon_config::launcher::CopyRecord;
use futures::FutureExt;
use std::{
    collections::{HashMap, HashSet},
    panic::AssertUnwindSafe,
};

/// Adds the copy `goal` names to the running stack and records it with the
/// launch, whatever the outcome: a failed join rolls back to the record it
/// started from.
pub(in crate::services::stack) async fn join(
    goal: StackJoinGoal,
    ctx: StackChangeContext,
) -> LaunchResult {
    let mut logs = JoinLogs::default();
    let result = change_active_launch(&ctx, async |active| {
        join_inner(&goal, active, &ctx, &mut logs).boxed().await
    })
    .await;
    result.with_node_logs(logs.add, logs.build, logs.run)
}

/// What the join's node phases produced: the log of every add, build and
/// run, and the nodes whose add on a peer outlived its budget.
#[derive(Default)]
struct JoinLogs {
    add: Vec<NodeAddLogEntry>,
    build: Vec<NodeBuildLogEntry>,
    run: Vec<NodeRunLogEntry>,
    unresolved: Vec<UnresolvedAdd>,
}

/// How far a join got, so its rollback undoes what happened and nothing
/// more.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum JoinStage {
    /// No machine has changed.
    Planned,
    /// The participants were asked to hold this launch's slice and every
    /// source points at its new watchers.
    SlicesBegun,
    /// The record names the copy and the time source holds the new fleet.
    RecordChanged,
    /// Node entities were being added; the add logs say which landed.
    NodesAdding,
    /// Instances were being started, their observers registered.
    InstancesStarting,
}

/// What a rollback has to undo, given how far the join got. Each step
/// belongs to the stage that did the work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RollbackPlan {
    stops_instances: bool,
    removes_added_nodes: bool,
    clears_slices: bool,
    restores_participants: bool,
}

impl JoinStage {
    fn rollback_plan(self) -> RollbackPlan {
        RollbackPlan {
            stops_instances: self >= Self::InstancesStarting,
            removes_added_nodes: self >= Self::NodesAdding,
            clears_slices: self >= Self::SlicesBegun,
            restores_participants: self >= Self::RecordChanged,
        }
    }
}

/// Composes the copy, reserves the machines it touches, checks the
/// instances it depends on are live, then adds, builds and starts its nodes.
/// A failure undoes what the stage it reached did and restores the record.
async fn join_inner(
    goal: &StackJoinGoal,
    active: &mut ActiveLaunch,
    ctx: &StackChangeContext,
    logs: &mut JoinLogs,
) -> ChangeResult<()> {
    let ResolvedJoin {
        combined,
        planned,
        placements,
        new_ids,
        host,
        copy,
        resolved,
    } = ResolvedJoin::resolve(ctx, active, goal).await?;
    let root = ctx.node_stack.root().read().config().clone();
    let (ordered, bindings, pairings, observations) =
        validate_and_order_dependencies(ctx, &planned, &root, &placements)
            .await
            .map_err(reason)?;
    let watchers = lifecycle_watchers(&observations, &placements)?;
    let required_ids = join_dependencies(
        &planned,
        &new_ids,
        active
            .time_source
            .as_ref()
            .map(|source| &source.instance_id),
    );
    let touched = selected_instances(&planned, |instance| {
        required_ids.contains(instance.instance_id.as_str())
    });
    let change = preflight_change(
        ctx,
        &active.launch_id,
        &planned,
        &touched,
        &placements,
        active.clock.as_ref(),
    )
    .await?;
    let participants = change.reserved.core_nodes();
    let delta: HashMap<_, _> = selected_instances(&planned, |instance| {
        new_ids.contains(instance.instance_id.as_str())
    })
    .into_iter()
    .map(|item| (NodeKey::new(&item.node_name, &item.node_tag), item))
    .collect();
    let owned: HashSet<_> = delta
        .values()
        .flat_map(|item| &item.deployment.instances)
        .map(|instance| &instance.instance_id)
        .collect();
    let record = StackCopy {
        record: CopyRecord {
            instance_ids: instance_ids_in_start_order(&planned, &ordered, &owned),
            ..copy.clone()
        },
        core_node: host,
    };
    let phase = PhaseGoal {
        launch_id: active.launch_id.clone(),
        rebuild: false,
    };
    let previous = active.clone();
    let time_source = active
        .time_source
        .clone()
        .or_else(|| TimeSource::of(ctx, &planned, &placements, &change.reserved));
    // Set once the machines start changing: from then on a failure has
    // something to roll back.
    let mut scope: Option<JoinScope> = None;
    let mut stage = JoinStage::Planned;
    let operation = async {
        let clock = change.reserved.established_clock(&change.clock)?;
        warn_when_no_time_source(ctx, &planned, &clock).await;
        let live = check_live_stack(
            ctx,
            LiveCheck {
                active,
                touched: &touched,
                placements: &placements,
                participants: &participants,
                new_ids: &new_ids,
                required_ids: &required_ids,
            },
        )
        .await?;
        prepare_local_container_mounts(
            ctx,
            hosts_container_nodes(delta.values(), &placements, &ctx.bound_core_node),
            change
                .mounts
                .get(&ctx.bound_core_node)
                .cloned()
                .unwrap_or_default(),
            if crate::services::stack::holds_nodes(&ctx.node_stack) {
                LocalMounts::AddedToRunningStack
            } else {
                LocalMounts::WholeStack
            },
        )
        .await?;
        let planned_nodes = delta
            .values()
            .flat_map(|item| {
                item.deployment.instances.iter().map(|instance| HostedNode {
                    node: NodeKey::new(&item.node_name, &item.node_tag),
                    core_node: placements
                        .core_node_of(instance.instance_id.as_str())
                        .clone(),
                })
            })
            .collect();
        let scope: &mut JoinScope = scope.insert(JoinScope {
            name: goal.name.clone(),
            copy: record.clone(),
            existing_nodes: live.reusable,
            planned_nodes,
            fresh_hosts: live.fresh_hosts,
            participants: participants.clone(),
            placements: placements.clone(),
            restored_watchers: watchers_replacing(&watchers, &previous.watchers),
        });
        stage = JoinStage::SlicesBegun;
        federated::begin_participant_slices(
            ctx,
            &phase.launch_id,
            &participants,
            &change.mounts,
            &watchers,
            &placements,
            true,
        )
        .await
        .map_err(|refusal| {
            // A machine that refused kept its own stack and takes no
            // watchers; the rollback clears the fresh machines that took
            // the slice and re-points the rest.
            scope.fresh_hosts = refusal.holders_among(&scope.fresh_hosts);
            scope.participants = refusal.holders_among(&scope.participants);
            refusal.reason
        })?;
        stage = JoinStage::RecordChanged;
        active.flat = combined;
        active.planned = planned;
        active.resolved.extend(resolved);
        active.placements = placements.clone();
        active.time_source = time_source;
        active.clock = Some(clock);
        active.watchers = watchers;
        active.copies.insert(goal.name.clone(), record);
        if let (Some(source), Some(fleet)) = (&previous.time_source, &change.fleet) {
            source.set_participants(ctx, fleet.clone()).await?;
        }
        stage = JoinStage::NodesAdding;
        add_nodes_to_stack(
            ctx,
            &phase,
            PhaseChange::Join(scope),
            &ordered,
            &delta,
            &placements,
            &mut logs.add,
            &mut logs.build,
            &mut logs.unresolved,
        )
        .await?;
        let pairings: Vec<_> = pairings
            .into_iter()
            .filter(|pair| {
                new_ids.contains(&pair.a.instance_id) || new_ids.contains(&pair.b.instance_id)
            })
            .collect();
        let observations: Vec<_> = observations
            .into_iter()
            .filter(|observation| new_ids.contains(&observation.observer_instance_id))
            .collect();
        stage = JoinStage::InstancesStarting;
        start_node_instances(
            ctx,
            &phase,
            PhaseChange::Join(scope),
            &ordered,
            &delta,
            &mut logs.run,
            &bindings,
            &pairings,
            &observations,
            &placements,
            change.fleet.as_ref(),
        )
        .await?;
        Ok(())
    };
    let outcome: ChangeResult<()> = AssertUnwindSafe(operation)
        .catch_unwind()
        .await
        .unwrap_or_else(|panic| Err(format!("join failed: {}", panic_message(&*panic))));
    let outcome = match (outcome, scope) {
        (Err(error), Some(scope)) => {
            match rollback_join(ctx, &scope, stage, logs, previous, active).await {
                Ok(()) => Err(error),
                Err(cleanup) => Err(format!("{error}; {cleanup}")),
            }
        }
        (other, _) => other,
    };
    change.reserved.release().await;
    outcome?;
    publish_stdout(
        ctx,
        format!(
            "Copy `{}` of `{}` joined: {}",
            goal.name,
            copy.option,
            copy.selection.echo()
        ),
        LaunchFeedbackStep::LauncherStep,
    )
    .await;
    Ok(())
}

/// Undoes what the failed join's stage did: stops what it started, removes
/// the nodes it added and the nodes a peer holds for an add that outlived
/// its budget, puts every machine's sources back on their previous
/// watchers, clears the machines that held nothing before it, hands the
/// time source its previous participants, and restores the record it
/// started from. A cleanup that fails leaves the copy recorded, so `stack
/// remove` can retry it.
async fn rollback_join(
    ctx: &StackChangeContext,
    scope: &JoinScope,
    stage: JoinStage,
    logs: &JoinLogs,
    previous: ActiveLaunch,
    active: &mut ActiveLaunch,
) -> ChangeResult<()> {
    let launch_id = &previous.launch_id;
    let plan = stage.rollback_plan();
    let cleanup = async {
        if plan.stops_instances {
            stop_copy(ctx, launch_id, &scope.copy).await?;
        }
        if plan.removes_added_nodes {
            for hosted in scope.added_nodes() {
                if scope.fresh_hosts.contains(&hosted.core_node.to_string()) {
                    continue;
                }
                let present = landed(&logs.add, hosted)
                    || match logs.unresolved.iter().find(|add| add.hosted == *hosted) {
                        Some(add) => holds_node(ctx, hosted, &add.config_sha256).await?,
                        None => false,
                    };
                if present {
                    remove_node(ctx, launch_id, hosted).await?;
                }
            }
        }
        if plan.clears_slices {
            // Ahead of the clearing: a machine takes watchers for this launch
            // only while it holds its slice.
            federated::set_participant_watchers(
                ctx,
                launch_id,
                &scope.participants,
                &scope.restored_watchers,
                &scope.placements,
            )
            .await;
            federated::clear_participant_slices(ctx, &scope.fresh_hosts).await;
        }
        if plan.restores_participants
            && let Some(source) = &previous.time_source
            && let Some(fleet) = plan_fleet(&previous.planned, &previous.placements)?
        {
            source.set_participants(ctx, fleet).await?;
        }
        Ok::<_, String>(())
    }
    .await;
    cleanup.map_err(|error| {
        format!(
            "cleanup failed: {error}. The copy remains listed; retry peppy stack remove {}",
            scope.name
        )
    })?;
    *active = previous;
    Ok(())
}

/// Whether `hosted`'s machine holds the node the join added, asked after
/// an add that outlived its budget: an entity of that name with another
/// fingerprint was there before the join.
async fn holds_node(
    ctx: &StackChangeContext,
    hosted: &HostedNode,
    config_sha256: &str,
) -> ChangeResult<bool> {
    let response = node_info_on(ctx, hosted.core_node.as_str(), &hosted.node).await?;
    Ok(matches!(
        response,
        core_node_api::encoding::NodeInfoResponse::Found(info) if info.config_integrity == config_sha256
    ))
}

/// Whether the add phase put `hosted` on its machine.
fn landed(added: &[NodeAddLogEntry], hosted: &HostedNode) -> bool {
    added.iter().any(|entry| {
        !entry.failed
            && entry.node_label == hosted.node.label()
            && entry.core_node == hosted.core_node.as_str()
    })
}

/// Removes one node the join added, from this daemon or from the peer
/// that hosts it, after its instances stopped. The peer admits the
/// removal by the launch it is reserved for.
async fn remove_node(
    ctx: &StackChangeContext,
    launch_id: &str,
    hosted: &HostedNode,
) -> ChangeResult<()> {
    if hosted.core_node.as_str() == ctx.bound_core_node {
        ctx.node_stack
            .remove_config(&hosted.node.name, &hosted.node.tag)
            .map_err(|error| format!("cannot remove {}: {error}", hosted.node.label()))?;
        return Ok(());
    }
    let response = peppylib::core_node::transport::poll(
        &core_node_api::encoding::NodeRemoveRequest::new(&hosted.node.name, &hosted.node.tag)
            .with_launch_id(launch_id),
        &ctx.messenger,
        &ctx.bound_core_node,
        &ctx.core_instance_id,
        hosted.core_node.as_str(),
        STACK_QUERY_TIMEOUT,
    )
    .await
    .map_err(|error| {
        format!(
            "cannot remove {} from `{}`: {error}",
            hosted.node.label(),
            hosted.core_node
        )
    })?;
    if response.success {
        return Ok(());
    }
    Err(format!(
        "`{}` refused to remove {}: {}",
        hosted.core_node,
        hosted.node.label(),
        response
            .error_message
            .unwrap_or_else(|| String::from("no reason given"))
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::runtime::CoreNodeName;
    use std::path::PathBuf;

    fn hosted(node: &str, core_node: &str) -> HostedNode {
        HostedNode {
            node: NodeKey::new(node, "v1"),
            core_node: CoreNodeName::new(core_node).expect("valid core node name"),
        }
    }

    fn add_log(node_label: &str, core_node: &str, failed: bool) -> NodeAddLogEntry {
        NodeAddLogEntry {
            node_label: node_label.to_owned(),
            log_path: PathBuf::from("/tmp/add.log"),
            failed,
            core_node: core_node.to_owned(),
        }
    }

    /// Only an add that succeeded on the node's own machine put it there, so
    /// only that one is the rollback's to remove. Every other node on that
    /// machine belongs to the stack the join found running.
    #[test]
    fn a_node_landed_only_where_its_add_succeeded() {
        let recorder = hosted("recorder", "cn-cloud");

        assert!(landed(
            &[add_log("recorder:v1", "cn-cloud", false)],
            &recorder
        ));
        assert!(!landed(&[], &recorder), "nothing was added");
        assert!(
            !landed(&[add_log("recorder:v1", "cn-cloud", true)], &recorder),
            "a failed add left nothing behind"
        );
        assert!(
            !landed(&[add_log("recorder:v1", "cn-edge", false)], &recorder),
            "the add succeeded on another machine"
        );
        assert!(
            !landed(&[add_log("camera:v1", "cn-cloud", false)], &recorder),
            "another node was added there"
        );
        assert!(
            landed(
                &[
                    add_log("camera:v1", "cn-cloud", false),
                    add_log("recorder:v1", "cn-cloud", false),
                ],
                &recorder
            ),
            "the machine's other adds do not hide this one"
        );
    }

    /// Each stage undoes its own work and every earlier stage's, so a join
    /// that failed before a step never has that step undone.
    #[test]
    fn a_rollback_undoes_every_stage_the_join_reached() {
        let nothing = RollbackPlan {
            stops_instances: false,
            removes_added_nodes: false,
            clears_slices: false,
            restores_participants: false,
        };
        assert_eq!(JoinStage::Planned.rollback_plan(), nothing);
        assert_eq!(
            JoinStage::SlicesBegun.rollback_plan(),
            RollbackPlan {
                clears_slices: true,
                ..nothing
            }
        );
        assert_eq!(
            JoinStage::RecordChanged.rollback_plan(),
            RollbackPlan {
                clears_slices: true,
                restores_participants: true,
                ..nothing
            }
        );
        assert_eq!(
            JoinStage::NodesAdding.rollback_plan(),
            RollbackPlan {
                removes_added_nodes: true,
                clears_slices: true,
                restores_participants: true,
                ..nothing
            }
        );
        assert_eq!(
            JoinStage::InstancesStarting.rollback_plan(),
            RollbackPlan {
                stops_instances: true,
                removes_added_nodes: true,
                clears_slices: true,
                restores_participants: true,
            }
        );
    }
}
