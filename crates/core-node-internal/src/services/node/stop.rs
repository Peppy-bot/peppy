use crate::Result;
use crate::services::response::into_service_response;
use config::runtime::Name;
use core_node_api::ServiceId;
use core_node_api::encoding::{NodeStopRequest, NodeStopResponse};
use core_node_api::names;
use node_stack::{EntityHandle, NodeStack, TrackedNodeInstance};
use peppylib::messaging::SenderTarget;
use peppylib::messaging::{
    SHUTDOWN_SERVICE, ServiceMessenger, ServiceRequestContext, ServiceTarget,
};
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyResult};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Defensive latency cap on the macOS in-VM guest force-kill phase. The work is
/// already internally bounded (the Lima VM liveness probe and each per-key guest
/// SIGKILL carry their own `limactl` subprocess deadlines), so this only bounds
/// the stop's completion latency if a `limactl` call wedges past those internal
/// deadlines. It does not cancel the blocking closure (a `spawn_blocking` task
/// runs to completion regardless); it only stops the stop path waiting on it.
const GUEST_FORCE_KILL_BUDGET: Duration = Duration::from_secs(30);

pub async fn listen_for_node_stop(
    messenger: &MessengerHandle,
    core_node_node: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    pairing: Arc<super::pairing::PairingCoordinator>,
    observation: Arc<super::observation::ObservationCoordinator>,
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
                    Arc::clone(&pairing),
                    Arc::clone(&observation),
                )
            })
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

#[allow(clippy::too_many_arguments)]
async fn handle_node_stop_request(
    context: ServiceRequestContext,
    messenger: MessengerHandle,
    core_node_node: String,
    core_instance_id: String,
    node_stack: Arc<NodeStack>,
    pairing: Arc<super::pairing::PairingCoordinator>,
    observation: Arc<super::observation::ObservationCoordinator>,
) -> PeppyResult<Payload> {
    into_service_response(
        &context,
        handle_node_stop_request_inner(
            &context,
            &messenger,
            &core_node_node,
            &core_instance_id,
            node_stack,
            &pairing,
            &observation,
        )
        .await,
    )
}

