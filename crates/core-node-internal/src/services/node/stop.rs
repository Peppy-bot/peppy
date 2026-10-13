use crate::Result;
use crate::services::response::into_service_response;
use config::runtime::Name;
use core_node_api::ServiceId;
use core_node_api::encoding::{NodeStopRequest, NodeStopResponse};
use core_node_api::names;
use node_stack::{
    EntityHandle, GUEST_FORCE_KILL_BUDGET, InstanceState, KeptRelations, NodeEntity, NodeStack,
    ProcessClaim, SlotRemoval, StartingClaim, TrackedNodeInstance, kill_guest_groups,
    kill_process_group, terminate_process_group,
};
use peppylib::messaging::SenderTarget;
use peppylib::messaging::{
    SHUTDOWN_SERVICE, ServiceMessenger, ServiceRequestContext, ServiceTarget,
};
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyResult};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use super::RelationshipCoordinators;

/// How long a shutdown request waits for a node's answer.
pub(super) const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(100);

pub async fn listen_for_node_stop(
    messenger: &MessengerHandle,
    core_node_node: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    relationships: RelationshipCoordinators,
    ownership: Arc<crate::services::federation::SliceOwnership>,
) -> Result<JoinHandle<Result<()>>> {
    let core_node_node = core_node_node.to_string();
    let core_instance_id = instance_id.to_string();
    let messenger = messenger.clone();

    let mut endpoint = ServiceMessenger::listen(
        &messenger,
        &core_node_node,
        &core_instance_id,
        SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
        ServiceId::NodeStop.name(),
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(|context| {
                handle_node_stop_request(
                    context,
                    messenger.clone(),
                    core_node_node.clone(),
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

async fn handle_node_stop_request(
    context: ServiceRequestContext,
    messenger: MessengerHandle,
    core_node_node: String,
    core_instance_id: String,
    node_stack: Arc<NodeStack>,
    relationships: RelationshipCoordinators,
    ownership: Arc<crate::services::federation::SliceOwnership>,
) -> PeppyResult<Payload> {
    let _admission = match ownership.admit_node_change(None) {
        Ok(admission) => admission,
        Err(reason) => {
            return into_service_response(
                &context,
                NodeStopResponse::failure(reason)
                    .encode()
                    .map_err(Into::into),
            );
        }
    };
    into_service_response(
        &context,
        crate::services::node::gate::finish_on_reset(
            async {
                handle_node_stop_request_inner(
                    &context,
                    &messenger,
                    &core_node_node,
                    &core_instance_id,
                    Arc::clone(&node_stack),
                    &relationships,
                )
                .await?
                .encode()
                .map_err(Into::into)
            },
            &ownership.stack.cancellation(),
            crate::services::node::gate::reset_drain_budget(node_stack.shutdown_grace()),
            || {
                NodeStopResponse::failure("node stop cancelled by stack reset")
                    .encode()
                    .map_err(Into::into)
            },
        )
        .await,
    )
}

async fn handle_node_stop_request_inner(
    context: &ServiceRequestContext,
    messenger: &MessengerHandle,
    core_node_node: &str,
    core_instance_id: &str,
    node_stack: Arc<NodeStack>,
    relationships: &RelationshipCoordinators,
) -> Result<NodeStopResponse> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload_bytes();

    let request = NodeStopRequest::decode(payload.as_ref())?;

    debug!(
        "Received `node_stop` request from {sender_instance_id}, instance_id={}",
        request.instance_id
    );

    // Parse the instance_id string to a Name
    let instance_id = match Name::new(&request.instance_id) {
        Ok(name) => name,
        Err(e) => {
            return Ok(NodeStopResponse::failure(format!(
                "Invalid instance_id: {}",
                e
            )));
        }
    };

    let (kept, response) = loop {
        match act_on_slot(&node_stack, &instance_id).await {
            SlotStep::Refused(refusal) => return Ok(refusal.response(&instance_id)),
            SlotStep::Cleared(kept) => break (Some(kept), NodeStopResponse::success()),
            SlotStep::Stop { pid, entity } => {
                break stop_process(
                    messenger,
                    core_node_node,
                    core_instance_id,
                    &node_stack,
                    &instance_id,
                    pid,
                    &entity,
                )
                .await;
            }
            SlotStep::ReadAgain => continue,
        }
    };

    // This stop tears down only what its claim took: a slot it removed, or a
    // start it claimed, once that start's abort ended it.
    if let Some(kept) = kept {
        relationships
            .tear_down_instance(instance_id.as_str(), kept)
            .await;
    }
    Ok(response)
}

/// Stops the process `pid` (`None` for a fixture slot) of `instance_id` of
/// `entity_handle`, whose slot this stop or another stop path claimed, then
/// removes the slot through [`NodeEntity::claim_and_remove`], returning what
/// the removed slot kept of its relations and the answer. Nothing is removed
/// when another stop path removed the slot meanwhile, or a new run under the
/// id runs its own process.
async fn stop_process(
    messenger: &MessengerHandle,
    core_node_node: &str,
    core_instance_id: &str,
    node_stack: &Arc<NodeStack>,
    instance_id: &Name,
    pid: Option<u32>,
    entity_handle: &EntityHandle,
) -> (Option<KeptRelations>, NodeStopResponse) {
    // Resolve everything we need from the entity up front, under a short-lived
    // read lock that is NOT held across any await. The slot was claimed before
    // this, so its exit watcher leaves the exit to the stop paths.
    let target = DoomedInstance::of(&entity_handle.read(), instance_id.clone(), pid);

    // Cooperative-then-force, identical to the SIGINT teardown for this one
    // instance: ask the node to shut down, give it a bounded grace window, then
    // SIGKILL its process group if it ignored us, and wait for it to be gone.
    // A stuck/non-responsive node is now force-killed (and reported as success)
    // rather than left alive behind a timeout error. We deliberately keep the
    // live registry entry until after the kill+reap so a retried or racing
    // caller can still observe the in-flight shutdown.
    let outcome = force_stop_instance(
        messenger,
        core_node_node,
        core_instance_id,
        &target,
        node_stack.shutdown_grace(),
    )
    .await;

    // Process is gone (properly or improperly): finalize the registry
    // removal; the handler tears the instance down.
    let kept = match NodeEntity::claim_and_remove(entity_handle, instance_id, pid) {
        SlotRemoval::Removed(kept) => Some(kept),
        SlotRemoval::Left(_) | SlotRemoval::Untracked => None,
    };

    // Tell the caller whether the node exited gracefully or had to be
    // force-killed, so the CLI can warn the user about the latter.
    let response = match outcome {
        StopOutcome::ForceKilled => NodeStopResponse::success_force_killed(),
        StopOutcome::Graceful | StopOutcome::NoProcess => NodeStopResponse::success(),
    };
    (kept, response)
}

/// What a stop does with the slot it read under the id.
enum SlotStep {
    /// The stop is refused.
    Refused(Refusal),
    /// The slot is gone, claimed with the relations it kept.
    Cleared(KeptRelations),
    /// The running instance this stop or another stop path claimed is
    /// stopped: its process `pid`, `None` for a fixture slot without one.
    Stop {
        pid: Option<u32>,
        entity: EntityHandle,
    },
    /// The slot's state moved on between the read and the act.
    ReadAgain,
}

/// Reads the slot of `instance_id` and acts by its state, claiming it only
/// while it is still in that state. A running instance is claimed with the
/// process it runs and stopped. An instance without a process is claimed and
/// removed in one step, so a restart that fires meanwhile finds no slot. A
/// restart's start in flight is claimed, which cancels it: its slot leaves
/// with the start, or reaches `running` when the start committed first, and
/// that process is stopped. A first start is refused.
async fn act_on_slot(node_stack: &NodeStack, instance_id: &Name) -> SlotStep {
    let Some((instance, entity_handle)) =
        node_stack.find_instance_and_entity_any_state(instance_id)
    else {
        return SlotStep::Refused(Refusal::NotFound);
    };
    if Arc::ptr_eq(&entity_handle, &node_stack.root()) {
        return SlotStep::Refused(Refusal::CoreNode);
    }
    match instance.state() {
        InstanceState::Running => match instance.pid() {
            Some(pid) => claim_running(entity_handle, instance_id, pid),
            None => clear_without_process(&entity_handle, instance_id),
        },
        InstanceState::Restarting | InstanceState::Finished | InstanceState::Failed => {
            clear_without_process(&entity_handle, instance_id)
        }
        InstanceState::Starting => {
            let claim = entity_handle.read().claim_starting(instance_id);
            match claim {
                StartingClaim::Claimed(kept) => {
                    match wait_for_claimed_start(node_stack, instance_id).await {
                        ClaimedStart::Ended => SlotStep::Cleared(kept),
                        ClaimedStart::Committed { pid } => SlotStep::Stop {
                            pid,
                            entity: entity_handle,
                        },
                    }
                }
                StartingClaim::FirstStart => SlotStep::Refused(Refusal::FirstStart),
                StartingClaim::ClaimedElsewhere => SlotStep::Refused(Refusal::AlreadyBeingStopped),
                StartingClaim::NotStarting => SlotStep::ReadAgain,
            }
        }
    }
}

/// Claims the running instance `instance_id` with its process `pid`, which a
/// stop then ends; another stop path's claim on that process is joined, as
/// both stops end the same process. A slot that no longer runs `pid` is read
/// again.
fn claim_running(entity_handle: EntityHandle, instance_id: &Name, pid: u32) -> SlotStep {
    let claim = entity_handle.read().claim_process(instance_id, pid);
    match claim {
        ProcessClaim::Claimed | ProcessClaim::ClaimedElsewhere => SlotStep::Stop {
            pid: Some(pid),
            entity: entity_handle,
        },
        ProcessClaim::Gone => SlotStep::ReadAgain,
    }
}

/// Claims and removes the slot of `instance_id` while it has no process; a
/// slot that started one meanwhile is read again.
fn clear_without_process(entity_handle: &EntityHandle, instance_id: &Name) -> SlotStep {
    match NodeEntity::claim_and_remove(entity_handle, instance_id, None) {
        SlotRemoval::Removed(kept) => SlotStep::Cleared(kept),
        SlotRemoval::Left(_) | SlotRemoval::Untracked => SlotStep::ReadAgain,
    }
}

/// How a start a stop claimed ended.
enum ClaimedStart {
    /// The claimed start's slot is gone; a slot now under the id is another
    /// instance's.
    Ended,
    /// The start committed before it saw the claim; its process `pid` runs.
    Committed { pid: Option<u32> },
}

/// How the start a stop claimed ended, from the slot now under its id;
/// `None` while it starts. A slot under the id that no stop claimed is a new
/// instance's, so the claimed one left.
fn claimed_start(slot: Option<TrackedNodeInstance>) -> Option<ClaimedStart> {
    let Some(slot) = slot.filter(TrackedNodeInstance::is_stopping) else {
        return Some(ClaimedStart::Ended);
    };
    match slot.state() {
        InstanceState::Starting => None,
        InstanceState::Running => Some(ClaimedStart::Committed { pid: slot.pid() }),
        // A failed start removes a claimed respawn's slot and an exit leaves
        // a claimed slot alone, so a claimed slot in these states is another
        // instance's, claimed by another stop path.
        InstanceState::Restarting | InstanceState::Finished | InstanceState::Failed => {
            Some(ClaimedStart::Ended)
        }
    }
}

/// Polls the slot until the restart's start this stop claimed ends: a claim
/// before the fork ends it there, one after by its abort (the group kill,
/// the readers' drain, a container's in-VM kill). The wait has no deadline
/// of its own.
async fn wait_for_claimed_start(node_stack: &NodeStack, instance_id: &Name) -> ClaimedStart {
    loop {
        match claimed_start(node_stack.find_instance_any_state(instance_id)) {
            Some(end) => return end,
            None => tokio::time::sleep(PROCESS_POLL_INTERVAL).await,
        }
    }
}

/// Why a stop is refused.
enum Refusal {
    /// No slot has the id.
    NotFound,
    /// The id is the core node's own instance.
    CoreNode,
    /// A first start in flight, refused until it completes.
    FirstStart,
    /// A start another stop path already claimed.
    AlreadyBeingStopped,
}

impl Refusal {
    /// The answer that refuses the stop of `instance_id`.
    fn response(self, instance_id: &Name) -> NodeStopResponse {
        NodeStopResponse::failure(match self {
            Refusal::NotFound => format!("Node instance '{instance_id}' not found in node stack"),
            Refusal::CoreNode => "Cannot stop the core node".to_owned(),
            Refusal::FirstStart => format!(
                "Node instance '{instance_id}' is in Starting state; retry after the start \
                 completes"
            ),
            Refusal::AlreadyBeingStopped => {
                format!("Node instance '{instance_id}' is already being stopped")
            }
        })
    }
}

/// Sends a `SHUTDOWN_SERVICE` request and returns once the receiver
/// acknowledges. Does NOT touch the entity's instances list; that is done
/// only after [`force_stop_instances`] has confirmed the OS processes are
/// gone, so retries can still find the live instance during an in-flight
/// shutdown.
async fn send_shutdown_signal(
    messenger: &MessengerHandle,
    core_node_node: &str,
    core_instance_id: &str,
    node_name: &str,
    node_tag: &str,
    instance_id: &Name,
) -> std::result::Result<(), String> {
    let instance_id_str = instance_id.as_str();
    debug!(
        "Sending shutdown request to node instance '{}'",
        instance_id_str
    );
    ServiceMessenger::poll(
        messenger,
        core_node_node,
        core_instance_id,
        SenderTarget::node(node_name, node_tag).map_err(|e| e.to_string())?,
        SHUTDOWN_SERVICE,
        ServiceTarget::Producer(&peppylib::messaging::ProducerRef::new(
            core_node_node,
            instance_id_str,
        )),
        Payload::from_static(b"shutdown"),
        SHUTDOWN_TIMEOUT,
    )
    .await
    .map_err(|e| {
        crate::error::Error::ShutdownInstanceFailed {
            instance_id: instance_id_str.to_owned(),
            reason: e.to_string(),
        }
        .to_string()
    })?;
    debug!("Node instance '{}' shutdown acknowledged", instance_id_str);
    Ok(())
}

/// How a single instance ended up stopped, surfaced to the user so a
/// force-kill (the node ignored the cooperative shutdown) is distinguishable
/// from a clean graceful exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StopOutcome {
    /// The node exited on its own within the grace period.
    Graceful,
    /// The node did not exit in time and its process group was SIGKILLed.
    ForceKilled,
    /// No tracked OS process; nothing to wait on or kill.
    NoProcess,
}

/// Cooperative-then-force termination of a single instance: the one-element
/// form of [`force_stop_instances`], used by `peppy node stop` so it behaves
/// identically to the SIGINT teardown ("properly OR improperly stopped").
///
/// Returns a [`StopOutcome`] so the caller can tell the user whether the node
/// exited gracefully or had to be force-killed. Takes NO lock: the caller
/// resolves the [`DoomedInstance`] fields and `graceful_budget` up front, so
/// nothing is held across an await.
///
/// `node_run` stops a starting instance the same way when it gives up its
/// start after the ready signal: the node answers the shutdown request from
/// then on, so its shutdown hooks run.
pub(super) async fn force_stop_instance(
    messenger: &MessengerHandle,
    core_node_node: &str,
    core_instance_id: &str,
    target: &DoomedInstance,
    graceful_budget: Duration,
) -> StopOutcome {
    force_stop_instances(
        messenger,
        core_node_node,
        core_instance_id,
        std::slice::from_ref(target),
        graceful_budget,
    )
    .await
    .pop()
    .expect("one outcome per doomed instance")
}

/// Cooperative-then-force termination of a batch of instances: the single
/// implementation behind `peppy node stop`, the `node add` overwrite path, and
/// the SIGINT/ctrl+C teardown:
///
/// 1. Best-effort `SHUTDOWN_SERVICE` sends, concurrently; a failed/timed-out
///    send is logged and ignored. A non-responsive node must still be
///    force-killed, never surfaced as an error (this is what makes a stuck node
///    a success, not a failure-with-the-process-left-alive).
/// 2. If a container node's shutdown request failed, best-effort SIGTERM before
///    the force window: in-VM SIGTERM on macOS/Lima, or host process-group
///    SIGTERM on native Linux Apptainer. This gives both backends the same
///    signal-driven cleanup chance before SIGKILL.
/// 3. Bounded graceful wait (`graceful_budget`, from
///    `peppy_config.lifecycle.shutdown_grace_secs`, shared across the whole
///    batch) for the OS processes to exit on their own.
/// 4. `kill_process_group(pid)` for each instance (SIGKILL the whole group;
///    nodes are spawned as group leaders) plus, on macOS for container nodes,
///    the in-VM guest group kill.
/// 5. Bounded reap ([`TEARDOWN_REAP_BUDGET`]) so the SIGKILLed groups are gone
///    before we return.
///
/// Returns one [`StopOutcome`] per input, in order, so callers can tell the
/// user whether each node exited gracefully or had to be force-killed. An
/// instance with `pid == None` has no OS process: it gets no shutdown request
/// and yields [`StopOutcome::NoProcess`].
async fn force_stop_instances(
    messenger: &MessengerHandle,
    core_node_node: &str,
    core_instance_id: &str,
    doomed: &[DoomedInstance],
    graceful_budget: Duration,
) -> Vec<StopOutcome> {
    // Phase 1 (graceful, bounded): ask every node to shut down cooperatively,
    // concurrently, then poll until they're all gone or the deadline elapses.
    // The deadline is the node's full cooperative exit cost (hook grace +
    // event-loop join + interpreter finalize), not just the hook grace, so a
    // node that cleans up correctly is never force-killed; see
    // `force_kill_deadline`.
    let deadline = force_kill_deadline(graceful_budget);
    let _ = tokio::time::timeout(deadline, async {
        let sends = doomed
            .iter()
            .filter(|d| d.pid.is_some())
            .map(|d| async move {
                let result = send_shutdown_signal(
                    messenger,
                    core_node_node,
                    core_instance_id,
                    &d.node_name,
                    &d.node_tag,
                    &d.instance_id,
                )
                .await;
                if let Err(e) = &result {
                    debug!(
                        "Cooperative shutdown of node instance '{}' failed; \
                         trying container signal fallback before force-kill: {}",
                        d.instance_id.as_str(),
                        e
                    );
                }
                (d, result)
            });
        let no_shutdown: Vec<&DoomedInstance> = futures::future::join_all(sends)
            .await
            .into_iter()
            .filter_map(|(d, result)| result.err().map(|_| d))
            .collect();
        terminate_failed_container_instances_best_effort(&no_shutdown).await;
        wait_until_all_gone(doomed).await;
    })
    .await;
    force_kill_leftovers(doomed, deadline, graceful_budget).await
}

/// Within the force-kill deadline of `graceful_budget`: SIGTERMs each
/// container of `no_shutdown`, the instances no shutdown request reached, as
/// a stop does, then waits for the processes of `doomed` and of
/// `claimed_elsewhere`, whose kill is another stop path's, to exit. Then
/// kills what is left of the groups of `doomed`, a container's in-VM group
/// included, and reaps them: [`force_stop_instances`] after its shutdown
/// requests, for a caller that sent its own. One [`StopOutcome`] per
/// `doomed`, in order.
pub(super) async fn await_exit_then_force_kill(
    doomed: &[DoomedInstance],
    no_shutdown: &[&DoomedInstance],
    claimed_elsewhere: &[DoomedInstance],
    graceful_budget: Duration,
) -> Vec<StopOutcome> {
    let deadline = force_kill_deadline(graceful_budget);
    let _ = tokio::time::timeout(deadline, async {
        terminate_failed_container_instances_best_effort(no_shutdown).await;
        wait_until_all_gone(doomed).await;
        wait_until_all_gone(claimed_elsewhere).await;
    })
    .await;
    force_kill_leftovers(doomed, deadline, graceful_budget).await
}

/// Steps 4 and 5 of [`force_stop_instances`]: SIGKILLs each group, a
/// container's in-VM group included, and reaps them; `deadline` and
/// `graceful_budget` word the force-kill warning.
async fn force_kill_leftovers(
    doomed: &[DoomedInstance],
    deadline: Duration,
    graceful_budget: Duration,
) -> Vec<StopOutcome> {
    // Phase 2 (force): SIGKILL each recorded process group. We deliberately do
    // not pre-check whether the leader is still alive before killing: a process
    // group can outlive its leader (the leader exits but a descendant it spawned
    // keeps running), and the negative-pid kill reaches the whole group (each
    // node is spawned as a group leader), so descendants die too. An already-dead
    // group yields ESRCH, which kill_process_group ignores.
    //
    // We do take one snapshot here purely to classify which nodes are still
    // alive (i.e. ignored the cooperative shutdown) so the user is warned that
    // they were force-killed rather than having exited gracefully.
    let mut snapshot = sysinfo::System::new();
    refresh_pids(&mut snapshot, &doomed_pids(doomed));
    let outcomes = doomed
        .iter()
        .map(|d| {
            let Some(pid) = d.pid else {
                return StopOutcome::NoProcess; // Nothing to force-kill or reap.
            };
            if !pid_running_in(&snapshot, pid) {
                debug!(
                    "Node instance '{}' exited gracefully within the grace period",
                    d.instance_id.as_str()
                );
                return StopOutcome::Graceful;
            }
            warn!(
                "Node instance '{}' did not exit within its {}s cooperative shutdown \
                 window ({}s grace + runtime teardown); force-killing its process group \
                 (pid {})",
                d.instance_id.as_str(),
                deadline.as_secs(),
                graceful_budget.as_secs(),
                pid
            );
            StopOutcome::ForceKilled
        })
        .collect();

    kill_process_groups(doomed).await;

    outcomes
}

/// The force phases of a stop, alone: SIGKILLs the process group of every
/// instance, reaches the in-VM group of a container instance on macOS, and
/// waits a bounded time for the killed groups to be reaped. The second half of
/// [`force_stop_instances`], through [`force_kill_leftovers`].
async fn kill_process_groups(doomed: &[DoomedInstance]) {
    // Phase 2 (force): SIGKILL each recorded process group. We deliberately do
    // not pre-check whether the leader is still alive before killing: a process
    // group can outlive its leader (the leader exits but a descendant it spawned
    // keeps running), and the negative-pid kill reaches the whole group (each
    // node is spawned as a group leader), so descendants die too. An already-dead
    // group yields ESRCH, which kill_process_group ignores.
    let mut guest_kill_keys: Vec<String> = Vec::new();
    for d in doomed {
        let Some(pid) = d.pid else {
            continue; // Nothing to force-kill or reap.
        };
        kill_process_group(pid);
        if d.is_container {
            guest_kill_keys.push(d.instance_id.as_str().to_owned());
        }
    }

    // Phase 2b (macOS): for container nodes the host group kill above only
    // reached the `limactl` client; the workload runs inside the Lima VM. Reach
    // into the VM and SIGKILL each recorded guest process group (keyed by
    // instance id, matching the `cancel_pgid` set at spawn). The facade owns the
    // platform gate (no-op on Linux, where the host kill already reached the
    // shared-namespace container) and never fails: a guest-kill problem must
    // not block a stop or teardown. Runs on a blocking thread because it shells
    // out to `limactl`.
    kill_guest_groups(guest_kill_keys).await;

    // Phase 3 (bounded reap): let the killed groups die while the daemon is
    // still alive so they don't briefly linger as zombies parented to it; any
    // straggler reparents to init/launchd and is reaped there.
    let _ = tokio::time::timeout(TEARDOWN_REAP_BUDGET, wait_until_all_gone(doomed)).await;
}

/// If the Peppy shutdown service could not be delivered to a container node,
/// give it a normal SIGTERM before the force-kill deadline. On macOS/Lima the
/// signal must be sent inside the guest because the host process group is only
/// the `limactl` client. On Linux/native Apptainer there is no VM boundary, so
/// the host process group is the cooperative signal target.
async fn terminate_failed_container_instances_best_effort(doomed: &[&DoomedInstance]) {
    let failed_containers: Vec<&DoomedInstance> = doomed
        .iter()
        .copied()
        .filter(|d| d.is_container && d.pid.is_some())
        .collect();
    if failed_containers.is_empty() {
        return;
    }

    let guest_term_keys: Vec<String> = failed_containers
        .iter()
        .map(|d| d.instance_id.as_str().to_owned())
        .collect();
    let guest_term_attempted = node_stack::terminate_guest_groups(guest_term_keys).await;

    if !guest_term_attempted {
        for doomed in failed_containers {
            if let Some(pid) = doomed.pid {
                terminate_process_group(pid);
            }
        }
    }
}

/// Removes a stopped instance from the entity's registry, returning what
/// its slot kept of its relations (nothing for an untracked one). Called
/// only after the OS process, when there was one, has been confirmed
/// terminated.
fn remove_instance_from_registry(
    node_stack: &NodeStack,
    node_name: &str,
    node_tag: &str,
    instance_id: &Name,
) -> KeptRelations {
    let Some(handle) = node_stack.find(node_name, node_tag) else {
        return KeptRelations::default();
    };
    let mut guard = handle.write();
    if let Some(kept) = guard.stop_instance(instance_id) {
        return kept;
    }
    let starting = guard
        .instance(instance_id)
        .is_some_and(|inst| inst.state() == InstanceState::Starting);
    if starting {
        debug!(
            "Node instance '{}' is in Starting state on {}:{}; cannot stop via stop_instance \
             (will resolve via abort_started)",
            instance_id.as_str(),
            node_name,
            node_tag
        );
    } else {
        debug!(
            "Node instance '{}' was not tracked in entity {}:{}",
            instance_id.as_str(),
            node_name,
            node_tag
        );
    }
    KeptRelations::default()
}

/// Cooperative-then-force stop of the given running instances of one entity,
/// then removes them from the node stack. The entity remains in the graph with
/// zero instances, preserving dependency edges so that a subsequent
/// `push_config` call can correctly validate interface changes against
/// dependents.
///
/// Shares [`force_stop_instances`] with `handle_node_stop_request_inner` and
/// the SIGINT teardown, so a stuck instance is force-killed (not left alive
/// behind a timeout error); and, like the teardown, the whole batch is stopped
/// together: every cooperative shutdown is sent concurrently and the instances
/// share ONE grace budget, so an overwrite of an entity with several stuck
/// instances costs one grace window, not one per instance. Returns only once
/// every process is gone, with what each removed slot kept of its relations.
/// Infallible.
pub(crate) async fn stop_instances(
    messenger: &MessengerHandle,
    core_node_node: &str,
    core_instance_id: &str,
    node_stack: &Arc<NodeStack>,
    node_name: &str,
    node_tag: &str,
    instance_ids: &[Name],
) -> BTreeMap<Name, KeptRelations> {
    if instance_ids.is_empty() {
        return BTreeMap::new();
    }
    // Resolve pid + is_container for every instance under ONE short read lock,
    // released before the await below. The pids are read up front, before
    // the shutdown, from one consistent view of the registry. An instance
    // that is no longer tracked resolves to a pid-less target, which gets no
    // shutdown request and no kill.
    let doomed: Vec<DoomedInstance> = match node_stack.find(node_name, node_tag) {
        Some(handle) => {
            let guard = handle.read();
            instance_ids
                .iter()
                .map(|instance_id| {
                    let pid = guard
                        .claim_instance(instance_id)
                        .and_then(|inst| inst.pid());
                    DoomedInstance::of(&guard, instance_id.clone(), pid)
                })
                .collect()
        }
        None => return BTreeMap::new(), // Entity already gone; nothing to stop.
    };

    // force_stop_instances warns (daemon-side) when it has to force-kill, which
    // is the right surface for this overwrite path (the user sees node_add's
    // feedback stream; node_stop additionally relays force-kill to the CLI).
    force_stop_and_forget(
        messenger,
        core_node_node,
        core_instance_id,
        node_stack,
        &doomed,
    )
    .await
}

/// Stops in reverse dependency order, including terminal initializer
/// instances. An id no node runs is torn down from the relationship
/// registries alone, where a start that never came still registered it.
pub(crate) async fn stop_named_instances(
    messenger: &MessengerHandle,
    core_node: &str,
    root_id: &str,
    node_stack: &Arc<NodeStack>,
    relationships: &RelationshipCoordinators,
    ids: &[Name],
) {
    for instance in named_instances(node_stack, ids) {
        stop_named_instance(
            messenger,
            core_node,
            root_id,
            node_stack,
            relationships,
            &instance,
        )
        .await;
    }
}

/// One id a stop of named instances takes, with the node `name:tag` of the
/// instance the stack holds under it, when it holds one.
pub(crate) struct NamedInstance<'a> {
    pub(crate) id: &'a Name,
    node: Option<(String, String)>,
}

impl NamedInstance<'_> {
    /// Whether the stack holds the instance, so that its stop stops one.
    pub(crate) fn is_on_stack(&self) -> bool {
        self.node.is_some()
    }
}

/// Each of `ids`, the last first, with the node of the instance the stack
/// holds under it, all read from one snapshot of the stack: the order and
/// the instances of a stop of named instances.
pub(crate) fn named_instances<'a>(
    node_stack: &NodeStack,
    ids: &'a [Name],
) -> Vec<NamedInstance<'a>> {
    let graph = node_stack.to_serialized_graph();
    ids.iter()
        .rev()
        .map(|id| NamedInstance {
            id,
            node: graph
                .nodes
                .iter()
                .find(|node| {
                    node.instances
                        .iter()
                        .any(|instance| instance.instance_id == id.as_str())
                })
                .map(|node| (node.name.clone(), node.tag.clone())),
        })
        .collect()
}

