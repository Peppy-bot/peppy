use crate::Result;
use crate::names;
use config::node::Name;
use core_node_api::encoding::{NodeStopRequest, NodeStopResponse};
use node_stack::NodeStack;
use peppylib::messaging::SenderTarget;
use peppylib::messaging::{SHUTDOWN_SERVICE, ServiceMessenger, ServiceRequestContext};
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyError, PeppyResult};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(100);

pub async fn listen_for_node_stop(
    messenger: &MessengerHandle,
    core_node_node: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
) -> Result<JoinHandle<Result<()>>> {
    let core_node_node = core_node_node.to_string();
    let core_instance_id = instance_id.to_string();
    let messenger = messenger.clone();

    let mut endpoint = ServiceMessenger::listen(
        &messenger,
        &core_node_node,
        &core_instance_id,
        SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
        names::NODE_STOP,
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
) -> PeppyResult<Payload> {
    let sender_instance_id = context.message().instance_id();
    handle_node_stop_request_inner(
        &context,
        &messenger,
        &core_node_node,
        &core_instance_id,
        node_stack,
    )
    .await
    .map_err(|e| PeppyError::InvalidServiceRequest {
        identifier: sender_instance_id.to_string(),
        reason: e.to_string(),
    })
}

async fn handle_node_stop_request_inner(
    context: &ServiceRequestContext,
    messenger: &MessengerHandle,
    core_node_node: &str,
    core_instance_id: &str,
    node_stack: Arc<NodeStack>,
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

    // Find the instance and entity by instance_id. The lookup helpers
    // intentionally only match `Running` instances. If the instance is
    // mid-start (`Starting`), the lookup returns `None` even though the
    // instance does exist — surface that as a distinct retryable error so
    // callers can back off and retry once `commit_started`/`abort_started`
    // has resolved the in-flight start.
    let instance = match node_stack.find_by_instance_id(&instance_id) {
        Some(instance) => instance,
        None => {
            if instance_is_in_starting(&node_stack, &instance_id) {
                return NodeStopResponse::failure(format!(
                    "Node instance '{}' is in Starting state; retry after the start completes",
                    request.instance_id
                ))
                .encode()
                .map_err(Into::into);
            }
            return NodeStopResponse::failure(format!(
                "Node instance '{}' not found in node stack",
                request.instance_id
            ))
            .encode()
            .map_err(Into::into);
        }
    };

    let entity_handle = match node_stack.find_entity_by_instance_id(&instance_id) {
        Some(entity) => entity,
        None => {
            if instance_is_in_starting(&node_stack, &instance_id) {
                return NodeStopResponse::failure(format!(
                    "Node instance '{}' is in Starting state; retry after the start completes",
                    request.instance_id
                ))
                .encode()
                .map_err(Into::into);
            }
            return NodeStopResponse::failure(format!(
                "Node instance '{}' not found in node stack",
                request.instance_id
            ))
            .encode()
            .map_err(Into::into);
        }
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

    // Process is gone (properly or improperly). Finalize the registry removal.
    remove_instance_from_registry(&node_stack, &node_name, &node_tag, &instance_id);

    // Tell the caller whether the node exited gracefully or had to be
    // force-killed, so the CLI can warn the user about the latter.
    let response = match outcome {
        StopOutcome::ForceKilled => NodeStopResponse::success_force_killed(),
        StopOutcome::Graceful | StopOutcome::NoProcess => NodeStopResponse::success(),
    };
    response.encode().map_err(Into::into)
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
/// acknowledges. Does NOT touch the entity's instances list — that is done
/// only after [`force_stop_instance`] has confirmed the OS process is gone, so
/// retries can still find the live instance during an in-flight shutdown.
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
        Some(core_node_node),
        Some(instance_id_str),
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

/// How a single instance ended up stopped — surfaced to the user so a
/// force-kill (the node ignored the cooperative shutdown) is distinguishable
/// from a clean graceful exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopOutcome {
    /// The node exited on its own within the grace period.
    Graceful,
    /// The node did not exit in time and its process group was SIGKILLed.
    ForceKilled,
    /// No tracked OS process — nothing to wait on or kill.
    NoProcess,
}

/// Cooperative-then-force termination of a single instance — the single-instance
/// sibling of [`teardown_all_instances`] (the SIGINT/ctrl+C path), running the
/// same phases for one pid so `peppy node stop` behaves identically to a
/// catchable daemon shutdown ("properly OR improperly stopped"):
///
/// 1. Best-effort `SHUTDOWN_SERVICE` send — a failed/timed-out send is logged
///    and ignored. A non-responsive node must still be force-killed, never
///    surfaced as an error (this is what makes a stuck node a success, not a
///    failure-with-the-process-left-alive).
/// 2. Bounded graceful wait (`graceful_budget`, from
///    `peppy_config.lifecycle.shutdown_grace_secs`) for the OS process to exit
///    on its own.
/// 3. If still alive, `kill_process_group(pid)` (SIGKILL the whole group — nodes
///    are spawned as group leaders) plus, on macOS for container nodes, the
///    in-VM guest group kill.
/// 4. Bounded reap ([`TEARDOWN_REAP_BUDGET`]) so the SIGKILLed group is gone
///    before we return.
///
/// Returns a [`StopOutcome`] so the caller can tell the user whether the node
/// exited gracefully or had to be force-killed. `target.pid == None` means there
/// is no OS process to terminate (the cooperative send is still attempted
/// best-effort) and yields [`StopOutcome::NoProcess`]. Takes NO lock — the
/// caller resolves the [`DoomedInstance`] fields and `graceful_budget` up front,
/// so nothing is held across an await.
async fn force_stop_instance(
    messenger: &MessengerHandle,
    core_node_node: &str,
    core_instance_id: &str,
    target: &DoomedInstance,
    graceful_budget: Duration,
) -> StopOutcome {
    let instance_id = &target.instance_id;

    // Phase 1 (graceful, bounded): cooperative shutdown, then wait for exit.
    // Best-effort send: ignore failure/timeout and fall through to force-kill.
    let _ = tokio::time::timeout(graceful_budget, async {
        if let Err(e) = send_shutdown_signal(
            messenger,
            core_node_node,
            core_instance_id,
            &target.node_name,
            &target.node_tag,
            instance_id,
        )
        .await
        {
            debug!(
                "Cooperative shutdown of node instance '{}' failed; \
                 falling through to force-kill: {}",
                instance_id.as_str(),
                e
            );
        }
        if let Some(pid) = target.pid {
            wait_for_pid_gone(pid).await;
        }
    })
    .await;

    let Some(pid) = target.pid else {
        return StopOutcome::NoProcess; // No OS process to force-kill or reap.
    };

    // Decide the outcome by whether the leader is still alive after the grace
    // window. We still SIGKILL the group unconditionally below (a group can
    // outlive its leader; the negative-pid kill reaches survivors, and an
    // already-dead group yields ESRCH, which `kill_process_group` ignores) —
    // the liveness check here only classifies graceful vs forced for the user.
    let outcome = if is_process_running(pid) {
        warn!(
            "Node instance '{}' did not exit within the {}s shutdown grace period; \
             force-killing its process group (pid {})",
            instance_id.as_str(),
            graceful_budget.as_secs(),
            pid
        );
        StopOutcome::ForceKilled
    } else {
        debug!(
            "Node instance '{}' exited gracefully within the grace period",
            instance_id.as_str()
        );
        StopOutcome::Graceful
    };
    kill_process_group(pid);

    // Phase 2b (macOS): the host group kill above only reached the `limactl`
    // client for container nodes; reach into the Lima VM and SIGKILL the guest
    // group keyed by instance id (matching the `cancel_pgid` set at spawn). Runs
    // on a blocking thread because it shells out to `limactl`. Best-effort: a
    // facade-init failure must not block the stop. Compiled out / no-op on Linux.
    if cfg!(target_os = "macos") && target.is_container {
        let key = instance_id.as_str().to_owned();
        let _ = tokio::task::spawn_blocking(move || match containers::Apptainer::new() {
            Ok(apptainer) => {
                let _ = apptainer.kill_guest_process_group(&key);
            }
            Err(e) => {
                debug!("Skipping in-VM container kill (apptainer init failed): {e}");
            }
        })
        .await;
    }

    // Phase 3 (bounded reap): let the killed group die before we return so it
    // doesn't briefly linger as a zombie parented to the still-alive daemon.
    let _ = tokio::time::timeout(TEARDOWN_REAP_BUDGET, wait_for_pid_gone(pid)).await;

    outcome
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

/// Cooperative-then-force stop of a single running node instance, then removes
/// it from the node stack. The entity remains in the graph with zero instances,
/// preserving dependency edges so that a subsequent `push_config` call can
/// correctly validate interface changes against dependents.
///
/// Shares [`force_stop_instance`] with `handle_node_stop_request_inner` and the
/// SIGINT teardown, so a stuck instance is force-killed (not left alive behind a
/// timeout error): the cooperative shutdown is attempted first, then the process
/// group is SIGKILLed if the node ignores it, and the call returns only once the
/// process is gone. Returns `Result` purely so callers can keep `?`; it is
/// effectively infallible.
pub(super) async fn stop_instance(
    messenger: &MessengerHandle,
    core_node_node: &str,
    core_instance_id: &str,
    node_stack: &Arc<NodeStack>,
    node_name: &str,
    node_tag: &str,
    instance_id: &Name,
) -> std::result::Result<(), String> {
    // Resolve pid + is_container from the live entry under one short read lock,
    // not held across the await below. Reading the pid up front (rather than
    // after shutdown) avoids a race with any concurrent registry mutation.
    let (pid, is_container) = match node_stack.find(node_name, node_tag) {
        Some(handle) => {
            let guard = handle.read();
            let is_container = guard.config().execution.container.is_some();
            let pid = guard
                .instances()
                .iter()
                .find(|inst| inst.instance_id() == instance_id)
                .and_then(|inst| inst.pid());
            (pid, is_container)
        }
        None => return Ok(()), // Entity already gone; nothing to stop.
    };

    let target = DoomedInstance {
        node_name: node_name.to_owned(),
        node_tag: node_tag.to_owned(),
        instance_id: instance_id.clone(),
        pid,
        is_container,
    };
    // force_stop_instance warns (daemon-side) when it has to force-kill, which
    // is the right surface for this overwrite path (the user sees node_add's
    // feedback stream; node_stop additionally relays force-kill to the CLI).
    force_stop_instance(
        messenger,
        core_node_node,
        core_instance_id,
        &target,
        node_stack.shutdown_grace(),
    )
    .await;

    remove_instance_from_registry(node_stack, node_name, node_tag, instance_id);
    Ok(())
}

/// Bounded wait for SIGKILLed groups to be reaped before the daemon exits, so
/// they don't briefly linger as zombies parented to the still-alive daemon.
/// (The cooperative-phase grace window is configurable via
/// `peppy_config.lifecycle.shutdown_grace_secs` and carried on the `NodeStack`;
/// only this reap budget is a fixed internal constant.)
const TEARDOWN_REAP_BUDGET: Duration = Duration::from_secs(2);

/// A non-root node instance to terminate — the routing identity + process info
/// needed to stop one instance. Used both by the batched [`teardown_all_instances`]
/// and, one at a time, by [`force_stop_instance`] (`peppy node stop` / overwrite).
struct DoomedInstance {
    node_name: String,
    node_tag: String,
    instance_id: Name,
    pid: Option<u32>,
    /// `true` when this instance is a container node. On macOS the workload runs
    /// inside the Lima VM, so the host process-group SIGKILL only reaches the
    /// `limactl` client; the force phase additionally SIGKILLs the in-VM group
    /// keyed by `instance_id`. Always `false` for process nodes (the host group
    /// kill covers them) and a no-op for containers on Linux.
    is_container: bool,
}

/// Force every non-root node out of the stack on a catchable daemon shutdown
/// (ctrl+C / SIGTERM), so no node — or any descendant it spawned — outlives the
/// daemon as an orphan.
///
/// Graceful-then-force: first asks each node to shut down cooperatively (the
/// same `SHUTDOWN_SERVICE` path as `peppy node stop`, so robot nodes can stop
/// actuators cleanly), gives them a short shared budget to exit, then SIGKILLs
/// the process group of anything still alive. The root (core) node is skipped —
/// its pid is the daemon itself. This does NOT wait the uncatchable-death grace
/// period; that timer only governs a daemon that died without running cleanup.
///
/// The single-instance sibling is [`force_stop_instance`], which `peppy node
/// stop` and the `node add` overwrite path use; this batched form is kept
/// separate because it shares one grace budget across all instances and sends
/// every shutdown concurrently, which a per-instance loop would not.
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
    // Configurable cooperative-shutdown grace (peppy_config.lifecycle
    // .shutdown_grace_secs), pinned on the stack at daemon startup.
    let graceful_budget = node_stack.shutdown_grace();
    debug!(
        "Tearing down {} node instance(s) on daemon shutdown",
        doomed.len()
    );

    // Phase 1 (graceful, bounded): ask every node to shut down cooperatively,
    // concurrently, then poll until they're all gone or the budget elapses.
    let _ = tokio::time::timeout(graceful_budget, async {
        let sends = doomed.iter().map(|d| {
            send_shutdown_signal(
                messenger,
                core_node_name,
                core_instance_id,
                &d.node_name,
                &d.node_tag,
                &d.instance_id,
            )
        });
        let _ = futures::future::join_all(sends).await;
        wait_until_all_gone(&doomed).await;
    })
    .await;

    // Phase 2 (force): SIGKILL each recorded process group. We deliberately do
    // not pre-check whether the leader is still alive before killing: a process
    // group can outlive its leader (the leader exits but a descendant it spawned
    // keeps running), and the negative-pid kill reaches the whole group (each
    // node is spawned as a group leader), so descendants die too. An already-dead
    // group yields ESRCH, which kill_process_group ignores. For container nodes,
    // also record the instance id so the in-VM group can be killed below (macOS).
    //
    // We do take one snapshot here purely to classify which nodes are still
    // alive (i.e. ignored the cooperative shutdown) so the user is warned that
    // they were force-killed rather than having exited gracefully.
    let snapshot = process_snapshot();
    let mut container_kill_keys: Vec<String> = Vec::new();
    for d in &doomed {
        if let Some(pid) = d.pid {
            if pid_running_in(&snapshot, pid) {
                warn!(
                    "Node instance '{}' did not exit within the {}s shutdown grace period; \
                     force-killing its process group (pid {})",
                    d.instance_id.as_str(),
                    graceful_budget.as_secs(),
                    pid
                );
            } else {
                debug!(
                    "Node instance '{}' exited gracefully on shutdown",
                    d.instance_id.as_str()
                );
            }
            kill_process_group(pid);
            if d.is_container {
                container_kill_keys.push(d.instance_id.as_str().to_owned());
            }
        }
    }

    // Phase 2b (macOS): for container nodes the host group kill above only reached
    // the `limactl` client; the workload runs inside the Lima VM. Reach into the
    // VM and SIGKILL each still-running container's recorded guest process group
    // (keyed by instance id, matching the `cancel_pgid` set at spawn). Skipped on
    // Linux, where the host kill already reached the shared-namespace container.
    // Best-effort: a facade-init failure must not block teardown (the host kill
    // already ran, and a non-responsive node is backstopped by its in-container
    // daemon-liveness watchdog). Runs on a blocking thread because the guest kill
    // shells out to `limactl`.
    if cfg!(target_os = "macos") && !container_kill_keys.is_empty() {
        let _ = tokio::task::spawn_blocking(move || match containers::Apptainer::new() {
            Ok(apptainer) => {
                for key in &container_kill_keys {
                    let _ = apptainer.kill_guest_process_group(key);
                }
            }
            Err(e) => {
                debug!("Skipping in-VM container teardown (apptainer init failed): {e}");
            }
        })
        .await;
    }

    // Phase 3 (bounded reap): let the killed groups die while the daemon is
    // still alive so they don't show as zombies; any straggler reparents to
    // init/launchd and is reaped there.
    let _ = tokio::time::timeout(TEARDOWN_REAP_BUDGET, wait_until_all_gone(&doomed)).await;
}

/// Snapshot every non-root instance (both `Running` and `Starting` — a
/// `Starting` instance already has a live child) with its routing identity and
/// pid. Skips the root entity by pointer identity.
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

/// Polls until every instance with a known pid has exited. Builds a single
/// process snapshot per poll and checks all pids against it, rather than one
/// snapshot per pid (which would rescan every process on the machine N times a
/// poll for N instances).
async fn wait_until_all_gone(doomed: &[DoomedInstance]) {
    loop {
        let system = process_snapshot();
        if doomed
            .iter()
            .filter_map(|d| d.pid)
            .all(|pid| !pid_running_in(&system, pid))
        {
            return;
        }
        tokio::time::sleep(PROCESS_POLL_INTERVAL).await;
    }
}

/// Polls until `pid` has exited (or become a zombie). Unbounded on its own —
/// callers wrap it in `tokio::time::timeout` to apply a budget, matching how
/// [`wait_until_all_gone`] is used by [`teardown_all_instances`]. Kept distinct
/// from a self-bounded poll so `force_stop_instance` can apply different graceful
/// and reap budgets to the same primitive.
async fn wait_for_pid_gone(pid: u32) {
    while is_process_running(pid) {
        tokio::time::sleep(PROCESS_POLL_INTERVAL).await;
    }
}

/// SIGKILLs the entire process group led by `pid`. Nodes are spawned as group
/// leaders (PGID == PID; see `node_stack::run_steps::spawn_process_node`), so a
/// negative-pid signal reaches the node and every descendant in its group.
/// `setpgid`/`killpg` semantics are identical on Linux and macOS.
#[cfg(unix)]
fn kill_process_group(pid: u32) {
    // SAFETY: a plain `kill(2)` syscall with no memory effects. A negative pid
    // targets the process group `pid`. An already-dead group yields ESRCH,
    // which we ignore.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_process_group(_pid: u32) {}

/// Snapshots the machine's processes (existence + status only). Building this is
/// not free (it rescans every process), so poll loops over many pids should take
/// one snapshot and reuse it via [`pid_running_in`] rather than calling
/// [`is_process_running`] per pid.
fn process_snapshot() -> sysinfo::System {
    sysinfo::System::new_with_specifics(
        sysinfo::RefreshKind::nothing().with_processes(sysinfo::ProcessRefreshKind::nothing()),
    )
}

/// Whether `pid` is present and not a zombie in an already-taken `system`
/// snapshot.
fn pid_running_in(system: &sysinfo::System, pid: u32) -> bool {
    match system.process(sysinfo::Pid::from_u32(pid)) {
        Some(process) => process.status() != sysinfo::ProcessStatus::Zombie,
        None => false,
    }
}

/// Checks if a process with the given PID is still running.
fn is_process_running(pid: u32) -> bool {
    pid_running_in(&process_snapshot(), pid)
}
