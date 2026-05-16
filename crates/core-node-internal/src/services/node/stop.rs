use crate::Result;
use crate::names;
use config::node::Name;
use core_node_api::encoding::{NodeStopRequest, NodeStopResponse};
use node_stack::NodeStack;
use peppylib::messaging::{NATIVE_IFACE_SEGMENT_NAME, NATIVE_IFACE_SEGMENT_TAG};
use peppylib::messaging::{SHUTDOWN_SERVICE, ServiceMessenger, ServiceRequestContext};
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyError, PeppyResult};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::debug;

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const PROCESS_TERMINATION_TIMEOUT: Duration = Duration::from_secs(5);
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
        node_name,
        NATIVE_IFACE_SEGMENT_NAME,
        NATIVE_IFACE_SEGMENT_TAG,
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

    // Get the PID for later verification (if available)
    let pid = instance.pid();

    let (node_name, node_tag) = {
        let guard = entity_handle.read();
        (
            guard.config().manifest.name.as_str().to_owned(),
            guard.config().manifest.tag.clone(),
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

    // Step 1: send the shutdown signal — but do NOT remove the instance from
    // the entity's registry yet. We must keep the live entry so a retried or
    // racing caller can still observe the in-flight shutdown, and so we can
    // observe the actual process termination via PID polling below.
    if let Err(e) = send_shutdown_signal(
        messenger,
        core_node_node,
        core_instance_id,
        &node_name,
        &instance_id,
    )
    .await
    {
        debug!(
            "Failed to shutdown node instance '{}': {}",
            request.instance_id, e
        );
        return NodeStopResponse::failure(e).encode().map_err(Into::into);
    }

    // Step 2: verify the process has actually terminated (if we have a PID).
    // Only on confirmed termination do we remove the instance from the
    // registry, so a timeout/failure leaves the entry in place for retries.
    if let Some(pid) = pid {
        if !wait_for_process_termination(pid).await {
            return NodeStopResponse::failure(format!(
                "Process {} for node instance '{}' did not terminate within timeout",
                pid, request.instance_id
            ))
            .encode()
            .map_err(Into::into);
        }
        debug!(
            "Process {} for node instance '{}' has terminated",
            pid, request.instance_id
        );
    }

    // Step 3: confirmed termination — now finalize the registry removal.
    if let Err(e) = remove_instance_from_registry(&node_stack, &node_name, &node_tag, &instance_id)
    {
        return NodeStopResponse::failure(e).encode().map_err(Into::into);
    }

    NodeStopResponse::success().encode().map_err(Into::into)
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
/// only after `wait_for_process_termination` has confirmed the OS process is
/// gone, so retries can still find the live instance on failure.
async fn send_shutdown_signal(
    messenger: &MessengerHandle,
    core_node_node: &str,
    core_instance_id: &str,
    node_name: &str,
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
        node_name,
        NATIVE_IFACE_SEGMENT_NAME,
        NATIVE_IFACE_SEGMENT_TAG,
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

/// Removes a `Running` instance from the entity's registry. Called only
/// after the OS process has been confirmed terminated.
fn remove_instance_from_registry(
    node_stack: &Arc<NodeStack>,
    node_name: &str,
    node_tag: &str,
    instance_id: &Name,
) -> std::result::Result<(), String> {
    let Some(handle) = node_stack.find(node_name, node_tag) else {
        return Ok(());
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
    Ok(())
}

/// Sends a `SHUTDOWN_SERVICE` signal to a running node instance, waits for the
/// underlying OS process to actually exit, and then removes the instance from
/// the node stack. The entity remains in the graph with zero instances,
/// preserving dependency edges so that a subsequent `push_config` call can
/// correctly validate interface changes against dependents.
///
/// Mirrors the three-step contract used by `handle_node_stop_request_inner`:
/// 1. Send shutdown and wait for ACK, without touching the registry.
/// 2. Verify the PID is gone via `wait_for_process_termination`, so a
///    slow-to-die child cannot be dropped from tracking while still running.
/// 3. Only after confirmed termination, remove the entry via
///    `remove_instance_from_registry`.
pub(super) async fn stop_instance(
    messenger: &MessengerHandle,
    core_node_node: &str,
    core_instance_id: &str,
    node_stack: &Arc<NodeStack>,
    node_name: &str,
    node_tag: &str,
    instance_id: &Name,
) -> std::result::Result<(), String> {
    let instance_id_str = instance_id.as_str();

    // Capture the PID up front so we can verify termination after the ACK.
    // Reading it from the live entry (rather than after shutdown) avoids a
    // race with any concurrent registry mutation and matches the shape of
    // `handle_node_stop_request_inner`.
    let pid = node_stack.find(node_name, node_tag).and_then(|handle| {
        handle
            .read()
            .instances()
            .iter()
            .find(|inst| inst.instance_id() == instance_id)
            .and_then(|inst| inst.pid())
    });

    send_shutdown_signal(
        messenger,
        core_node_node,
        core_instance_id,
        node_name,
        instance_id,
    )
    .await?;

    if let Some(pid) = pid
        && !wait_for_process_termination(pid).await
    {
        return Err(format!(
            "Process {} for node instance '{}' did not terminate within timeout",
            pid, instance_id_str
        ));
    }

    remove_instance_from_registry(node_stack, node_name, node_tag, instance_id)
}

/// Waits for a process to terminate, polling at regular intervals.
/// Returns `true` if the process has terminated, `false` if it's still running after the timeout.
async fn wait_for_process_termination(pid: u32) -> bool {
    let deadline = tokio::time::Instant::now() + PROCESS_TERMINATION_TIMEOUT;

    while tokio::time::Instant::now() < deadline {
        if !is_process_running(pid) {
            return true;
        }
        tokio::time::sleep(PROCESS_POLL_INTERVAL).await;
    }

    // Final check after timeout
    !is_process_running(pid)
}

/// Checks if a process with the given PID is still running.
fn is_process_running(pid: u32) -> bool {
    let system = sysinfo::System::new_with_specifics(
        sysinfo::RefreshKind::nothing().with_processes(sysinfo::ProcessRefreshKind::nothing()),
    );
    match system.process(sysinfo::Pid::from_u32(pid)) {
        Some(process) => process.status() != sysinfo::ProcessStatus::Zombie,
        None => false,
    }
}