/// Stops one instance of [`named_instances`]: the instance the stack holds,
/// then its entries in the relationship registries with what its removed
/// slot kept of its relations, where a start that never came still
/// registered it.
pub(crate) async fn stop_named_instance(
    messenger: &MessengerHandle,
    core_node: &str,
    root_id: &str,
    node_stack: &Arc<NodeStack>,
    relationships: &RelationshipCoordinators,
    instance: &NamedInstance<'_>,
) {
    let kept = match &instance.node {
        Some((name, tag)) => stop_instances(
            messenger,
            core_node,
            root_id,
            node_stack,
            name,
            tag,
            std::slice::from_ref(instance.id),
        )
        .await
        .remove(instance.id)
        .unwrap_or_default(),
        None => KeptRelations::default(),
    };
    relationships
        .tear_down_instance(instance.id.as_str(), kept)
        .await;
}

/// Cooperative-then-force stop of `doomed` (see [`force_stop_instances`]),
/// then removal of each from the stack, returning what each removed slot
/// kept of its relations. Once a process is gone the stack must
/// not keep tracking its pid: the stack's drop signals every pid it still
/// holds, and a pid whose process exited could be reused.
async fn force_stop_and_forget(
    messenger: &MessengerHandle,
    core_node_node: &str,
    core_instance_id: &str,
    node_stack: &NodeStack,
    doomed: &[DoomedInstance],
) -> BTreeMap<Name, KeptRelations> {
    force_stop_instances(
        messenger,
        core_node_node,
        core_instance_id,
        doomed,
        // Configurable cooperative-shutdown grace (peppy_config.lifecycle
        // .shutdown_grace_secs), pinned on the stack at daemon startup.
        node_stack.shutdown_grace(),
    )
    .await;

    doomed
        .iter()
        .map(|d| {
            let kept = remove_instance_from_registry(
                node_stack,
                &d.node_name,
                &d.node_tag,
                &d.instance_id,
            );
            (d.instance_id.clone(), kept)
        })
        .collect()
}

