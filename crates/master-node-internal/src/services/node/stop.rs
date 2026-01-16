use crate::Result;
use crate::encoding::{NodeStopRequest, NodeStopResponse};
use crate::names;
use bytes::Bytes;
use config::node::Name;
use node_stack::NodeStack;
use peppylib::messaging::{SHUTDOWN_SERVICE, ServiceMessenger, ServiceRequestContext};
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
    master_node_node: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
) -> Result<JoinHandle<Result<()>>> {
    let master_node_node = master_node_node.to_string();
    let master_instance_id = instance_id.to_string();
    let messenger = messenger.clone();

    let mut endpoint = ServiceMessenger::listen(
        &messenger,
        &master_node_node,
        &master_instance_id,
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
                    master_node_node.clone(),
                    master_instance_id.clone(),
                    node_stack.clone(),
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
    master_node_node: String,
    master_instance_id: String,
    node_stack: Arc<NodeStack>,
) -> PeppyResult<Bytes> {
    let sender_instance_id = context.message().instance_id();
    handle_node_stop_request_inner(
        &context,
        &messenger,
        &master_node_node,
        &master_instance_id,
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
    master_node_node: &str,
    master_instance_id: &str,
    node_stack: Arc<NodeStack>,
) -> Result<Bytes> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let request = NodeStopRequest::decode(&payload.as_bytes())?;

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
        return NodeStopResponse::failure("Cannot stop the master node").encode();
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
        master_node_node,
        master_instance_id,
        node_name.as_str(),
        SHUTDOWN_SERVICE,
        Some(master_node_node),
        Some(&request.instance_id),
        Bytes::from_static(b"shutdown"),
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
/// Uses kill(pid, 0) which doesn't send a signal but checks if the process exists.
fn is_process_running(pid: u32) -> bool {
    // SAFETY: kill(pid, 0) is safe - it doesn't send any signal,
    // just checks if the process exists and we have permission to signal it.
    let result = unsafe { libc::kill(pid as i32, 0) };
    if result == 0 {
        // Process exists and we can signal it
        true
    } else {
        // Check errno to determine why it failed
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        // ESRCH (3) means no such process - it has terminated
        // EPERM (1) means process exists but we don't have permission (still running)
        errno != libc::ESRCH
    }
}
