//! Taking one copy off the running stack: stopping its instances on the
//! machine that runs them, pointing the sources it observed at their
//! remaining watchers, and dropping it from the launch's record, which
//! leaves the launcher active and every other copy running.

use super::super::action::StackChangeContext;
use super::super::launch::feedback::{publish_stderr, publish_stdout};
use super::super::launch::orchestrate::validate_and_order_dependencies;
use super::super::launch::preflight::preflight_change;
use super::super::launch::watchers::{LifecycleWatchers, lifecycle_watchers, watchers_replacing};
use super::super::launch::{PlannedDeployment, federated};
use super::super::state::{ActiveLaunch, StackCopy};
use super::super::{ChangeResult, STACK_QUERY_TIMEOUT};
use super::{change_active_launch, plan::selected_instances, reason, stack_list_on};
use crate::services::node::stop_named_instances;
use config::runtime::Name;
use core_node_api::encoding::{
    LaunchFeedbackStep, LaunchResult, ParticipantInstancesRemoveRequest, StackRemoveGoal,
};
use daemon_config::launcher::DeploymentInstance;
use peppylib::core_node::transport::poll;
use std::collections::{BTreeSet, HashSet};
use std::time::Duration;

/// Removing a copy stops its instances one after another, each within its
/// host's teardown budget.
pub fn copy_removal_budget(shutdown_grace_secs: u64, instances: usize) -> Duration {
    crate::services::node::teardown_timeout(Duration::from_secs(shutdown_grace_secs))
        .saturating_mul(instances.try_into().unwrap_or(u32::MAX))
}

/// Stops the copy's instances and takes it out of the record. The record
/// follows what runs: once the instances are stopped the copy is gone from
/// it, whether or not the time source could be told. A copy whose machine
/// is off the federation leaves the record too; its instances stay for that
/// machine's `stack reset`.
pub(in crate::services::stack) async fn remove(
    goal: StackRemoveGoal,
    ctx: StackChangeContext,
) -> LaunchResult {
    change_active_launch(&ctx, async |active| {
        remove_inner(&goal.name, active, &ctx).await
    })
    .await
}

async fn remove_inner(
    name: &Name,
    active: &mut ActiveLaunch,
    ctx: &StackChangeContext,
) -> ChangeResult<()> {
    let copy = active.copies.get(name).cloned().ok_or_else(|| {
        format!("copy `{name}` is absent; peppy stack list shows the copies on the stack")
    })?;
    let remaining = active
        .prepared
        .remove(&active.flat, &copy.record)
        .map_err(|e| e.to_string())?;
    let removed: HashSet<_> = copy.record.instance_ids.iter().map(Name::as_str).collect();
    let remaining_planned = selected_instances(&active.planned, |instance| {
        !removed.contains(instance.instance_id.as_str())
    });
    let removes_clock = active
        .time_source
        .as_ref()
        .is_some_and(|source| removed.contains(source.instance_id.as_str()));
    if removes_clock {
        let consumers = TimeConsumers::of(
            remaining_planned
                .iter()
                .flat_map(|item| &item.deployment.instances),
            |instance| {
                active
                    .copies
                    .iter()
                    .find(|(_, copy)| copy.record.instance_ids.contains(instance))
                    .map(|(name, _)| name)
            },
        );
        if !consumers.is_empty() {
            return Err(consumers.refusal(name));
        }
    }
    let root = ctx.node_stack.root().read().config().clone();
    let (_, _, _, observations) =
        validate_and_order_dependencies(ctx, &remaining_planned, &root, &active.placements)
            .await
            .map_err(reason)?;
    let watchers = lifecycle_watchers(&observations, &active.placements)?;
    let repointed = watchers_replacing(&active.watchers, &watchers);
    let host_live = copy.core_node.as_str() == ctx.bound_core_node
        || federated::live_core_nodes(&ctx.messenger)
            .await?
            .contains(copy.core_node.as_str());
    let touched = removal_deployments(active, &copy, host_live, removes_clock, &repointed);
    let change = preflight_change(
        ctx,
        &active.launch_id,
        &remaining_planned,
        &touched,
        &active.placements,
        active.clock.as_ref(),
    )
    .await?;
    let outcome = async {
        if host_live {
            stop_copy(ctx, &active.launch_id, &copy).await?;
        } else {
            publish_stderr(
                ctx,
                format!(
                    "`{}` is not live on the federation, so the copy's instances there are not \
                     stopped; clear that machine with `peppy stack reset` from there when it \
                     returns",
                    copy.core_node
                ),
                LaunchFeedbackStep::LauncherStep,
            )
            .await;
        }
        active.copies.remove(name);
        active.flat = remaining;
        active.planned = remaining_planned;
        active.watchers = watchers;
        federated::set_participant_watchers(
            ctx,
            &active.launch_id,
            &change.reserved.core_nodes(),
            &repointed,
            &active.placements,
        )
        .await;
        if let Some(federated::ClockDemand::Sim(federated::SimDemandOrigin::Instance(instance))) =
            &active.clock
            && removed.contains(instance.as_str())
        {
            active.clock = Some(federated::ClockDemand::Sim(
                federated::SimDemandOrigin::ActiveLaunch,
            ));
        }
        match (removes_clock, &active.time_source, &change.fleet) {
            (true, _, _) => {
                active.time_source = None;
                Ok(())
            }
            (false, Some(source), Some(fleet)) => source
                .set_participants(ctx, fleet.clone())
                .await
                .map_err(|error| {
                    format!(
                        "copy `{name}` removed and its instances stopped, but the simulation \
                         time source keeps `{}` as a participant: {error}. The next join or \
                         removal hands it the participants again",
                        copy.core_node
                    )
                }),
            (false, _, _) => Ok(()),
        }
    }
    .await;
    change.reserved.release().await;
    outcome?;
    publish_stdout(
        ctx,
        format!("Copy `{name}` removed"),
        LaunchFeedbackStep::LauncherStep,
    )
    .await;
    Ok(())
}

