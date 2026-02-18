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
    daemon_node_node: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
) -> Result<JoinHandle<Result<()>>> {
    let daemon_node_node = daemon_node_node.to_string();
    let daemon_instance_id = instance_id.to_string();
    let messenger = messenger.clone();

    let mut endpoint = ServiceMessenger::listen(
        &messenger,
        &daemon_node_node,
        &daemon_instance_id,
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
                    daemon_node_node.clone(),
                    daemon_instance_id.clone(),
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
    daemon_node_node: String,
    daemon_instance_id: String,
    node_stack: Arc<NodeStack>,
) -> PeppyResult<Payload> {
    let sender_instance_id = context.message().instance_id();
    handle_node_stop_request_inner(
        &context,
        &messenger,
        &daemon_node_node,
        &daemon_instance_id,
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
    daemon_node_node: &str,
    daemon_instance_id: &str,
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

    let entity = match node_stack.find_entity_by_instance_id(&instance_id) {
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

    let root_node_name = node_stack.root().config().manifest.name.as_str().to_owned();
    let node_name = entity.config().manifest.name.as_str().to_owned();

    if node_name == root_node_name {
        return NodeStopResponse::failure("Cannot stop the daemon node").encode();
    }

    let node_tag = entity.config().manifest.tag.clone();
    let node_config = entity.config().clone();
    let node_root_path = entity.root_path().to_path_buf();

    // Send a shutdown request to the node
    debug!(
        "Sending shutdown request to node instance '{}'",
        request.instance_id
    );

    let shutdown_result = ServiceMessenger::poll(
        messenger,
        daemon_node_node,
        daemon_instance_id,
        node_name.as_str(),
        SHUTDOWN_SERVICE,
        Some(daemon_node_node),
        Some(&request.instance_id),
        Payload::from_static(b"shutdown"),
        SHUTDOWN_TIMEOUT,
    )
    .await;

    match shutdown_result {
        Ok(_) => {
            debug!(
                "Node instance '{}' shutdown acknowledged",
                request.instance_id
            );

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

            match node_stack.remove_instance(&node_name, &node_tag, &instance_id) {
                Ok(true) => {}
                Ok(false) => {
                    return NodeStopResponse::failure(format!(
                        "Node instance '{}' not found in node stack",
                        request.instance_id
                    ))
                    .encode();
                }
                Err(e) => {
                    return NodeStopResponse::failure(format!(
                        "Failed to remove node instance '{}' from node stack: {}",
                        request.instance_id, e
                    ))
                    .encode();
                }
            }

            // `remove_instance` may remove the entity entirely if it was the last instance.
            // Re-push the config so the node remains in the stack with 0 instances.
            if let Err(e) = node_stack.push_config(node_config, false, node_root_path) {
                return NodeStopResponse::failure(format!(
                    "Failed to keep node config for '{}:{}' in node stack: {}",
                    node_name, node_tag, e
                ))
                .encode();
            }

            NodeStopResponse::success().encode()
        }
        Err(e) => {
            debug!(
                "Failed to shutdown node instance '{}': {}",
                request.instance_id, e
            );
            NodeStopResponse::failure(format!("Failed to shutdown node: {}", e)).encode()
        }
    }
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