/// Bounded wait for SIGKILLed groups to be reaped before the daemon exits, so
/// they don't briefly linger as zombies parented to the still-alive daemon.
/// The cooperative-phase grace window is configurable via
/// `peppy_config.lifecycle.shutdown_grace_secs` and carried on the
/// `NodeStack`; this reap budget is a fixed term of [`node_stop_bound`].
pub const TEARDOWN_REAP_BUDGET: Duration = Duration::from_secs(2);

/// How long a stop's force phase takes at most: the force-kill deadline,
/// the in-VM kill and the reap.
fn force_stop_bound(shutdown_grace: Duration) -> Duration {
    force_kill_deadline(shutdown_grace) + GUEST_FORCE_KILL_BUDGET + TEARDOWN_REAP_BUDGET
}

/// How long a `node_stop` takes at most once its instance's start, if one is
/// in flight, has forked: a running instance's force phase and the notice of
/// its end to the daemons its relations name. A restart's start the stop
/// cancels ends within its readers' bound and its in-VM kill, which the reap
/// and the in-VM kill cover. Outside the bound: the dissolve of its pairs and
/// the source-down update of its observers wait for the pairing and
/// observation coordinators, which a delivery in progress on this daemon
/// holds across its peers' answers; a local peer that does not answer its
/// pair's dissolve, or a local observer its source-down update, adds its own
/// wait; and a stop that meets a start committing polls once more.
pub fn node_stop_bound(shutdown_grace: Duration) -> Duration {
    force_stop_bound(shutdown_grace) + super::relationship_notify::NOTIFY_TIMEOUT
}