/// What still reads simulation time once a copy leaves.
#[derive(Debug, Default, PartialEq, Eq)]
struct TimeConsumers {
    /// Copies, which `stack remove` takes out one at a time.
    copies: BTreeSet<Name>,
    /// The stack's own instances, which only a reset stops.
    stack: Vec<Name>,
}

impl TimeConsumers {
    /// The simulation-time readers among `instances`, each filed under its
    /// copy where `copy_of` names one.
    fn of<'a>(
        instances: impl IntoIterator<Item = &'a DeploymentInstance>,
        copy_of: impl Fn(&Name) -> Option<&'a Name>,
    ) -> Self {
        let mut consumers = Self::default();
        for instance in instances
            .into_iter()
            .filter(|instance| instance.framework.use_sim_time != Some(false))
        {
            match copy_of(&instance.instance_id) {
                Some(copy) => {
                    consumers.copies.insert(copy.clone());
                }
                None => consumers.stack.push(instance.instance_id.clone()),
            }
        }
        consumers
    }

    fn is_empty(&self) -> bool {
        self.copies.is_empty() && self.stack.is_empty()
    }

    /// The refusal for removing `source`, the copy supplying their time,
    /// with the fix each kind of consumer admits.
    fn refusal(&self, source: &Name) -> String {
        let copies = daemon_config::format_quoted_list(self.copies.iter().map(Name::as_str));
        if self.stack.is_empty() {
            return format!(
                "`{source}` supplies simulation time to copies {copies}; remove them first"
            );
        }
        let stack = daemon_config::format_quoted_list(self.stack.iter().map(Name::as_str));
        let and_copies = if self.copies.is_empty() {
            String::new()
        } else {
            format!(" and to copies {copies}")
        };
        format!(
            "`{source}` supplies simulation time to {stack}{and_copies}; run peppy stack reset, \
             then launch without `{source}`: drop its deployments entry from the launcher, or \
             leave it out of the copies you join"
        )
    }
}

