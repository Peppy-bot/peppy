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
use super::changed_slots::{
    Change, ChangedSlot, UNDELIVERED_REMEDY, changed_slots, check_not_emptied, deliver_sets,
    holds_any, whole_sets,
};
use super::live::{live_machines, report_offline_sets, split_by_liveness};
use super::{change_active_launch, plan::selected_instances, stack_list_on};
use crate::services::node::stop_named_instances;
use config::runtime::Name;
use core_node_api::encoding::{
    LaunchFeedbackStep, LaunchResult, ParticipantInstancesRemoveRequest, StackRemoveGoal,
};
use daemon_config::launcher::{CopyMembership, DeploymentInstance, PeppyLauncher};
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

/// The deployments the launcher now holds, each carrying the pins the launch
/// resolved for its source, the way a join plans what it adds.
fn planned_from(
    launcher: &PeppyLauncher,
    resolved: &[PlannedDeployment],
) -> Vec<PlannedDeployment> {
    launcher
        .deployments
        .iter()
        .map(|deployment| {
            let item = resolved
                .iter()
                .find(|item| item.deployment.source == deployment.source)
                .expect("every deployment on the stack was resolved by the launch or by a join");
            PlannedDeployment {
                deployment: deployment.clone(),
                ..item.clone()
            }
        })
        .collect()
}