const _: () = assert!(
    node_stack::OUTPUT_READER_JOIN_BUDGET.as_millis() <= TEARDOWN_REAP_BUDGET.as_millis(),
    "a cancelled start's readers end within the reap a running instance's stop waits"
);

/// How long a `node_remove` takes at most once its reachability probes
/// answered: its shutdown requests, then a [`node_stop_bound`] for the
/// instances it stops, whose ends are noticed at once; what lies outside
/// [`node_stop_bound`] lies outside it too.
pub fn node_remove_bound(shutdown_grace: Duration) -> Duration {
    SHUTDOWN_TIMEOUT + node_stop_bound(shutdown_grace)
}

/// How long the daemon waits for a cooperatively-stopping node's process to
/// disappear before force-killing its group. Strictly larger than the node's
/// real exit cost so a node that cleans up correctly is never SIGKILLed: after
/// receiving the shutdown request a node runs its hooks (bounded by
/// `shutdown_grace`), then a Python node joins its asyncio event-loop thread
/// (bounded by [`config::peppy_config::EVENT_LOOP_JOIN_BUDGET_SECS`]), then the
/// interpreter finalizes ([`config::peppy_config::RUNTIME_FINALIZE_MARGIN_SECS`]
/// of slack). The single source of this formula: the CLI request timeout and
/// the `shutdown_grace_margin` regression test both derive from it.
pub fn force_kill_deadline(shutdown_grace: Duration) -> Duration {
    shutdown_grace
        + Duration::from_secs(
            config::peppy_config::EVENT_LOOP_JOIN_BUDGET_SECS
                + config::peppy_config::RUNTIME_FINALIZE_MARGIN_SECS,
        )
}

