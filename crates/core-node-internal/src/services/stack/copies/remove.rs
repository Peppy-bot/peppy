//! Taking one copy off the running stack: stopping its instances on each
//! machine that runs them, pointing the sources it observed at their
//! remaining watchers, and dropping it from the launch's record, which
//! leaves the launcher active and every other copy running.
//!
//! A copy runs on several machines when a part of it runs away from its
//! base. Each of them is in one of three states: live and holding the slice
//! of this launch, where the copy's instances stop; live and holding no
//! slice of it, because someone reset it, where nothing stops; and not live,
//! where nothing stops and a warning says what stays there.

use super::super::action::StackChangeContext;
use super::super::launch::feedback::{publish_stdout, publish_warning};
use super::super::launch::orchestrate::{ValidatedPlan, validate_and_order_dependencies};
use super::super::launch::preflight::preflight_change;
use super::super::launch::watchers::{LifecycleWatchers, lifecycle_watchers, watchers_replacing};
use super::super::launch::{PlannedDeployment, federated};
use super::super::state::{ActiveLaunch, StackCopy};
use super::super::{
    ChangeEnd, ChangeResult, STACK_QUERY_TIMEOUT, attempt_failed, peer_failure,
    precondition_failed, rejected,
};
use super::changed_slots::{
    Change, ChangedSlot, UNDELIVERED_REMEDY, changed_slots, check_not_emptied, deliver_sets,
    holds_any, whole_sets,
};
use super::live::{live_machines, report_offline_sets};
use super::{
    change_active_launch,
    plan::{planned_from, selected_instances},
    stack_list_on,
};
use crate::services::node::{named_instances, stop_named_instance};
use crate::services::response::rephrased;
use config::runtime::{CopyName, Name};
use core_node_api::encoding::{
    LaunchFeedbackStep, LaunchResult, ParticipantInstancesRemoveRequest, StackListResponse,
    StackRemoveGoal,
};
use daemon_config::launcher::{CompositionError, CopyMembership, DeploymentInstance, Placements};
use peppylib::core_node::transport::poll;
use peppylib::messaging::Failure;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::time::Duration;

/// Removing a copy stops its instances one after another, each as a stop
/// does within its host's bound on a stop, the notice of its end included.
pub fn copy_removal_budget(shutdown_grace_secs: u64, instances: usize) -> Duration {
    crate::services::node::node_stop_bound(Duration::from_secs(shutdown_grace_secs))
        .saturating_mul(instances.try_into().unwrap_or(u32::MAX))
}

/// Stops the copy's instances and takes it out of the record. The record
/// follows what runs: once the instances are stopped the copy is gone from
/// it, whether or not the time source could be told. The instances on a
/// machine that is off the federation stay for that machine's `stack
/// reset`, and the copy leaves the record too.
///
/// A cancel stops the removal before its next stop, as a stop that fails
/// does: the copy stays in the record, and `stack remove` stops what is
/// left of it.
pub(in crate::services::stack) async fn remove(
    goal: StackRemoveGoal,
    ctx: StackChangeContext,
) -> ChangeEnd {
    let outcome = change_active_launch(&ctx, async |active| {
        remove_inner(&goal.name, active, &ctx).await
    })
    .await;
    ChangeEnd::without_node_logs(outcome.map(|()| LaunchResult::default()))
}