async fn remove_inner(
    name: &Name,
    active: &mut ActiveLaunch,
    ctx: &StackChangeContext,
) -> ChangeResult<()> {
    let copy = active.copies.get(name).cloned().ok_or_else(|| {
        format!("copy `{name}` is absent; peppy stack list shows the copies on the stack")
    })?;
    let staying: Vec<_> = active
        .copy_records()
        .filter(|other| other.name != copy.record.name)
        .cloned()
        .collect();
    let remaining = active
        .prepared
        .remove(&active.flat, &copy.record, &active.selection, &staying)
        .map_err(|e| e.to_string())?;
    let shrunk_slots = changed_slots(&copy.record, &active.planned, Change::Removal)?;
    check_not_emptied(&copy.record, &shrunk_slots, &remaining)?;
    let removed: HashSet<_> = copy.record.instance_ids().map(Name::as_str).collect();
    let remaining_planned = planned_from(&remaining, &active.resolved);
    let copies = CopyMembership::of(&staying);
    // The copy supplies a clock when one of its instances publishes a domain
    // this stack reads.
    let removed_domains: Vec<Name> = active
        .resolved_clocks
        .simulated()
        .filter(|(instance, _)| {
            active.resolved_clocks.of(instance).is_publisher() && removed.contains(instance)
        })
        .map(|(_, domain)| domain.name.clone())
        .collect();
    if !removed_domains.is_empty() {
        let consumers = ClockConsumers::of(
            remaining_planned
                .iter()
                .flat_map(|item| &item.deployment.instances),
            &active.resolved_clocks,
            &removed_domains,
            &copies,
        );
        if !consumers.is_empty() {
            return Err(consumers.refusal(name, &removed_domains));
        }
    }
    let root = ctx.node_stack.root().read().config().clone();
    let (_, bindings, _, observations) = validate_and_order_dependencies(
        ctx,
        &remaining_planned,
        &root,
        &active.placements,
        &copies,
        &active.resolved_clocks,
    )
    .await?;
    let watchers = lifecycle_watchers(&observations, &active.placements)?;
    let repointed = watchers_replacing(&active.watchers, &watchers);
    let live = live_machines(ctx, &active.placements).await?;
    let host_live = live.contains(copy.core_node.as_str());
    let (reachable_slots, offline_slots) =
        split_by_liveness(shrunk_slots, &active.placements, &live);
    let shrunk_sets = whole_sets(&reachable_slots, &bindings, &observations);
    let touched = removal_deployments(active, &copy, host_live, &repointed, &reachable_slots);
    let change = preflight_change(
        ctx,
        &active.launch_id,
        &touched,
        &active.placements,
        Some(&live),
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
        // The copy's instances are stopped by now, so a set that did not reach
        // its instance leaves the removal standing and says what heals it.
        if let Err(failure) =
            deliver_sets(ctx, &active.launch_id, &active.placements, shrunk_sets).await
        {
            publish_stderr(
                ctx,
                format!("{failure}\n{UNDELIVERED_REMEDY}"),
                LaunchFeedbackStep::LauncherStep,
            )
            .await;
        }
        report_offline_sets(ctx, name, &offline_slots, &active.placements).await;
        // A domain leaves with the instance that supplied it. Its lifetime
        // goes too, so a copy rejoining under the same name mints a new one.
        for domain in &removed_domains {
            active.clocks.remove(domain);
        }
        for instance in &removed {
            active.resolved_clocks.remove(instance);
        }
        Ok::<_, String>(())
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

/// What still reads a clock domain leaving with a copy.
#[derive(Debug, Default, PartialEq, Eq)]
struct ClockConsumers {
    /// Copies, which `stack remove` takes out one at a time.
    copies: BTreeSet<Name>,
    /// The stack's own instances, which only a reset stops.
    stack: Vec<Name>,
}

impl ClockConsumers {
    /// The readers of `domains` among `instances`, each filed under its copy
    /// where `copies` names one.
    fn of<'a>(
        instances: impl IntoIterator<Item = &'a DeploymentInstance>,
        clocks: &daemon_config::launcher::ResolvedClocks,
        domains: &[Name],
        copies: &CopyMembership,
    ) -> Self {
        let mut consumers = Self::default();
        for instance in instances.into_iter().filter(|instance| {
            clocks
                .of(instance.instance_id.as_str())
                .domain()
                .is_some_and(|domain| domains.contains(&domain.name))
        }) {
            match copies.copy_of(instance.instance_id.as_str()) {
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

    /// The refusal for removing `source`, the copy supplying `domains`, with
    /// the fix each kind of consumer admits.
    fn refusal(&self, source: &Name, domains: &[Name]) -> String {
        let clock = clock_label(domains);
        let copies = daemon_config::format_quoted_list(self.copies.iter().map(Name::as_str));
        if self.stack.is_empty() {
            return format!("`{source}` supplies {clock} to copies {copies}; remove them first");
        }
        let stack = daemon_config::format_quoted_list(self.stack.iter().map(Name::as_str));
        let and_copies = if self.copies.is_empty() {
            String::new()
        } else {
            format!(" and to copies {copies}")
        };
        format!(
            "`{source}` supplies {clock} to {stack}{and_copies}; run peppy stack reset, \
             then launch without `{source}`: drop its deployments entry from the launcher, or \
             leave it out of the copies you join"
        )
    }
}

/// How a refusal names the domains leaving with a copy.
fn clock_label(domains: &[Name]) -> String {
    let names = daemon_config::format_quoted_list(domains.iter().map(Name::as_str));
    match domains {
        [_] => format!("clock {names}"),
        _ => format!("clocks {names}"),
    }
}

/// Removal reserves the copy's host while it is live, the host of every
/// source whose watchers change, and the host of each of `shrunk_slots`,
/// the sets it delivers.
fn removal_deployments(
    active: &ActiveLaunch,
    copy: &StackCopy,
    host_live: bool,
    repointed: &LifecycleWatchers,
    shrunk_slots: &[ChangedSlot],
) -> Vec<PlannedDeployment> {
    selected_instances(&active.planned, |instance| {
        let host = active
            .placements
            .core_node_of(instance.instance_id.as_str());
        (host_live && host == &copy.core_node)
            || repointed.contains_key(&instance.instance_id)
            || holds_any(shrunk_slots, &instance.instance_id)
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
            &copy.record.instance_ids().cloned().collect::<Vec<_>>(),
        )
        .await;
        return Ok(());
    }
    let stack = stack_list_on(ctx, host).await?;
    // The host stops each instance in turn within its own teardown budget,
    // then answers.
    let timeout = copy_removal_budget(stack.shutdown_grace_secs, copy.record.instances.len())
        .saturating_add(STACK_QUERY_TIMEOUT);
    let instance_ids = core_node_api::encoding::RemovedInstances::try_from(
        copy.record.instance_ids().cloned().collect::<Vec<_>>(),
    )
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
    .and_then(|verdict| {
        verdict
            .into_result()
            .map_err(|reason| format!("`{host}` refused to remove the copy's instances: {reason}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use daemon_config::launcher::{CopyInstance, CopyRecord};

    fn instance(id: &str) -> DeploymentInstance {
        DeploymentInstance::empty(Name::new(id).unwrap())
    }

    /// The instances reading the departing domain, filed under their copy or
    /// the stack, with everything on another clock left out and the refusal
    /// naming the domain and the fix each kind admits.
    #[test]
    fn time_consumers_name_the_copies_to_remove_and_the_stack_to_reset() {
        let alpha = Name::new("alpha").unwrap();
        let instances = [
            instance("alpha_arm_inst"),
            instance("alpha_cam_inst"),
            instance("shared_inst"),
            instance("wall_inst"),
        ];
        let robot = config::runtime::ClockDomainId::new(
            Name::new("robot").unwrap(),
            config::runtime::CoreNodeName::new("cn-sim").unwrap(),
            config::runtime::ClockIncarnation::try_from(1).unwrap(),
        );
        let reads_robot = config::runtime::ClockBinding::consumer(
            robot.clone(),
            config::runtime::ProducerRef::new("cn-sim", "sim_inst"),
        );
        let clocks = daemon_config::launcher::ResolvedClocks::of_running([
            ("alpha_arm_inst".to_owned(), reads_robot.clone()),
            ("alpha_cam_inst".to_owned(), reads_robot.clone()),
            ("shared_inst".to_owned(), reads_robot),
            ("wall_inst".to_owned(), config::runtime::ClockBinding::Wall),
        ]);
        let domains = [Name::new("robot").unwrap()];
        let copies = CopyMembership::of(&[CopyRecord {
            name: alpha.clone(),
            axis: "robot".into(),
            option: "real".into(),
            selection: Default::default(),
            instances: ["arm_inst", "cam_inst"]
                .into_iter()
                .map(|id| CopyInstance {
                    instance_id: config::runtime::instance_id_in_copy(&alpha, id),
                    in_copy: Name::new(id).unwrap(),
                })
                .collect(),
            set_members: Vec::new(),
        }]);
        let consumers = ClockConsumers::of(&instances, &clocks, &domains, &copies);
        assert_eq!(consumers.copies, BTreeSet::from([alpha.clone()]));
        assert_eq!(consumers.stack, [Name::new("shared_inst").unwrap()]);
        let source = Name::new("sim").unwrap();
        let refusal = consumers.refusal(&source, &domains);
        assert!(
            refusal.contains("supplies clock `robot`"),
            "the refusal names the clock leaving with the copy: {refusal}"
        );
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

        let copies_only = ClockConsumers::of(&instances[..2], &clocks, &domains, &copies);
        assert!(!copies_only.is_empty());
        let refusal = copies_only.refusal(&source, &domains);
        assert!(
            refusal.ends_with("copies `alpha`; remove them first"),
            "{refusal}"
        );

        assert!(ClockConsumers::of(&instances[3..], &clocks, &domains, &copies).is_empty());
    }
}