/// Upper bound for one batch of cooperative shutdown, force-kill, and reap.
pub(crate) fn teardown_timeout(shutdown_grace: Duration) -> Duration {
    SHUTDOWN_TIMEOUT + force_stop_bound(shutdown_grace)
}

/// A non-root node instance to terminate: the routing identity + process info
/// needed to stop one instance. Fed to [`force_stop_instances`] in a batch by
/// [`teardown_all_instances`] and [`stop_instances`] (the `node add` overwrite
/// path), one at a time by [`force_stop_instance`] (`peppy node stop`, and
/// `node_run` giving up a start after the ready signal), and to
/// [`await_exit_then_force_kill`] by `peppy node remove`, which sends its own
/// shutdown requests.
#[derive(Clone)]
pub(super) struct DoomedInstance {
    pub(super) node_name: String,
    pub(super) node_tag: String,
    pub(super) instance_id: Name,
    /// The process to wait on and force-kill; `None` for a `restarting` or
    /// terminal instance, a removal's start in flight, or an id the stack does
    /// not track, which gets no shutdown request and no kill.
    pub(super) pid: Option<u32>,
    /// `true` when this instance is a container node. On macOS the workload runs
    /// inside the Lima VM, so host process-group signals only reach the
    /// `limactl` client; the stop path also signals the in-VM group keyed by
    /// `instance_id` (SIGTERM after a failed shutdown request, SIGKILL in the
    /// force phase). Always `false` for process nodes (the host group kill
    /// covers them) and a no-op for containers on Linux.
    pub(super) is_container: bool,
}

