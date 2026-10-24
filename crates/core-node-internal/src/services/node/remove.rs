use crate::Result;
use crate::services::response::{failure, into_service_answer};
use config::failure_kind::PeppyFailureKind;
use config::runtime::Name;
use core_node_api::ServiceId;
use core_node_api::encoding::{NodeRemoveRequest, NodeRemoveResponse};
use core_node_api::names;
use node_stack::{
    EntityHandle, KeptRelations, NodeEntity, NodeStack, NodeStackError, ProcessClaim, SlotLeft,
    SlotRemoval,
};
use peppylib::messaging::SenderTarget;
use peppylib::messaging::{
    Failure, SHUTDOWN_SERVICE, ServiceAnswer, ServiceMessenger, ServiceRequestContext,
    ServiceTarget,
};
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyError};
use std::sync::Arc;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use super::RelationshipCoordinators;
use super::stop::{DoomedInstance, SHUTDOWN_TIMEOUT, StopOutcome, await_exit_then_force_kill};

pub async fn listen_for_node_remove(
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    relationships: RelationshipCoordinators,
    ownership: Arc<crate::services::federation::SliceOwnership>,
) -> Result<JoinHandle<Result<()>>> {
    let core_node_name = core_node_name.to_string();
    let core_instance_id = instance_id.to_string();
    let messenger = messenger.clone();

    let mut endpoint = ServiceMessenger::listen(
        &messenger,
        &core_node_name,
        &core_instance_id,
        SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
        ServiceId::NodeRemove.name(),
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(|context| {
                handle_node_remove_request(
                    context,
                    messenger.clone(),
                    core_node_name.clone(),
                    core_instance_id.clone(),
                    Arc::clone(&node_stack),
                    relationships.clone(),
                    ownership.clone(),
                )
            })
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_node_remove_request(
    context: ServiceRequestContext,
    messenger: MessengerHandle,
    core_node_name: String,
    core_instance_id: String,
    node_stack: Arc<NodeStack>,
    relationships: RelationshipCoordinators,
    ownership: Arc<crate::services::federation::SliceOwnership>,
) -> ServiceAnswer {
    let request = NodeRemoveRequest::decode(context.message().payload_bytes().as_ref())
        .map_err(|error| failure(PeppyFailureKind::Rejected, error))?;
    let _admission = ownership.admit_node_goal(&request)?;
    // An instance whose restart falls due during this removal is not started
    // again before the removal claims its slot; a removal that fails releases
    // its hold, and the policies apply once the notes of its shutdown
    // requests are withdrawn.
    let _restarts_held = ownership.hold_restarts();
    let outcome = crate::services::node::gate::finish_on_reset(
        handle_node_remove_request_inner(
            &context,
            &request,
            &messenger,
            &core_node_name,
            &core_instance_id,
            Arc::clone(&node_stack),
            &relationships,
        ),
        &ownership.stack.cancellation(),
        crate::services::node::gate::reset_drain_budget(node_stack.shutdown_grace()),
        || {
            Err(RemoveFailure::new(
                PeppyFailureKind::PreconditionFailed,
                "node remove cancelled by stack reset",
            ))
        },
    )
    .await;
    let force_killed = outcome.map_err(RemoveFailure::into_failure)?;
    into_service_answer(
        NodeRemoveResponse::new(force_killed)
            .encode()
            .map_err(Into::into),
    )
}

/// Why a node remove failed: its failure kind, its message, and the
/// instances its force phase killed before the failure, which the error
/// reply names so the CLI user learns of each.
struct RemoveFailure {
    kind: PeppyFailureKind,
    message: String,
    force_killed: Vec<String>,
}

impl RemoveFailure {
    fn new(kind: PeppyFailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            force_killed: Vec::new(),
        }
    }

    /// The error reply of the failure; its message names each instance the
    /// force phase killed.
    fn into_failure(self) -> Failure {
        if self.force_killed.is_empty() {
            return failure(self.kind, self.message);
        }
        failure(
            self.kind,
            format!(
                "{}; the removal force-killed {}, which did not exit within the shutdown grace",
                self.message,
                self.force_killed.join(", ")
            ),
        )
    }
}

async fn handle_node_remove_request_inner(
    context: &ServiceRequestContext,
    request: &NodeRemoveRequest,
    messenger: &MessengerHandle,
    core_node_name: &str,
    core_instance_id: &str,
    node_stack: Arc<NodeStack>,
    relationships: &RelationshipCoordinators,
) -> std::result::Result<Vec<String>, RemoveFailure> {
    let sender_instance_id = context.message().instance_id();

    debug!(
        "Received `node_remove` request from {sender_instance_id}, node_name={}, tag={}, stop_instances={}",
        request.node_name, request.tag, request.stop_instances
    );

    let root_handle = node_stack.root();
    let (root_node_name, root_node_tag) = {
        let guard = root_handle.read();
        (
            guard.config().manifest.name.as_str().to_owned(),
            guard.config().manifest.tag.clone(),
        )
    };
    if request.node_name == root_node_name && request.tag == root_node_tag {
        return Err(RemoveFailure::new(
            PeppyFailureKind::Rejected,
            "Cannot remove the core node from the node stack",
        ));
    }

    let matching_entity = node_stack.snapshot().into_iter().find(|handle| {
        let guard = handle.read();
        guard.config().manifest.name.as_str() == request.node_name
            && guard.config().manifest.tag == request.tag
    });
    let Some(matching_entity) = matching_entity else {
        return Err(RemoveFailure::new(
            PeppyFailureKind::Rejected,
            format!(
                "Node '{}:{}' not found in node stack",
                request.node_name, request.tag
            ),
        ));
    };

    // `targets` are the `Running` instances: each gets a reachability probe, a
    // reachable one a cooperative shutdown request, and each is claimed and
    // stopped below. `targets_without_process` are the `Restarting`,
    // `Finished` and `Failed` instances, which have no process to signal but
    // are tracked, so the safety gate counts them and each is cleared before
    // `remove_config`. `starting` are the starts in flight, each with the
    // start it was collected as, which get no shutdown request: under
    // `stop_instances` the claim below clears one whose start ended and
    // leaves one still starting, which keeps the node.
    let (targets, targets_without_process, starting) = {
        let guard = matching_entity.read();
        let mut targets: Vec<DoomedInstance> = Vec::new();
        let mut targets_without_process: Vec<DoomedInstance> = Vec::new();
        let mut starting: Vec<(Name, SlotLeft)> = Vec::new();
        for instance in guard.instances() {
            let instance_id = instance.instance_id().clone();
            match instance.state() {
                node_stack::InstanceState::Running => {
                    targets.push(DoomedInstance::of(&guard, instance_id, instance.pid()));
                }
                node_stack::InstanceState::Starting => {
                    let collected_as = SlotLeft::Starting {
                        restart: instance.starts_a_restart(),
                    };
                    starting.push((instance_id, collected_as));
                }
                node_stack::InstanceState::Restarting
                | node_stack::InstanceState::Finished
                | node_stack::InstanceState::Failed => {
                    targets_without_process.push(DoomedInstance::of(&guard, instance_id, None));
                }
            }
        }
        (targets, targets_without_process, starting)
    };

    // Classify each tracked instance as reachable, unreachable, or probe-failed.
    // `running_targets` drives the actual shutdown loop below (only reachable
    // instances can accept the shutdown RPC). `unreachable_targets` is tracked
    // separately so the safety gate can decide what to do about them without
    // silently dropping their existence.
    // Probe all instances in parallel so overall latency is bounded by the
    // slowest probe rather than the sum.
    let reachability: Vec<std::result::Result<bool, PeppyError>> =
        futures::future::join_all(targets.iter().map(|target| {
            let producer =
                peppylib::messaging::ProducerRef::new(core_node_name, target.instance_id.as_str());
            async move {
                ServiceMessenger::is_reachable(
                    messenger,
                    core_node_name,
                    core_instance_id,
                    SenderTarget::node_from_validated(&target.node_name, &target.node_tag),
                    SHUTDOWN_SERVICE,
                    ServiceTarget::Producer(&producer),
                )
                .await
            }
        }))
        .await;

    let mut running_targets: Vec<DoomedInstance> = Vec::new();
    let mut unreachable_targets: Vec<DoomedInstance> = Vec::new();
    for (target, reachable) in targets.iter().zip(reachability) {
        let reachable = match reachable {
            Ok(r) => r,
            Err(e) => {
                return Err(RemoveFailure::new(
                    PeppyFailureKind::AttemptFailed,
                    format!(
                        "Failed to check shutdown service for instance '{}': {}",
                        target.instance_id.as_str(),
                        e
                    ),
                ));
            }
        };
        if reachable {
            running_targets.push(target.clone());
        } else {
            unreachable_targets.push(target.clone());
        }
    }

    // Safety gate: without `stop_instances`, a running instance or one without
    // a process blocks the removal, since `remove_config` rejects a node that
    // still tracks an instance. A start in flight does not block it here: under
    // `stop_instances` the claim below reads how it ended, and otherwise its
    // slot fails `remove_config`.
    if !request.stop_instances
        && let Some(example) = targets.iter().chain(&targets_without_process).next()
    {
        return Err(RemoveFailure::new(
            PeppyFailureKind::PreconditionFailed,
            format!(
                "Node '{}:{}' has tracked instances (e.g. '{}'); {} clears them before removing",
                request.node_name,
                request.tag,
                example.instance_id.as_str(),
                removal_command(&request.node_name, &request.tag, core_node_name),
            ),
        ));
    }

    // We proceed despite unreachable instances, which get no shutdown
    // request (a container gets a SIGTERM) and are killed once the force-kill
    // deadline passes, and log a warning per instance so the divergence is
    // never silent.
    for target in &unreachable_targets {
        warn!(
            "Instance '{}' of node '{}:{}' is tracked but its shutdown service is unreachable; \
             it gets no shutdown request, a container a SIGTERM, and what is left of it is \
             killed once the force-kill deadline passes",
            target.instance_id.as_str(),
            target.node_name,
            target.node_tag,
        );
    }

    // An instance that exits while this removal stops it ends there, whatever
    // its policy, since this removal removes it next; one the removal leaves
    // keeps its policy once the notes are withdrawn.
    let mut noted_shutdowns = NotedShutdownRequests::note(
        Arc::clone(&matching_entity),
        targets
            .iter()
            .filter_map(|target| Some((target.instance_id.clone(), target.pid?)))
            .collect(),
    );
    for target in &running_targets {
        debug!(
            "Stopping node instance '{}' before removal",
            target.instance_id.as_str()
        );
    }

    // Shut down instances concurrently. Overall wall-clock latency is
    // bounded by the slowest shutdown (up to SHUTDOWN_TIMEOUT) rather than
    // the sum.
    let shutdown_results = futures::future::join_all(running_targets.iter().map(|target| {
        let producer =
            peppylib::messaging::ProducerRef::new(core_node_name, target.instance_id.as_str());
        async move {
            ServiceMessenger::poll(
                messenger,
                core_node_name,
                core_instance_id,
                SenderTarget::node_from_validated(&target.node_name, &target.node_tag),
                SHUTDOWN_SERVICE,
                ServiceTarget::Producer(&producer),
                Payload::from_static(b"shutdown"),
                SHUTDOWN_TIMEOUT,
            )
            .await
        }
    }))
    .await;
    let (answered, failed): (Vec<_>, Vec<_>) = running_targets
        .iter()
        .zip(shutdown_results)
        .partition(|(_, res)| res.is_ok());
    if let Some((target, Err(e))) = failed.into_iter().next() {
        // The instances that answered exit on their request and end there
        // within the force-kill deadline a stop gives them, so their notes
        // stand that long after the removal fails.
        let standing =
            noted_shutdowns.split_off(answered.iter().map(|(target, _)| &target.instance_id));
        let deadline = super::stop::force_kill_deadline(node_stack.shutdown_grace());
        tokio::spawn(async move {
            tokio::time::sleep(deadline).await;
            drop(standing);
        });
        return Err(RemoveFailure::new(
            PeppyFailureKind::AttemptFailed,
            format!(
                "Failed to stop node instance '{}': {}",
                target.instance_id.as_str(),
                e
            ),
        ));
    }

    // Each slot still running the process this removal stops is claimed, so
    // its exit is this removal's, as a stop's is. One that exited already had
    // its groups killed by its exit watcher and is cleared below, whatever
    // state its exit left it in; one another stop path claimed (a concurrent
    // `peppy node stop`) is that path's to kill, and the removal waits for
    // it. The force phase then runs while each claimed slot still holds its
    // id.
    let (claimed, claimed_elsewhere) = {
        let guard = matching_entity.read();
        let mut claimed = Vec::new();
        let mut claimed_elsewhere = Vec::new();
        for target in &targets {
            let Some(pid) = target.pid else {
                continue;
            };
            match guard.claim_process(&target.instance_id, pid) {
                ProcessClaim::Claimed => claimed.push(target.clone()),
                ProcessClaim::ClaimedElsewhere => claimed_elsewhere.push(target.clone()),
                ProcessClaim::Gone => {}
            }
        }
        (claimed, claimed_elsewhere)
    };
    let no_shutdown: Vec<&DoomedInstance> = claimed
        .iter()
        .filter(|target| {
            unreachable_targets
                .iter()
                .any(|unreachable| unreachable.instance_id == target.instance_id)
        })
        .collect();
    let outcomes = await_exit_then_force_kill(
        &claimed,
        &no_shutdown,
        &claimed_elsewhere,
        node_stack.shutdown_grace(),
    )
    .await;
    let force_killed: Vec<String> = claimed
        .iter()
        .zip(outcomes)
        .filter(|(_, outcome)| *outcome == StopOutcome::ForceKilled)
        .map(|(target, _)| target.instance_id.as_str().to_owned())
        .collect();

    // Clear every tracked instance from its entity before `remove_config`:
    // the ones just stopped, the unreachable ones, and the ones without a
    // process. Each is claimed and removed in one write-lock section, so a
    // restart it awaits is cancelled. One running a process this removal did
    // not stop is left to that process and keeps the node.
    let mut cleared: Vec<(&Name, KeptRelations)> = Vec::new();
    let mut left: Vec<(Name, SlotLeft)> = Vec::new();
    for target in targets.iter().chain(&targets_without_process) {
        match NodeEntity::claim_and_remove(&matching_entity, &target.instance_id, target.pid) {
            SlotRemoval::Removed(kept) => cleared.push((&target.instance_id, kept)),
            SlotRemoval::Left(why) => left.push((target.instance_id.clone(), why)),
            SlotRemoval::Untracked => {
                // Instance was concurrently removed; treat as success.
                debug!(
                    "Node instance '{}' already absent; skipping",
                    target.instance_id.as_str()
                );
                cleared.push((&target.instance_id, KeptRelations::default()));
            }
        }
    }
    // Under `stop_instances`, a start that ended since it was collected is
    // cleared like any other instance (a restart's failed start is
    // `restarting` again), and one still starting, or running the process its
    // start committed, keeps the node. A first start that failed took its slot
    // with it and its own abort tore it down, so nothing is left to tear down.
    // Without `stop_instances` a start is not claimed: it keeps the node as
    // the start it was collected as.
    for (instance_id, collected_as) in &starting {
        if !request.stop_instances {
            left.push((instance_id.clone(), *collected_as));
            continue;
        }
        match NodeEntity::claim_and_remove(&matching_entity, instance_id, None) {
            SlotRemoval::Removed(kept) => cleared.push((instance_id, kept)),
            SlotRemoval::Left(why) => left.push((instance_id.clone(), why)),
            SlotRemoval::Untracked => {}
        }
    }

    // The removed instances are torn down together, so their ends are
    // noticed within one notice's bound.
    futures::future::join_all(
        cleared.into_iter().map(|(instance_id, kept)| {
            relationships.tear_down_instance(instance_id.as_str(), kept)
        }),
    )
    .await;

    match node_stack.remove_config(&request.node_name, &request.tag) {
        Ok(true) => {}
        Ok(false) => {
            // Concurrently removed; treat as success.
            debug!(
                "Node '{}:{}' already absent from node stack during remove_config",
                request.node_name, request.tag
            );
        }
        Err(e) => {
            // An instance that started while the remove stopped the others
            // keeps the node in the stack.
            let kind = match e {
                NodeStackError::CannotRemoveNodeWithInstances { .. } => {
                    PeppyFailureKind::PreconditionFailed
                }
                _ => PeppyFailureKind::AttemptFailed,
            };
            return Err(RemoveFailure {
                kind,
                message: format!(
                    "Failed to remove node config '{}:{}': {}{}",
                    request.node_name,
                    request.tag,
                    e,
                    left_instances_tail(&left, &request.node_name, &request.tag, core_node_name)
                ),
                force_killed,
            });
        }
    }

    Ok(force_killed)
}

/// The shutdown notes a removal sets on the slots of the running instances it
/// stops, an unreachable one included: while a note stands, an exit ends the
/// instance whatever its policy. Dropping it withdraws the notes it still
/// holds, so an instance the removal leaves keeps its policy; a slot the
/// removal removed is gone by then. Each note is for the process the
/// removal stops, so a note withdrawn after that process ended leaves a new
/// instance under the id alone.
struct NotedShutdownRequests {
    entity: EntityHandle,
    processes: Vec<(Name, u32)>,
}

impl NotedShutdownRequests {
    fn note(entity: EntityHandle, processes: Vec<(Name, u32)>) -> Self {
        let mut guard = entity.write();
        for (instance_id, pid) in &processes {
            guard.note_shutdown_request(instance_id, *pid);
        }
        drop(guard);
        Self { entity, processes }
    }

    /// Moves the notes of `answered` into requests of their own, which stand
    /// until the caller drops them.
    fn split_off<'a>(&mut self, answered: impl Iterator<Item = &'a Name>) -> Self {
        let answered: Vec<&Name> = answered.collect();
        let (answered_notes, rest) = std::mem::take(&mut self.processes)
            .into_iter()
            .partition(|(instance_id, _)| answered.contains(&instance_id));
        self.processes = rest;
        Self {
            entity: Arc::clone(&self.entity),
            processes: answered_notes,
        }
    }
}