async fn remove_inner(
    name: &CopyName,
    active: &mut ActiveLaunch,
    ctx: &StackChangeContext,
) -> ChangeResult<()> {
    let copy = active.copies.get(name).cloned().ok_or_else(|| {
        rejected(format!(
            "copy `{name}` is absent; peppy stack list shows the copies on the stack"
        ))
    })?;
    let staying: Vec<_> = active
        .copy_records()
        .filter(|other| other.name != copy.record.name)
        .cloned()
        .collect();
    let remaining = active
        .prepared
        .remove(&active.flat, &copy.record, &active.selection, &staying)
        .map_err(composition_failure)?;
    let shrunk_slots =
        changed_slots(&copy.record, &active.planned, Change::Removal).map_err(rejected)?;
    check_not_emptied(&copy.record, &shrunk_slots, &remaining).map_err(precondition_failed)?;
    let removed: HashSet<_> = copy.record.instance_ids().map(Name::as_str).collect();
    let remaining_planned = planned_from(&remaining, active.resolved.iter());
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
            return Err(precondition_failed(
                consumers.refusal(name, &removed_domains),
            ));
        }
    }
    let ValidatedPlan {
        slot_bindings: bindings,
        observations,
        ..
    } = validate_and_order_dependencies(
        ctx,
        &remaining_planned,
        &active.placements,
        &copies,
        &active.resolved_clocks,
    )
    .await
    .map_err(rejected)?;
    let watchers = lifecycle_watchers(&observations, &active.placements).map_err(rejected)?;
    let repointed = watchers_replacing(&active.watchers, &watchers);
    let live = live_machines(ctx, &active.placements).await?;
    // The machines the removal acts on: the copy's own, those of the sources
    // whose watchers change, and those of the sets it shrinks.
    let machines: BTreeSet<&str> = copy_machines(&copy, &active.placements)
        .into_iter()
        .chain(
            repointed
                .keys()
                .map(|instance| active.placements.of(instance.as_str())),
        )
        .chain(
            shrunk_slots
                .iter()
                .map(|slot| slot.host(&active.placements)),
        )
        .collect();
    let states = MachineStates::read(ctx, &active.launch_id, machines, &live).await?;
    let (reachable_slots, offline_slots): (Vec<ChangedSlot>, Vec<ChangedSlot>) = shrunk_slots
        .into_iter()
        .filter(|slot| !states.is_cleared(slot.host(&active.placements)))
        .partition(|slot| states.stops_on(slot.host(&active.placements)));
    let shrunk_sets = whole_sets(&reachable_slots, &bindings, &observations);
    let touched = removal_deployments(active, &copy, &states, &repointed, &reachable_slots);
    let change = preflight_change(
        ctx,
        &active.launch_id,
        &touched,
        &active.placements,
        Some(&live),
    )
    .await?;
    let outcome = async {
        report_machines_left(ctx, active, &copy, &states).await;
        stop_copy(
            ctx,
            &active.launch_id,
            &copy,
            &active.placements,
            &states,
            StopsOnCancel::End,
        )
        .await?;
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
            publish_warning(
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
        Ok::<_, Failure>(())
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
    fn refusal(&self, source: &CopyName, domains: &[Name]) -> String {
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

/// The machines of a copy: the machine of each of its instances.
pub(super) fn copy_machines<'a>(copy: &StackCopy, placements: &'a Placements) -> BTreeSet<&'a str> {
    copy.record
        .instance_ids()
        .map(|instance| placements.of(instance.as_str()))
        .collect()
}

/// Where a removal finds one machine it acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum MachineState {
    /// This daemon's own machine, which holds the launch's slice.
    Coordinator,
    /// Live, holding the slice of this launch: the copy's instances it holds
    /// stop, each within its host's bound on a stop.
    Holding {
        shutdown_grace_secs: u64,
        held: BTreeSet<String>,
    },
    /// Live, holding no slice of this launch: someone reset it, so nothing of
    /// the launch runs there.
    Cleared,
    /// Not live on the federation.
    Offline,
}

impl MachineState {
    /// The state of a live remote machine whose stack is `listed`.
    fn of_listing(launch_id: &str, listed: &StackListResponse) -> ChangeResult<Self> {
        let holds_slice = listed
            .launch
            .as_ref()
            .is_some_and(|launch| launch.launch_id == launch_id);
        if !holds_slice {
            return Ok(Self::Cleared);
        }
        let graph: core_node_api::SerializedNodeGraph = serde_json::from_str(&listed.graph_json)
            .map_err(|e| {
                attempt_failed(format!(
                    "invalid stack report from `{}`: {e}",
                    listed.core_node
                ))
            })?;
        Ok(Self::Holding {
            shutdown_grace_secs: listed.shutdown_grace_secs,
            held: graph
                .nodes
                .iter()
                .flat_map(|node| &node.instances)
                .map(|instance| instance.instance_id.clone())
                .collect(),
        })
    }
}

/// The state of each machine a removal acts on.
#[derive(Debug, Default)]
pub(super) struct MachineStates(BTreeMap<String, MachineState>);