impl DoomedInstance {
    /// The target of the instance `instance_id` of `entity`, running `pid`.
    pub(super) fn of(entity: &NodeEntity, instance_id: Name, pid: Option<u32>) -> Self {
        Self {
            node_name: entity.config().manifest.name.as_str().to_owned(),
            node_tag: entity.config().manifest.tag.clone(),
            instance_id,
            pid,
            is_container: entity.config().execution.container.is_some(),
        }
    }
}

/// Force every non-root node out of the stack on a catchable daemon shutdown
/// (ctrl+C / SIGTERM), so no node (or any descendant it spawned) outlives the
/// daemon as an orphan.
///
/// Graceful-then-force: first asks each node to shut down cooperatively (the
/// same `SHUTDOWN_SERVICE` path as `peppy node stop`, so robot nodes can stop
/// actuators cleanly), gives them a short shared budget to exit, then SIGKILLs
/// the process group of anything still alive. The root (core) node is skipped:
/// it is the daemon itself. This does NOT wait the uncatchable-death grace
/// period; that timer only governs a daemon that died without running cleanup.
///
/// Delegates to [`force_stop_and_forget`]: the same phases as `peppy node
/// stop`, batched (one grace budget shared across all instances and every
/// cooperative shutdown sent concurrently), then each instance is dropped
/// from the stack, which keeps no pid whose process is gone.
pub async fn teardown_all_instances(
    messenger: &MessengerHandle,
    core_node_name: &str,
    core_instance_id: &str,
    node_stack: &Arc<NodeStack>,
) {
    let doomed = collect_doomed_instances(node_stack);
    if doomed.is_empty() {
        return;
    }
    debug!(
        "Tearing down {} node instance(s) on daemon shutdown",
        doomed.len()
    );
    force_stop_and_forget(
        messenger,
        core_node_name,
        core_instance_id,
        node_stack,
        &doomed,
    )
    .await;
}