impl Drop for NotedShutdownRequests {
    fn drop(&mut self) {
        let mut guard = self.entity.write();
        for (instance_id, pid) in &self.processes {
            guard.withdraw_shutdown_request(instance_id, *pid);
        }
    }
}

/// The tail of a removal's failure on the daemon `core_node`: each instance
/// the removal left in place, a restart's start with the stop that ends it,
/// and the command to run again, after the starts end when one is starting;
/// empty when it left none.
fn left_instances_tail(
    left: &[(Name, SlotLeft)],
    node_name: &str,
    node_tag: &str,
    core_node: &str,
) -> String {
    if left.is_empty() {
        return String::new();
    }
    let named = left
        .iter()
        .map(|(instance_id, why)| match why {
            SlotLeft::Starting { restart } => {
                super::gate::starting_instance(instance_id, *restart, core_node)
            }
            SlotLeft::RunsAnotherProcess => format!(
                "`{}` runs a process this removal did not stop",
                instance_id.as_str()
            ),
        })
        .collect::<Vec<_>>()
        .join("; ");
    let when = if left
        .iter()
        .any(|(_, why)| matches!(why, SlotLeft::Starting { .. }))
    {
        " once none is starting"
    } else {
        ""
    };
    format!(
        "; {named}; run {} again{when}",
        removal_command(node_name, node_tag, core_node)
    )
}