impl MachineStates {
    /// Reads the state of each of `machines`: this daemon's own, each one
    /// that `live` does not hold, and each remote live one from its stack,
    /// read once. A stack that cannot be read refuses the change before any
    /// machine changes.
    pub(super) async fn read<'a>(
        ctx: &StackChangeContext,
        launch_id: &str,
        machines: impl IntoIterator<Item = &'a str>,
        live: &BTreeSet<String>,
    ) -> ChangeResult<Self> {
        let mut states = BTreeMap::new();
        for machine in machines {
            let state = if machine == ctx.bound_core_node {
                MachineState::Coordinator
            } else if !live.contains(machine) {
                MachineState::Offline
            } else {
                MachineState::of_listing(launch_id, &stack_list_on(ctx, machine).await?)?
            };
            states.insert(machine.to_owned(), state);
        }
        Ok(Self(states))
    }

    /// The state of `machine`; a machine the removal did not read holds
    /// nothing it stops.
    fn of(&self, machine: &str) -> &MachineState {
        self.0.get(machine).unwrap_or(&MachineState::Offline)
    }

    /// Whether the copy's instances on `machine` stop: it is this daemon's,
    /// or live and holding the launch's slice.
    pub(super) fn stops_on(&self, machine: &str) -> bool {
        matches!(
            self.of(machine),
            MachineState::Coordinator | MachineState::Holding { .. }
        )
    }

    fn is_cleared(&self, machine: &str) -> bool {
        matches!(self.of(machine), MachineState::Cleared)
    }
}

/// Removal reserves the machines where the copy's instances stop: their
/// instances, every source on them whose watchers change, and each of
/// `shrunk_slots`, the sets it delivers there. A source on a machine that is
/// not live keeps its stale watchers until that machine is reset, so it
/// does not hold the removal.
fn removal_deployments(
    active: &ActiveLaunch,
    copy: &StackCopy,
    states: &MachineStates,
    repointed: &LifecycleWatchers,
    shrunk_slots: &[ChangedSlot],
) -> Vec<PlannedDeployment> {
    selected_instances(&active.planned, |instance| {
        let id = instance.instance_id.as_str();
        let stops_here = states.stops_on(active.placements.of(id));
        (stops_here
            && (copy.record.owns_instance(id) || repointed.contains_key(&instance.instance_id)))
            || holds_any(shrunk_slots, &instance.instance_id)
    })
}

/// One stop of a copy's removal: an instance of the copy, its machine and
/// how it stops there.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Stop<'a> {
    /// On this daemon's machine, after a line when the stack holds it.
    Local { instance: &'a Name },
    /// On a live machine that holds the launch's slice and the instance,
    /// through one request, after a line.
    Remote {
        instance: &'a Name,
        machine: &'a str,
        shutdown_grace_secs: u64,
    },
}

impl Stop<'_> {
    fn machine<'m>(&'m self, coordinator: &'m str) -> &'m str {
        match self {
            Self::Local { .. } => coordinator,
            Self::Remote { machine, .. } => machine,
        }
    }
}

/// The stops of `copy`'s removal, its instances last started first. An
/// instance on a machine the removal does not stop on, or that a remote
/// machine no longer holds, has no stop.
fn stops<'a>(
    copy: &'a StackCopy,
    placements: &'a Placements,
    states: &'a MachineStates,
    coordinator: &str,
) -> Vec<Stop<'a>> {
    copy.record
        .instances
        .iter()
        .rev()
        .filter_map(|owned| {
            let instance = &owned.instance_id;
            let machine = placements.of(instance.as_str());
            if machine == coordinator {
                return Some(Stop::Local { instance });
            }
            match states.of(machine) {
                MachineState::Holding {
                    shutdown_grace_secs,
                    held,
                } if held.contains(instance.as_str()) => Some(Stop::Remote {
                    instance,
                    machine,
                    shutdown_grace_secs: *shutdown_grace_secs,
                }),
                _ => None,
            }
        })
        .collect()
}

/// The progress of a removal's stops: a stop that fails records one error
/// for its machine, and the later stops on that machine are skipped; the
/// other machines still stop. The errors come back together, with the kind
/// of the first.
#[derive(Debug, Default)]
struct StopWalk<'a> {
    failed: BTreeMap<&'a str, Failure>,
}