/// Snapshot every non-root instance, in any state (a `Starting` instance
/// already has a live child), with its routing identity and pid, none for an
/// instance without a process. Skips the root entity by pointer identity.
///
/// Each collected instance is claimed (`claim_instance`) so its exit watcher
/// treats the upcoming kill as intentional and does not relabel it a crash. The
/// daemon-shutdown caller does not strictly need this (its watchers bail on the
/// shutdown token), but the reset and launch-clear callers do: they tear the
/// stack down while the daemon keeps running, so without the claim each
/// force-killed instance would be recorded as `Failed` by its watcher.
fn collect_doomed_instances(node_stack: &NodeStack) -> Vec<DoomedInstance> {
    let root = node_stack.root();
    let mut doomed = Vec::new();
    for handle in node_stack.snapshot() {
        if Arc::ptr_eq(&handle, &root) {
            continue;
        }
        let guard = handle.read();
        for inst in guard.instances() {
            guard.claim_instance(inst.instance_id());
            doomed.push(DoomedInstance::of(
                &guard,
                inst.instance_id().clone(),
                inst.pid(),
            ));
        }
    }
    doomed
}

/// Polls until every instance with a known pid has exited (or become a
/// zombie). Refreshes only the watched pids each tick rather than rescanning
/// every process on the machine. Unbounded on its own; callers wrap it in
/// `tokio::time::timeout` so [`force_stop_instances`] can apply different
/// graceful and reap budgets to the same primitive.
async fn wait_until_all_gone(doomed: &[DoomedInstance]) {
    let pids = doomed_pids(doomed);
    if pids.is_empty() {
        return;
    }
    let mut system = sysinfo::System::new();
    loop {
        refresh_pids(&mut system, &pids);
        if pids
            .iter()
            .all(|&pid| !pid_running_in(&system, pid.as_u32()))
        {
            return;
        }
        tokio::time::sleep(PROCESS_POLL_INTERVAL).await;
    }
}