/// Removal reserves the copy's host while it is live, its clock source's
/// host while the source stays and is handed the remaining fleet, and the
/// host of every source whose watchers change.
fn removal_deployments(
    active: &ActiveLaunch,
    copy: &StackCopy,
    host_live: bool,
    removes_clock: bool,
    repointed: &LifecycleWatchers,
) -> Vec<PlannedDeployment> {
    selected_instances(&active.planned, |instance| {
        let host = active
            .placements
            .core_node_of(instance.instance_id.as_str());
        (host_live && host == &copy.core_node)
            || (!removes_clock
                && active
                    .time_source
                    .as_ref()
                    .is_some_and(|source| host == &source.core_node))
            || repointed.contains_key(&instance.instance_id)
    })
}

/// Stops the copy's instances on the machine that runs them.
pub(super) async fn stop_copy(
    ctx: &StackChangeContext,
    launch_id: &str,
    copy: &StackCopy,
) -> ChangeResult<()> {
    let host = copy.core_node.as_str();
    if host == ctx.bound_core_node {
        stop_named_instances(
            &ctx.messenger,
            &ctx.bound_core_node,
            &ctx.core_instance_id,
            &ctx.node_stack,
            &ctx.relationships,
            &copy.record.instance_ids,
        )
        .await;
        return Ok(());
    }
    let stack = stack_list_on(ctx, host).await?;
    // The host stops each instance in turn within its own teardown budget,
    // then answers.
    let timeout = copy_removal_budget(stack.shutdown_grace_secs, copy.record.instance_ids.len())
        .saturating_add(STACK_QUERY_TIMEOUT);
    let instance_ids =
        core_node_api::encoding::RemovedInstances::try_from(copy.record.instance_ids.clone())
            .map_err(|error| format!("cannot remove `{host}`'s instances: {error}"))?;
    poll(
        &ParticipantInstancesRemoveRequest {
            launch_id: launch_id.to_owned(),
            instance_ids,
        },
        &ctx.messenger,
        &ctx.bound_core_node,
        &ctx.core_instance_id,
        host,
        timeout,
    )
    .await
    .map_err(|e| format!("cannot remove instances on `{host}`: {e}"))
    .and_then(|response| {
        if response.ok {
            Ok(())
        } else {
            Err(format!(
                "`{host}` refused to remove the copy's instances: {}",
                response
                    .rejection_reason
                    .unwrap_or_else(|| String::from("no reason given"))
            ))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instance(id: &str, use_sim_time: Option<bool>) -> DeploymentInstance {
        let mut instance = DeploymentInstance::empty(Name::new(id).unwrap());
        instance.framework.use_sim_time = use_sim_time;
        instance
    }

    /// The readers of simulation time are filed under their copy or the
    /// stack, wall-time readers left out, and the refusal names the fix
    /// each kind admits.
    #[test]
    fn time_consumers_name_the_copies_to_remove_and_the_stack_to_reset() {
        let alpha = Name::new("alpha").unwrap();
        let instances = [
            instance("alpha_arm_inst", None),
            instance("alpha_cam_inst", Some(true)),
            instance("shared_inst", None),
            instance("wall_inst", Some(false)),
        ];
        let copy_of = |id: &Name| id.as_str().starts_with("alpha_").then_some(&alpha);
        let consumers = TimeConsumers::of(&instances, copy_of);
        assert_eq!(consumers.copies, BTreeSet::from([alpha.clone()]));
        assert_eq!(consumers.stack, [Name::new("shared_inst").unwrap()]);
        let source = Name::new("sim").unwrap();
        let refusal = consumers.refusal(&source);
        assert!(
            refusal.contains("`shared_inst` and to copies `alpha`"),
            "{refusal}"
        );
        assert!(
            refusal.contains("run peppy stack reset, then launch without `sim`"),
            "{refusal}"
        );
        assert!(
            refusal.contains("drop its deployments entry from the launcher"),
            "the refusal says what to change in the launcher: {refusal}"
        );

        let copies_only = TimeConsumers::of(&instances[..2], copy_of);
        assert!(!copies_only.is_empty());
        let refusal = copies_only.refusal(&source);
        assert!(
            refusal.ends_with("copies `alpha`; remove them first"),
            "{refusal}"
        );

        assert!(TimeConsumers::of(&instances[3..], copy_of).is_empty());
    }
}