#[allow(clippy::too_many_arguments)]
async fn handle_node_stop_request_inner(
    context: &ServiceRequestContext,
    messenger: &MessengerHandle,
    core_node_node: &str,
    core_instance_id: &str,
    node_stack: Arc<NodeStack>,
    pairing: &super::pairing::PairingCoordinator,
    observation: &super::observation::ObservationCoordinator,
) -> Result<Payload> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let request = NodeStopRequest::decode(payload.as_ref())?;

    debug!(
        "Received `node_stop` request from {sender_instance_id}, instance_id={}",
        request.instance_id
    );

    // Parse the instance_id string to a Name
    let instance_id = match Name::new(&request.instance_id) {
        Ok(name) => name,
        Err(e) => {
            return NodeStopResponse::failure(format!("Invalid instance_id: {}", e))
                .encode()
                .map_err(Into::into);
        }
    };

    let (instance, entity_handle) =
        match find_running_instance_and_entity(&node_stack, &instance_id, &request.instance_id) {
            Ok(found) => found,
            Err(response) => return response.encode().map_err(Into::into),
        };

    // Resolve everything we need from the entity up front, under a short-lived
    // read lock that is NOT held across any await. `is_container` drives the
    // macOS in-VM guest kill; `pid` is the process to wait on / force-kill.
    let pid = instance.pid();
    let (node_name, node_tag, is_container) = {
        let guard = entity_handle.read();
        (
            guard.config().manifest.name.as_str().to_owned(),
            guard.config().manifest.tag.clone(),
            guard.config().execution.container.is_some(),
        )
    };
    let (root_node_name, root_node_tag) = {
        let root = node_stack.root();
        let guard = root.read();
        (
            guard.config().manifest.name.as_str().to_owned(),
            guard.config().manifest.tag.clone(),
        )
    };

    if node_name == root_node_name && node_tag == root_node_tag {
        return NodeStopResponse::failure("Cannot stop the core node")
            .encode()
            .map_err(Into::into);
    }

    // Claim the instance for termination before signaling it, so its exit
    // watcher treats the upcoming exit as an intentional stop (owned by this
    // path's registry removal) rather than a self-exit, and never relabels a
    // force-kill as a crash. Claim under the entity read lock (not on the
    // lock-free clone) so the claim is mutually exclusive with the write lock
    // `mark_instance_exited` holds across its `is_stopping()` check and its
    // terminal-state flip. A lock-free store on the shared `Arc` could otherwise
    // land between that check and flip, letting a self-exit racing this stop be
    // recorded as a terminal exit. The bulk teardown paths (`stop_instances`,
    // `collect_doomed_instances`) claim the same way.
    {
        let guard = entity_handle.read();
        if let Some(inst) = guard
            .instances()
            .iter()
            .find(|inst| inst.instance_id() == &instance_id)
        {
            inst.mark_stopping();
        }
    }

    // Cooperative-then-force, identical to the SIGINT teardown for this one
    // instance: ask the node to shut down, give it a bounded grace window, then
    // SIGKILL its process group if it ignored us, and wait for it to be gone.
    // A stuck/non-responsive node is now force-killed (and reported as success)
    // rather than left alive behind a timeout error. We deliberately keep the
    // live registry entry until after the kill+reap so a retried or racing
    // caller can still observe the in-flight shutdown.
    let target = DoomedInstance {
        node_name: node_name.clone(),
        node_tag: node_tag.clone(),
        instance_id: instance_id.clone(),
        pid,
        is_container,
    };
    let outcome = force_stop_instance(
        messenger,
        core_node_node,
        core_instance_id,
        &target,
        node_stack.shutdown_grace(),
    )
    .await;

    // Process is gone (properly or improperly). Finalize the registry removal,
    // then run the single teardown seam: dissolve its pairs and live-notify
    // each surviving peer that its slot is Unpaired, and drop its observations
    // and notify any live observers a source went down (death auto-clears;
    // re-pairing is explicit). A stopped node is torn down identically whether
    // it paired, observed, both, or neither.
    remove_instance_from_registry(&node_stack, &node_name, &node_tag, &instance_id);
    super::tear_down_instance(pairing, observation, instance_id.as_str()).await;

    // Tell the caller whether the node exited gracefully or had to be
    // force-killed, so the CLI can warn the user about the latter.
    let response = match outcome {
        StopOutcome::ForceKilled => NodeStopResponse::success_force_killed(),
        StopOutcome::Graceful | StopOutcome::NoProcess => NodeStopResponse::success(),
    };
    response.encode().map_err(Into::into)
}

/// Finds the instance and its entity by instance_id, or the failure response
/// the stop handler should return. The lookups intentionally only match
/// `Running` instances. If the instance is mid-start (`Starting`), the lookup
/// returns `None` even though the instance does exist, so
/// [`stop_lookup_failure`] surfaces that as a distinct retryable error and
/// callers can back off and retry once `commit_started`/`abort_started` has
/// resolved the in-flight start.
fn find_running_instance_and_entity(
    node_stack: &Arc<NodeStack>,
    instance_id: &Name,
    raw_instance_id: &str,
) -> std::result::Result<(TrackedNodeInstance, EntityHandle), NodeStopResponse> {
    let Some(instance) = node_stack.find_by_instance_id(instance_id) else {
        return Err(stop_lookup_failure(
            node_stack,
            instance_id,
            raw_instance_id,
        ));
    };
    let Some(entity_handle) = node_stack.find_entity_by_instance_id(instance_id) else {
        return Err(stop_lookup_failure(
            node_stack,
            instance_id,
            raw_instance_id,
        ));
    };
    Ok((instance, entity_handle))
}

/// Builds the failure response for a stop request whose instance lookup came
/// back empty: "mid-start, retry later" when the id exists in `Starting`
/// state, "not found" otherwise.
fn stop_lookup_failure(
    node_stack: &Arc<NodeStack>,
    instance_id: &Name,
    raw_instance_id: &str,
) -> NodeStopResponse {
    if instance_is_in_starting(node_stack, instance_id) {
        NodeStopResponse::failure(format!(
            "Node instance '{}' is in Starting state; retry after the start completes",
            raw_instance_id
        ))
    } else {
        NodeStopResponse::failure(format!(
            "Node instance '{}' not found in node stack",
            raw_instance_id
        ))
    }
}