/// The known pids of `doomed`, in `sysinfo` form, for [`refresh_pids`].
fn doomed_pids(doomed: &[DoomedInstance]) -> Vec<sysinfo::Pid> {
    doomed
        .iter()
        .filter_map(|d| d.pid)
        .map(sysinfo::Pid::from_u32)
        .collect()
}

/// Refreshes only `pids` in `system` (existence + status), reading just those
/// `/proc` entries instead of rescanning every process on the machine. Dead
/// pids are dropped from the set so [`pid_running_in`] reports them gone.
fn refresh_pids(system: &mut sysinfo::System, pids: &[sysinfo::Pid]) {
    system.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(pids),
        true,
        sysinfo::ProcessRefreshKind::nothing(),
    );
}

/// Whether the process `pid` runs: present and not a zombie. It reads the
/// process table and reaps nothing, so a child of the daemon that exited
/// stays a zombie, which holds its pid, until the daemon reaps it.
pub(super) fn process_runs(pid: u32) -> bool {
    let mut system = sysinfo::System::new();
    refresh_pids(&mut system, &[sysinfo::Pid::from_u32(pid)]);
    pid_running_in(&system, pid)
}

/// Whether `pid` is present and not a zombie in an already-refreshed `system`.
fn pid_running_in(system: &sysinfo::System, pid: u32) -> bool {
    match system.process(sysinfo::Pid::from_u32(pid)) {
        Some(process) => process.status() != sysinfo::ProcessStatus::Zombie,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each bound sums its terms: a stop's is its force phase (the force-kill
    /// deadline, the in-VM kill and the reap) and the notice of its end; a
    /// removal's adds its shutdown requests to a stop's; a batch teardown's,
    /// which announces nothing, is the force phase with a shutdown request's
    /// timeout beside it.
    #[test]
    fn each_bound_sums_its_terms() {
        let grace = Duration::from_secs(config::peppy_config::DEFAULT_SHUTDOWN_GRACE_SECS);
        let force_phase =
            force_kill_deadline(grace) + GUEST_FORCE_KILL_BUDGET + TEARDOWN_REAP_BUDGET;
        let notice = crate::services::node::relationship_notify::NOTIFY_TIMEOUT;
        assert_eq!(node_stop_bound(grace), force_phase + notice);
        assert_eq!(
            node_remove_bound(grace),
            SHUTDOWN_TIMEOUT + force_phase + notice
        );
        assert_eq!(teardown_timeout(grace), SHUTDOWN_TIMEOUT + force_phase);
    }

    /// A first start is refused until it completes, a start another stop
    /// claimed is already being stopped, an untracked id is not found, and
    /// the core node is not stopped.
    #[test]
    fn a_refused_stop_says_why() {
        let id = Name::new("arm_inst").unwrap();
        let cases = [
            (
                Refusal::FirstStart,
                "Node instance 'arm_inst' is in Starting state; retry after the start completes",
            ),
            (
                Refusal::AlreadyBeingStopped,
                "Node instance 'arm_inst' is already being stopped",
            ),
            (
                Refusal::NotFound,
                "Node instance 'arm_inst' not found in node stack",
            ),
            (Refusal::CoreNode, "Cannot stop the core node"),
        ];
        for (refusal, text) in cases {
            let response = refusal.response(&id);
            assert!(!response.success);
            assert_eq!(response.error_message.as_deref(), Some(text));
        }
    }

    /// A claimed start is waited for while it starts, handed over once it
    /// commits, and over once its slot left or another instance's slot, a
    /// claimed one included, holds its id.
    #[test]
    fn a_claimed_start_ends_by_its_slot() {
        let slot = |state: InstanceState, claimed: bool| {
            let slot = TrackedNodeInstance::new(
                Name::new("arm_inst").unwrap(),
                state,
                std::collections::BTreeMap::new(),
            );
            if claimed {
                slot.mark_stopping();
            }
            Some(slot)
        };
        assert!(claimed_start(slot(InstanceState::Starting, true)).is_none());
        assert!(matches!(
            claimed_start(slot(InstanceState::Running, true)),
            Some(ClaimedStart::Committed { .. })
        ));
        for ended in [
            None,
            slot(InstanceState::Starting, false),
            slot(InstanceState::Running, false),
            slot(InstanceState::Restarting, true),
        ] {
            assert!(matches!(claimed_start(ended), Some(ClaimedStart::Ended)));
        }
    }
}