impl<'a> StopWalk<'a> {
    /// Whether the stops on `machine` are skipped: one of them failed.
    fn skips(&self, machine: &str) -> bool {
        self.failed.contains_key(machine)
    }

    fn record(&mut self, machine: &'a str, outcome: ChangeResult<()>) {
        if let Err(error) = outcome {
            self.failed.insert(machine, error);
        }
    }

    fn finish(self) -> ChangeResult<()> {
        let failures: Vec<Failure> = self.failed.into_values().collect();
        let Some(first) = failures.first() else {
            return Ok(());
        };
        let message = failures
            .iter()
            .map(Failure::message)
            .collect::<Vec<_>>()
            .join("; ");
        Err(rephrased(first, message))
    }
}

/// What a cancel of the stack goal does to the stops of a copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StopsOnCancel {
    /// A removal ends at the cancel, before its next stop: the copy stays,
    /// as a removal whose stop fails leaves it.
    End,
    /// The undo of a join stops every instance: a cancel is one of the
    /// failures that start the undo.
    Continue,
}

/// Stops the copy's instances, the last started first, one at a time across
/// its machines. On this daemon's machine each stop takes up to the bound on
/// a stop, so the change reports each instance the stack holds before it
/// stops it. On another machine that holds the launch's slice, each instance
/// it holds stops through one request within that machine's bound on a stop,
/// after a line. An instance on a machine that is not live, or that
/// holds no slice of this launch, is not stopped. A stop that fails skips
/// the later stops on its machine, and the others still run. `on_cancel`
/// says whether a cancel of the goal ends the stops before the next one.
pub(super) async fn stop_copy(
    ctx: &StackChangeContext,
    launch_id: &str,
    copy: &StackCopy,
    placements: &Placements,
    states: &MachineStates,
    on_cancel: StopsOnCancel,
) -> ChangeResult<()> {
    let coordinator = ctx.bound_core_node.as_str();
    let planned = stops(copy, placements, states, coordinator);
    let mut walk = StopWalk::default();
    for stop in &planned {
        if on_cancel == StopsOnCancel::End {
            ctx.stop_if_cancelled()?;
        }
        let machine = stop.machine(coordinator);
        if walk.skips(machine) {
            continue;
        }
        let outcome = match stop {
            Stop::Local { instance } => {
                stop_local_instance(ctx, &copy.record.name, instance).await;
                Ok(())
            }
            Stop::Remote {
                instance,
                machine,
                shutdown_grace_secs,
            } => {
                stop_remote_instance(
                    ctx,
                    launch_id,
                    &copy.record.name,
                    instance,
                    machine,
                    *shutdown_grace_secs,
                )
                .await
            }
        };
        walk.record(machine, outcome);
    }
    walk.finish()
}

/// [`stop_copy`] on the machines `live` holds, read again: what a join's
/// rollback stops, every instance of it, a cancel of the goal or not.
pub(super) async fn stop_copy_on_live_machines(
    ctx: &StackChangeContext,
    launch_id: &str,
    copy: &StackCopy,
    placements: &Placements,
    live: &BTreeSet<String>,
) -> ChangeResult<()> {
    let states = MachineStates::read(ctx, launch_id, copy_machines(copy, placements), live).await?;
    stop_copy(
        ctx,
        launch_id,
        copy,
        placements,
        &states,
        StopsOnCancel::Continue,
    )
    .await
}

async fn stop_local_instance(ctx: &StackChangeContext, copy: &CopyName, instance: &Name) {
    for named in named_instances(&ctx.node_stack, std::slice::from_ref(instance)) {
        if named.is_on_stack() {
            publish_stdout(
                ctx,
                format!("Stopping instance `{}` of copy `{copy}`", named.id),
                LaunchFeedbackStep::LauncherStep,
            )
            .await;
        }
        stop_named_instance(
            &ctx.messenger,
            &ctx.bound_core_node,
            &ctx.core_instance_id,
            &ctx.node_stack,
            &ctx.relationships,
            &named,
        )
        .await;
    }
}