/// Returns `true` if any tracked entity contains an instance with the
/// given id in `InstanceState::Starting`. Used to disambiguate "instance does
/// not exist" from "instance exists but is mid-start" in the stop handler.
fn instance_is_in_starting(node_stack: &Arc<NodeStack>, instance_id: &Name) -> bool {
    node_stack.snapshot().into_iter().any(|handle| {
        handle.read().instances().iter().any(|inst| {
            inst.instance_id() == instance_id && inst.state() == node_stack::InstanceState::Starting
        })
    })
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
enum StopOutcome {
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
async fn force_stop_instance(
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
/// instance with `pid == None` has no OS process to terminate (the cooperative
/// send is still attempted best-effort) and yields [`StopOutcome::NoProcess`].
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
        let sends = doomed.iter().map(|d| async move {
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
        let failed_shutdowns: Vec<&DoomedInstance> = futures::future::join_all(sends)
            .await
            .into_iter()
            .filter_map(|(d, result)| result.err().map(|_| d))
            .collect();
        terminate_failed_container_instances_best_effort(&failed_shutdowns).await;
        wait_until_all_gone(doomed).await;
    })
    .await;

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
    let mut guest_kill_keys: Vec<String> = Vec::new();
    let outcomes = doomed
        .iter()
        .map(|d| {
            let Some(pid) = d.pid else {
                return StopOutcome::NoProcess; // Nothing to force-kill or reap.
            };
            let outcome = if pid_running_in(&snapshot, pid) {
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
            } else {
                debug!(
                    "Node instance '{}' exited gracefully within the grace period",
                    d.instance_id.as_str()
                );
                StopOutcome::Graceful
            };
            kill_process_group(pid);
            if d.is_container {
                guest_kill_keys.push(d.instance_id.as_str().to_owned());
            }
            outcome
        })
        .collect();

    // Phase 2b (macOS): for container nodes the host group kill above only
    // reached the `limactl` client; the workload runs inside the Lima VM. Reach
    // into the VM and SIGKILL each recorded guest process group (keyed by
    // instance id, matching the `cancel_pgid` set at spawn). The facade owns the
    // platform gate (no-op on Linux, where the host kill already reached the
    // shared-namespace container) and never fails: a guest-kill problem must
    // not block a stop or teardown. Runs on a blocking thread because it shells
    // out to `limactl`.
    if !guest_kill_keys.is_empty() {
        // Cap how long the stop waits on the in-VM guest kill. The kill is itself
        // internally bounded (see `GUEST_FORCE_KILL_BUDGET`), so this only fires if
        // a `limactl` call wedges past its own deadline; it does not cancel the
        // blocking closure, which still runs to completion on the blocking pool.
        let _ = tokio::time::timeout(
            GUEST_FORCE_KILL_BUDGET,
            tokio::task::spawn_blocking(move || {
                containers::Apptainer::kill_guest_process_groups_best_effort(&guest_kill_keys);
            }),
        )
        .await;
    }

    // Phase 3 (bounded reap): let the killed groups die while the daemon is
    // still alive so they don't briefly linger as zombies parented to it; any
    // straggler reparents to init/launchd and is reaped there.
    let _ = tokio::time::timeout(TEARDOWN_REAP_BUDGET, wait_until_all_gone(doomed)).await;

    outcomes
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
    let guest_term_attempted = tokio::task::spawn_blocking(move || {
        containers::Apptainer::terminate_guest_process_groups_best_effort(&guest_term_keys)
    })
    .await
    .unwrap_or(false);

    if !guest_term_attempted {
        for doomed in failed_containers {
            if let Some(pid) = doomed.pid {
                terminate_process_group(pid);
            }
        }
    }
}

/// Removes a `Running` instance from the entity's registry. Called only
/// after the OS process has been confirmed terminated.
fn remove_instance_from_registry(
    node_stack: &Arc<NodeStack>,
    node_name: &str,
    node_tag: &str,
    instance_id: &Name,
) {
    let Some(handle) = node_stack.find(node_name, node_tag) else {
        return;
    };
    let mut guard = handle.write();
    let removed = guard.stop_instance(instance_id);
    if !removed {
        let starting = guard.instances().iter().any(|inst| {
            inst.instance_id() == instance_id && inst.state() == node_stack::InstanceState::Starting
        });
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
    }
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
/// every process is gone. Infallible.
pub(super) async fn stop_instances(
    messenger: &MessengerHandle,
    core_node_node: &str,
    core_instance_id: &str,
    node_stack: &Arc<NodeStack>,
    node_name: &str,
    node_tag: &str,
    instance_ids: &[Name],
) {
    if instance_ids.is_empty() {
        return;
    }
    // Resolve pid + is_container for every instance under ONE short read lock,
    // not held across the await below. Reading the pids up front (rather than
    // after shutdown) avoids a race with any concurrent registry mutation. An
    // instance that is no longer tracked resolves to a pid-less target: the
    // cooperative send is still attempted, and there is nothing to kill.
    let doomed: Vec<DoomedInstance> = match node_stack.find(node_name, node_tag) {
        Some(handle) => {
            let guard = handle.read();
            let is_container = guard.config().execution.container.is_some();
            instance_ids
                .iter()
                .map(|instance_id| {
                    // Claim each instance for termination before it is signaled,
                    // so its exit watcher leaves the state to this path's removal
                    // and does not record an intentional stop as a self-exit.
                    let pid = guard
                        .instances()
                        .iter()
                        .find(|inst| inst.instance_id() == instance_id)
                        .inspect(|inst| inst.mark_stopping())
                        .and_then(|inst| inst.pid());
                    DoomedInstance {
                        node_name: node_name.to_owned(),
                        node_tag: node_tag.to_owned(),
                        instance_id: instance_id.clone(),
                        pid,
                        is_container,
                    }
                })
                .collect()
        }
        None => return, // Entity already gone; nothing to stop.
    };

    // force_stop_instances warns (daemon-side) when it has to force-kill, which
    // is the right surface for this overwrite path (the user sees node_add's
    // feedback stream; node_stop additionally relays force-kill to the CLI).
    force_stop_instances(
        messenger,
        core_node_node,
        core_instance_id,
        &doomed,
        node_stack.shutdown_grace(),
    )
    .await;

    for instance_id in instance_ids {
        remove_instance_from_registry(node_stack, node_name, node_tag, instance_id);
    }
}

/// Bounded wait for SIGKILLed groups to be reaped before the daemon exits, so
/// they don't briefly linger as zombies parented to the still-alive daemon.
/// (The cooperative-phase grace window is configurable via
/// `peppy_config.lifecycle.shutdown_grace_secs` and carried on the `NodeStack`;
/// only this reap budget is a fixed internal constant.) Public so the CLI can
/// derive its request timeout from the daemon's worst-case stop duration
/// (configured grace + this reap) instead of guessing at it.
pub const TEARDOWN_REAP_BUDGET: Duration = Duration::from_secs(2);

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

/// A non-root node instance to terminate: the routing identity + process info
/// needed to stop one instance. Fed to [`force_stop_instances`] in a batch by
/// [`teardown_all_instances`] and [`stop_instances`] (the `node add` overwrite
/// path), and one at a time by [`force_stop_instance`] (`peppy node stop`).
struct DoomedInstance {
    node_name: String,
    node_tag: String,
    instance_id: Name,
    pid: Option<u32>,
    /// `true` when this instance is a container node. On macOS the workload runs
    /// inside the Lima VM, so host process-group signals only reach the
    /// `limactl` client; the stop path also signals the in-VM group keyed by
    /// `instance_id` (SIGTERM after a failed shutdown request, SIGKILL in the
    /// force phase). Always `false` for process nodes (the host group kill
    /// covers them) and a no-op for containers on Linux.
    is_container: bool,
}

/// Force every non-root node out of the stack on a catchable daemon shutdown
/// (ctrl+C / SIGTERM), so no node (or any descendant it spawned) outlives the
/// daemon as an orphan.
///
/// Graceful-then-force: first asks each node to shut down cooperatively (the
/// same `SHUTDOWN_SERVICE` path as `peppy node stop`, so robot nodes can stop
/// actuators cleanly), gives them a short shared budget to exit, then SIGKILLs
/// the process group of anything still alive. The root (core) node is skipped;
/// its pid is the daemon itself. This does NOT wait the uncatchable-death grace
/// period; that timer only governs a daemon that died without running cleanup.
///
/// Delegates to [`force_stop_instances`]: the same phases as `peppy node
/// stop`, batched: one grace budget shared across all instances and every
/// cooperative shutdown sent concurrently.
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
    force_stop_instances(
        messenger,
        core_node_name,
        core_instance_id,
        &doomed,
        // Configurable cooperative-shutdown grace (peppy_config.lifecycle
        // .shutdown_grace_secs), pinned on the stack at daemon startup.
        node_stack.shutdown_grace(),
    )
    .await;
}

/// Snapshot every non-root instance (both `Running` and `Starting`; a
/// `Starting` instance already has a live child) with its routing identity and
/// pid. Skips the root entity by pointer identity.
///
/// Each collected instance is claimed via `mark_stopping()` so its exit watcher
/// treats the upcoming kill as intentional and does not relabel it a crash. The
/// daemon-shutdown caller does not strictly need this (its watchers bail on the
/// shutdown token), but the reset and launch-clear callers do: they tear the
/// stack down while the daemon keeps running, so without the claim each
/// force-killed instance would be recorded as `Failed` by its watcher.
fn collect_doomed_instances(node_stack: &Arc<NodeStack>) -> Vec<DoomedInstance> {
    let root = node_stack.root();
    let mut doomed = Vec::new();
    for handle in node_stack.snapshot() {
        if Arc::ptr_eq(&handle, &root) {
            continue;
        }
        let guard = handle.read();
        let node_name = guard.config().manifest.name.as_str().to_owned();
        let node_tag = guard.config().manifest.tag.clone();
        let is_container = guard.config().execution.container.is_some();
        for inst in guard.instances() {
            // Claim before termination so the exit watcher leaves removal to the
            // teardown and never records an intentional kill as a self-exit.
            inst.mark_stopping();
            doomed.push(DoomedInstance {
                node_name: node_name.clone(),
                node_tag: node_tag.clone(),
                instance_id: inst.instance_id().clone(),
                pid: inst.pid(),
                is_container,
            });
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

/// SIGKILLs the entire process group led by `pid`. Nodes are spawned as group
/// leaders (PGID == PID; see `node_stack::run_steps::spawn_process_node`), so a
/// negative-pid signal reaches the node and every descendant in its group.
/// `setpgid`/`killpg` semantics are identical on Linux and macOS.
#[cfg(unix)]
fn kill_process_group(pid: u32) {
    signal_process_group(pid, nix::sys::signal::Signal::SIGKILL, "SIGKILL");
}

#[cfg(not(unix))]
fn kill_process_group(_pid: u32) {}

/// SIGTERMs the entire process group led by `pid`. Used only as a cooperative
/// fallback for native container runtimes when the Peppy shutdown RPC could not
/// be delivered; the force phase still SIGKILLs any process group that remains.
#[cfg(unix)]
fn terminate_process_group(pid: u32) {
    signal_process_group(pid, nix::sys::signal::Signal::SIGTERM, "SIGTERM");
}

#[cfg(not(unix))]
fn terminate_process_group(_pid: u32) {}

#[cfg(unix)]
fn signal_process_group(pid: u32, signal: nix::sys::signal::Signal, signal_name: &str) {
    // `killpg(pgrp, sig)` is POSIX-equivalent to `kill(-pgrp, sig)`: it targets
    // the process group whose PGID == `pid`. Using nix's safe wrapper keeps the
    // crate free of `unsafe`.
    match nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid as i32), signal) {
        // An already-dead group yields ESRCH; the group is already gone, which
        // is exactly the state we wanted, so treat it as success.
        Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
        // Any other errno (e.g. EPERM) means the signal did not land and the
        // node's process group may still be alive, so surface it.
        Err(err) => warn!(
            "Failed to {} node process group (pid {}): {}",
            signal_name, pid, err
        ),
    }
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

/// Whether `pid` is present and not a zombie in an already-refreshed `system`.
fn pid_running_in(system: &sysinfo::System, pid: u32) -> bool {
    match system.process(sysinfo::Pid::from_u32(pid)) {
        Some(process) => process.status() != sysinfo::ProcessStatus::Zombie,
        None => false,
    }
}
