use crate::Result;
use crate::encoding::{NodeStopRequest, NodeStopResponse};
use crate::names;
use config::node::Name;
use node_stack::NodeStack;
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
            return NodeStopResponse::failure(format!("Invalid instance_id: {}", e)).encode();
        }
    };

    // Find the instance and entity by instance_id
    let instance = match node_stack.find_by_instance_id(&instance_id) {
        Some(instance) => instance,
        None => {
            return NodeStopResponse::failure(format!(
                "Node instance '{}' not found in node stack",
                request.instance_id
            ))
            .encode();
        }
    };

    let entity_handle = match node_stack.find_entity_by_instance_id(&instance_id) {
        Some(entity) => entity,
        None => {
            return NodeStopResponse::failure(format!(
                "Node instance '{}' not found in node stack",
                request.instance_id
            ))
            .encode();
        }
    };

    // Get the PID for later verification (if available)
    let pid = instance.pid();

    let (node_name, node_tag) = {
        let guard = entity_handle.read().expect("entity poisoned");
        (
            guard.config().manifest.name.as_str().to_owned(),
            guard.config().manifest.tag.clone(),
        )
    };
    let root_node_name = node_stack
        .root()
        .read()
        .expect("entity poisoned")
        .config()
        .manifest
        .name
        .as_str()
        .to_owned();

    if node_name == root_node_name {
        return NodeStopResponse::failure("Cannot stop the core node").encode();
    }

    // Send shutdown signal and remove instance
    if let Err(e) = stop_instance(
        messenger,
        core_node_node,
        core_instance_id,
        &node_stack,
        &node_name,
        &node_tag,
        &instance_id,
    )
    .await
    {
        debug!(
            "Failed to shutdown node instance '{}': {}",
            request.instance_id, e
        );
        return NodeStopResponse::failure(e).encode();
    }

    // Verify the process has actually terminated (if we have a PID)
    if let Some(pid) = pid {
        if !wait_for_process_termination(pid).await {
            return NodeStopResponse::failure(format!(
                "Process {} for node instance '{}' did not terminate within timeout",
                pid, request.instance_id
            ))
            .encode();
        }
        debug!(
            "Process {} for node instance '{}' has terminated",
            pid, request.instance_id
        );
    }

    NodeStopResponse::success().encode()
}

/// Sends a `SHUTDOWN_SERVICE` signal to a running node instance and removes it from the
/// node stack. The entity remains in the graph with zero instances, preserving dependency
/// edges so that a subsequent `push_config` call can correctly validate interface changes
/// against dependents.
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

    debug!(
        "Sending shutdown request to node instance '{}'",
        instance_id_str
    );

    ServiceMessenger::poll(
        messenger,
        core_node_node,
        core_instance_id,
        node_name,
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

    if let Some(handle) = node_stack.find(node_name, node_tag) {
        let removed = handle
            .write()
            .expect("entity poisoned")
            .stop_instance(instance_id);
        if !removed {
            debug!(
                "Node instance '{}' was not tracked in entity {}:{}",
                instance_id_str, node_name, node_tag
            );
        }
    }

    Ok(())
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