/// The command that removes `node_name:node_tag` from the daemon `core_node`
/// with its instances.
fn removal_command(node_name: &str, node_tag: &str, core_node: &str) -> String {
    format!("`peppy node remove {node_name}:{node_tag} --stop-instances --core-node {core_node}`")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The failure names a start in flight, a restart's with the stop that
    /// ends it, and a process the removal did not stop, and says what to run
    /// again on the daemon.
    #[test]
    fn a_failure_names_the_instances_it_left() {
        let left = [
            (
                Name::new("arm_inst").unwrap(),
                SlotLeft::Starting { restart: false },
            ),
            (
                Name::new("grip_inst").unwrap(),
                SlotLeft::Starting { restart: true },
            ),
            (Name::new("cam_inst").unwrap(), SlotLeft::RunsAnotherProcess),
        ];
        assert_eq!(
            left_instances_tail(&left, "arm", "v1", "cn-a"),
            "; `arm_inst` is starting; `grip_inst` is starting again under its restart policy, \
             and `peppy node stop grip_inst --core-node cn-a` ends it and clears the instance; \
             `cam_inst` runs a process this removal did not stop; run `peppy node remove \
             arm:v1 --stop-instances --core-node cn-a` again once none is starting"
        );
        assert_eq!(
            left_instances_tail(&left[2..], "arm", "v1", "cn-a"),
            "; `cam_inst` runs a process this removal did not stop; run `peppy node remove \
             arm:v1 --stop-instances --core-node cn-a` again"
        );
        assert_eq!(left_instances_tail(&[], "arm", "v1", "cn-a"), "");
    }
}