async fn stop_remote_instance(
    ctx: &StackChangeContext,
    launch_id: &str,
    copy: &CopyName,
    instance: &Name,
    machine: &str,
    shutdown_grace_secs: u64,
) -> ChangeResult<()> {
    publish_stdout(
        ctx,
        format!("Stopping instance `{instance}` of copy `{copy}` on `{machine}`"),
        LaunchFeedbackStep::LauncherStep,
    )
    .await;
    let instance_ids = core_node_api::encoding::RemovedInstances::try_from(vec![instance.clone()])
        .map_err(|error| {
            attempt_failed(format!(
                "cannot remove `{instance}` on `{machine}`: {error}"
            ))
        })?;
    let removed = poll(
        &ParticipantInstancesRemoveRequest {
            launch_id: launch_id.to_owned(),
            instance_ids,
        },
        &ctx.messenger,
        &ctx.bound_core_node,
        &ctx.core_instance_id,
        machine,
        copy_removal_budget(shutdown_grace_secs, 1).saturating_add(STACK_QUERY_TIMEOUT),
    )
    .await;
    match removed {
        Ok(_) => Ok(()),
        Err(error) => match error.failure() {
            Some(refusal) => Err(rephrased(
                refusal,
                format!(
                    "`{machine}` refused to remove `{instance}`: {}",
                    refusal.message()
                ),
            )),
            None => Err(peer_failure(
                &error,
                format!("cannot remove `{instance}` on `{machine}`: {error}"),
            )),
        },
    }
}

/// Says what the removal leaves on each machine of the copy where nothing
/// stops: a line for a machine that holds no slice of this launch, and a
/// warning for a machine that is not live, naming the copy's instances that
/// stay there and every other instance of this launch that a `peppy stack
/// reset` there also stops.
async fn report_machines_left(
    ctx: &StackChangeContext,
    active: &ActiveLaunch,
    copy: &StackCopy,
    states: &MachineStates,
) {
    for machine in copy_machines(copy, &active.placements) {
        match states.of(machine) {
            MachineState::Cleared => {
                publish_stdout(
                    ctx,
                    format!("`{machine}` holds no instance of this launch, so nothing stops there"),
                    LaunchFeedbackStep::LauncherStep,
                )
                .await;
            }
            MachineState::Offline => {
                publish_warning(
                    ctx,
                    offline_warning(active, copy, machine),
                    LaunchFeedbackStep::LauncherStep,
                )
                .await;
            }
            MachineState::Coordinator | MachineState::Holding { .. } => {}
        }
    }
}

/// The warning for `machine`, which is not live: the copy's instances that
/// stay there, and the other instances of the launch a reset there stops.
fn offline_warning(active: &ActiveLaunch, copy: &StackCopy, machine: &str) -> String {
    let on_machine = |id: &&Name| active.placements.of(id.as_str()) == machine;
    let staying: Vec<&Name> = copy.record.instance_ids().filter(on_machine).collect();
    let others: Vec<&Name> = active
        .flat
        .deployments
        .iter()
        .flat_map(|deployment| &deployment.instances)
        .map(|instance| &instance.instance_id)
        .filter(on_machine)
        .filter(|id| !copy.record.owns_instance(id.as_str()))
        .collect();
    let reset = if others.is_empty() {
        String::new()
    } else {
        format!(
            "; that reset also stops every other instance of this launch on it: {}",
            daemon_config::format_quoted_list(&others)
        )
    };
    format!(
        "`{machine}` is not live on the federation, so the instances of copy `{}` there are not \
         stopped: {}. Clear that machine with `peppy stack reset` from there when it \
         returns{reset}",
        copy.record.name,
        daemon_config::format_quoted_list(&staying)
    )
}

/// The failure of a removal that the launcher's composition refuses: the
/// last copy of a `one_or_more` axis is a copy the stack needs, and any
/// other refusal says that the removal does not fit the launcher.
fn composition_failure(error: CompositionError) -> Failure {
    match error {
        CompositionError::CopyAxisEmptied { .. } => precondition_failed(error),
        other => rejected(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use daemon_config::launcher::{CopyInstance, CopyRecord};

    /// A copy's removal waits a stop's bound per instance, which holds the
    /// notice of the instance's end to the daemons its relations name.
    #[test]
    fn a_copy_removal_waits_a_stop_bound_per_instance() {
        let grace = Duration::from_secs(config::peppy_config::DEFAULT_SHUTDOWN_GRACE_SECS);
        let per_instance = crate::services::node::node_stop_bound(grace);
        assert_eq!(copy_removal_budget(grace.as_secs(), 3), per_instance * 3);
    }

    fn instance(id: &str) -> DeploymentInstance {
        DeploymentInstance::empty(Name::new(id).unwrap())
    }

    /// The instances reading the departing domain, filed under their copy or
    /// the stack, with everything on another clock left out and the refusal
    /// naming the domain and the fix each kind admits.
    #[test]
    fn time_consumers_name_the_copies_to_remove_and_the_stack_to_reset() {
        let alpha = CopyName::new("alpha").unwrap();
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
                    instance_id: config::runtime::instance_id_in_copy(alpha.as_name(), id),
                    in_copy: Name::new(id).unwrap(),
                    own_axis: None,
                })
                .collect(),
            set_members: Vec::new(),
        }]);
        let consumers = ClockConsumers::of(&instances, &clocks, &domains, &copies);
        assert_eq!(consumers.copies, BTreeSet::from([alpha.as_name().clone()]));
        assert_eq!(consumers.stack, [Name::new("shared_inst").unwrap()]);
        let source = CopyName::new("sim").unwrap();
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

    /// A launch whose flat plan runs `instances`, each `(id, machine)`, on the
    /// coordinator `cn-desk` and the machines they name, with the copy
    /// `alpha` owning `copy_instances`, in start order.
    fn launch_with(
        instances: &[(&str, &str)],
        copy_instances: &[&str],
    ) -> (ActiveLaunch, StackCopy) {
        let deployments: Vec<String> = instances
            .iter()
            .map(|(id, _)| {
                format!(
                    r#"{{ source: {{ name: "{id}_node", tag: "v1" }}, instances: [{{ instance_id: "{id}" }}] }}"#
                )
            })
            .collect();
        let document = format!(
            r#"{{ peppy_schema: "launcher/v1", deployments: [{}] }}"#,
            deployments.join(", ")
        );
        let flat = daemon_config::launcher::PeppyLauncherParser::from_content(&document)
            .expect("the plan parses");
        let prepared =
            daemon_config::launcher::PreparedLauncher::load(&flat, std::path::Path::new("x.json5"))
                .expect("it loads");
        let placements = crate::services::stack::fixtures::placements_with(
            "cn-desk",
            &instances
                .iter()
                .filter(|(_, machine)| *machine != "cn-desk")
                .copied()
                .collect::<Vec<_>>(),
        );
        let planned = instances
            .iter()
            .map(|(id, _)| {
                crate::services::stack::fixtures::planned_deployment(
                    &format!("{id}_node"),
                    &[(id, None)],
                )
            })
            .collect();
        let name = CopyName::new("alpha").unwrap();
        let copy = StackCopy {
            record: CopyRecord {
                name: name.clone(),
                axis: "robot".into(),
                option: "station".into(),
                selection: Default::default(),
                instances: copy_instances
                    .iter()
                    .map(|id| CopyInstance {
                        instance_id: Name::new(*id).unwrap(),
                        in_copy: Name::new(*id).unwrap(),
                        own_axis: None,
                    })
                    .collect(),
                set_members: Vec::new(),
            },
            core_node: config::runtime::CoreNodeName::new("cn-desk").unwrap(),
        };
        let active = ActiveLaunch::new(
            "launch-1",
            prepared,
            flat,
            Default::default(),
            placements,
            planned,
        );
        (active, copy)
    }

    fn holding(held: &[&str]) -> MachineState {
        MachineState::Holding {
            shutdown_grace_secs: 5,
            held: held.iter().map(|id| (*id).to_owned()).collect(),
        }
    }

    fn states(entries: &[(&str, MachineState)]) -> MachineStates {
        MachineStates(
            entries
                .iter()
                .map(|(machine, state)| ((*machine).to_owned(), state.clone()))
                .collect(),
        )
    }

    /// A live remote machine holds the launch's slice when its listing names
    /// the launch, with the instances its graph holds and its grace; a
    /// listing of another launch, or of none, is a machine someone reset.
    #[test]
    fn a_listing_tells_a_machine_that_holds_the_slice_from_one_that_was_reset() {
        let graph = crate::services::stack::fixtures::graph_holding(&[(
            "alpha_pad_inst",
            core_node_api::InstanceState::Running,
        )]);
        let mut listed = StackListResponse::new(
            serde_json::to_string(&graph).unwrap(),
            "cn-gpu-box",
            "gen-1",
            "gpu-box",
        )
        .with_launch(core_node_api::encoding::LaunchIdentity::new(
            "launch-1", "cn-desk",
        ));
        listed.shutdown_grace_secs = 7;
        assert_eq!(
            MachineState::of_listing("launch-1", &listed).unwrap(),
            MachineState::Holding {
                shutdown_grace_secs: 7,
                held: BTreeSet::from(["alpha_pad_inst".to_owned()]),
            }
        );
        assert_eq!(
            MachineState::of_listing("launch-2", &listed).unwrap(),
            MachineState::Cleared
        );
        let standalone = StackListResponse::new("{}", "cn-gpu-box", "gen-1", "gpu-box");
        assert_eq!(
            MachineState::of_listing("launch-1", &standalone).unwrap(),
            MachineState::Cleared
        );

        let states = states(&[
            ("cn-desk", MachineState::Coordinator),
            ("cn-gpu-box", holding(&[])),
            ("cn-reset", MachineState::Cleared),
            ("cn-gone", MachineState::Offline),
        ]);
        assert!(states.stops_on("cn-desk") && states.stops_on("cn-gpu-box"));
        assert!(!states.stops_on("cn-reset") && !states.stops_on("cn-gone"));
        assert!(!states.stops_on("cn-unread"));
        assert!(states.is_cleared("cn-reset") && !states.is_cleared("cn-gone"));
    }

    /// The stops run the copy's instances last started first, across its
    /// machines, and leave out what a machine does not stop: a machine that
    /// is not live, one that was reset, and an instance a machine no longer
    /// holds.
    #[test]
    fn the_stops_run_last_started_first_across_machines() {
        let (active, copy) = launch_with(
            &[
                ("alpha_arm_inst", "cn-desk"),
                ("alpha_pad_inst", "cn-gpu-box"),
                ("alpha_watch_inst", "cn-desk"),
            ],
            &["alpha_arm_inst", "alpha_pad_inst", "alpha_watch_inst"],
        );
        let ids = |stops: Vec<Stop<'_>>| -> Vec<String> {
            stops
                .iter()
                .map(|stop| match stop {
                    Stop::Local { instance } => format!("{instance}@local"),
                    Stop::Remote {
                        instance, machine, ..
                    } => format!("{instance}@{machine}"),
                })
                .collect()
        };
        let all = states(&[
            ("cn-desk", MachineState::Coordinator),
            ("cn-gpu-box", holding(&["alpha_pad_inst"])),
        ]);
        assert_eq!(
            ids(stops(&copy, &active.placements, &all, "cn-desk")),
            [
                "alpha_watch_inst@local",
                "alpha_pad_inst@cn-gpu-box",
                "alpha_arm_inst@local"
            ]
        );
        for state in [
            MachineState::Offline,
            MachineState::Cleared,
            holding(&["bravo_pad_inst"]),
        ] {
            let left = states(&[
                ("cn-desk", MachineState::Coordinator),
                ("cn-gpu-box", state),
            ]);
            assert_eq!(
                ids(stops(&copy, &active.placements, &left, "cn-desk")),
                ["alpha_watch_inst@local", "alpha_arm_inst@local"]
            );
        }
    }

    /// A stop that fails skips the later stops on its machine, the other
    /// machines still stop, and every error comes back.
    #[test]
    fn a_failed_machine_skips_its_later_stops_and_the_others_still_run() {
        let machines = [
            "cn-gpu-box",
            "cn-desk",
            "cn-gpu-box",
            "cn-jetson",
            "cn-desk",
        ];
        let mut walk = StopWalk::default();
        let mut ran = Vec::new();
        for (index, machine) in machines.into_iter().enumerate() {
            if walk.skips(machine) {
                continue;
            }
            ran.push(index);
            let outcome = match machine {
                "cn-gpu-box" => Err(precondition_failed("`cn-gpu-box` refused")),
                "cn-jetson" => Err(crate::services::stack::resource_unavailable(
                    "cannot reach `cn-jetson`",
                )),
                _ => Ok(()),
            };
            walk.record(machine, outcome);
        }
        assert_eq!(
            ran,
            [0, 1, 3, 4],
            "the second stop on cn-gpu-box is skipped"
        );
        let failure = walk.finish().unwrap_err();
        assert_eq!(
            failure.message(),
            "`cn-gpu-box` refused; cannot reach `cn-jetson`"
        );
        assert_eq!(
            failure.kind().as_str(),
            "precondition_failed",
            "the failure has the kind of the first machine's"
        );
        assert!(StopWalk::default().finish().is_ok());
    }

    /// The reservation holds the copy's instances and the sources whose
    /// watchers change on the machines where the copy stops, and nothing on a
    /// machine that is not live or holds no slice of the launch.
    #[test]
    fn the_reservation_holds_nothing_on_a_machine_that_does_not_stop() {
        let (active, copy) = launch_with(
            &[
                ("alpha_arm_inst", "cn-desk"),
                ("alpha_pad_inst", "cn-gpu-box"),
                ("monitor_inst", "cn-desk"),
                ("remote_monitor_inst", "cn-gpu-box"),
                ("reset_monitor_inst", "cn-reset"),
            ],
            &["alpha_arm_inst", "alpha_pad_inst"],
        );
        let repointed: LifecycleWatchers =
            ["monitor_inst", "remote_monitor_inst", "reset_monitor_inst"]
                .into_iter()
                .map(|id| (Name::new(id).unwrap(), BTreeSet::new()))
                .collect();
        let reserved = |states: &MachineStates| -> Vec<String> {
            removal_deployments(&active, &copy, states, &repointed, &[])
                .iter()
                .flat_map(|item| &item.deployment.instances)
                .map(|instance| instance.instance_id.to_string())
                .collect()
        };
        let offline = states(&[
            ("cn-desk", MachineState::Coordinator),
            ("cn-gpu-box", MachineState::Offline),
            ("cn-reset", MachineState::Cleared),
        ]);
        assert_eq!(reserved(&offline), ["alpha_arm_inst", "monitor_inst"]);
        let live = states(&[
            ("cn-desk", MachineState::Coordinator),
            ("cn-gpu-box", holding(&["alpha_pad_inst"])),
            ("cn-reset", MachineState::Cleared),
        ]);
        assert_eq!(
            reserved(&live),
            [
                "alpha_arm_inst",
                "alpha_pad_inst",
                "monitor_inst",
                "remote_monitor_inst"
            ]
        );
    }

    /// The warning for a machine that is not live names the copy's
    /// instances that stay there, and every other instance of the launch a
    /// reset there also stops: the parts of other robots and the stack's.
    #[test]
    fn the_warning_names_what_stays_and_what_a_reset_also_stops() {
        let (active, copy) = launch_with(
            &[
                ("alpha_arm_inst", "cn-desk"),
                ("alpha_brain_inst", "cn-gpu-box"),
                ("bravo_brain_inst", "cn-gpu-box"),
                ("detector_inst", "cn-gpu-box"),
            ],
            &["alpha_arm_inst", "alpha_brain_inst"],
        );
        assert_eq!(
            offline_warning(&active, &copy, "cn-gpu-box"),
            "`cn-gpu-box` is not live on the federation, so the instances of copy `alpha` there \
             are not stopped: `alpha_brain_inst`. Clear that machine with `peppy stack reset` \
             from there when it returns; that reset also stops every other instance of this \
             launch on it: `bravo_brain_inst`, `detector_inst`"
        );
        let (alone, copy) = launch_with(
            &[
                ("alpha_arm_inst", "cn-desk"),
                ("alpha_brain_inst", "cn-gpu-box"),
            ],
            &["alpha_arm_inst", "alpha_brain_inst"],
        );
        assert!(
            offline_warning(&alone, &copy, "cn-gpu-box").ends_with("from there when it returns"),
            "a machine with nothing else of the launch names nothing else"
        );
    }
}
