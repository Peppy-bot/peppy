use crate::Result;
use crate::encoding::{NodeStartRequest, NodeStartResponse};
use bytes::Bytes;
use config::node::Name;
use config::runtime::RuntimeConfig;
use node_stack::{NodeEntity, NodeStack};
use peppylib::encoding::health::NodeHealthRequest;
use peppylib::messaging::{NODE_HEALTH_SERVICE, ServiceRequestContext};
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::debug;

use crate::services::names;

pub async fn listen_for_node_start(
    messenger: &MessengerHandle,
    master_node_node: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    node_start_health_timeout: Duration,
) -> Result<JoinHandle<Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        master_node_node,
        instance_id,
        node_name,
        names::NODE_START,
    )
    .await?;

    let messenger = messenger.clone();
    let master_node_name = master_node_node.to_string();
    let caller_instance_id = instance_id.to_string();

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(|context| {
                handle_node_start_request(
                    context,
                    node_stack.clone(),
                    messenger.clone(),
                    master_node_name.clone(),
                    caller_instance_id.clone(),
                    node_start_health_timeout,
                )
            })
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_node_start_request(
    context: ServiceRequestContext,
    node_stack: Arc<NodeStack>,
    messenger: MessengerHandle,
    master_node_name: String,
    caller_instance_id: String,
    node_start_health_timeout: Duration,
) -> PeppyResult<Bytes> {
    let sender_instance_id = context.message().instance_id();
    handle_node_start_request_inner(
        &context,
        node_stack,
        &messenger,
        &master_node_name,
        &caller_instance_id,
        node_start_health_timeout,
    )
    .await
    .map_err(|e| PeppyError::InvalidServiceRequest {
        identifier: sender_instance_id.to_string(),
        reason: e.to_string(),
    })
}

async fn handle_node_start_request_inner(
    context: &ServiceRequestContext,
    node_stack: Arc<NodeStack>,
    messenger: &MessengerHandle,
    master_node_name: &str,
    caller_instance_id: &str,
    node_start_health_timeout: Duration,
) -> Result<Bytes> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let request = NodeStartRequest::decode(&payload.as_bytes())?;

    // Parse the PEPPY_RUNTIME_CONFIG from json5
    let runtime_config: RuntimeConfig = match serde_json5::from_str(&request.runtime_config_json5) {
        Ok(config) => config,
        Err(e) => {
            return NodeStartResponse::failure(format!(
                "Failed to parse PEPPY_RUNTIME_CONFIG: {}",
                e
            ))
            .encode();
        }
    };

    // Convert instance_id from config::peppy_config::Name to config::node::Name
    let instance_id_str = runtime_config.deployment_instance.instance_id.as_str();
    let instance_id = match Name::new(instance_id_str) {
        Ok(name) => name,
        Err(e) => {
            return NodeStartResponse::failure(format!("Invalid instance_id: {}", e)).encode();
        }
    };

    debug!(
        "Received `node_start` request from {sender_instance_id}, instance_id={}",
        instance_id_str
    );

    // Find the entity in the node stack
    let entity = match node_stack.find_entity_by_instance_id(&instance_id) {
        Some(entity) => entity,
        None => {
            return NodeStartResponse::failure(format!(
                "Node instance '{}' not found in node stack",
                instance_id_str
            ))
            .encode();
        }
    };

    // Run the node with the runtime config
    let mut child = match run_node(&entity, &request.runtime_config_json5) {
        Ok(child) => child,
        Err(e) => {
            debug!("Failed to start node instance '{}': {}", instance_id_str, e);
            return NodeStartResponse::failure(format!("Failed to start node: {}", e)).encode();
        }
    };

    debug!(
        "Successfully spawned node instance '{}', performing health check...",
        instance_id_str
    );

    // Perform health check with timeout
    let health_result = perform_health_check(
        messenger,
        master_node_name,
        caller_instance_id,
        runtime_config.node_name.as_str(),
        runtime_config.bound_master_node.as_str(),
        instance_id_str,
        node_start_health_timeout,
    )
    .await;

    match health_result {
        Ok(()) => {
            debug!(
                "Health check passed for node instance '{}'",
                instance_id_str
            );
            NodeStartResponse::success().encode()
        }
        Err(e) => {
            debug!(
                "Health check failed for node instance '{}': {}, killing process",
                instance_id_str, e
            );
            // Kill the process since health check failed
            if let Err(kill_err) = child.kill() {
                debug!(
                    "Failed to kill process for node instance '{}': {}",
                    instance_id_str, kill_err
                );
            }
            NodeStartResponse::failure(format!("Health check failed: {}", e)).encode()
        }
    }
}

/// Runs a node using its manifest's launch_cmd and passes the PEPPY_RUNTIME_CONFIG as an env var.
/// Returns the spawned child process handle on success.
pub fn run_node(entity: &NodeEntity, runtime_config_json5: &str) -> Result<Child> {
    let manifest = &entity.config().manifest;
    let launch_cmd = manifest.launch_cmd.as_ref().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "No launch_cmd configured for node '{}:{}'",
                manifest.name.as_str(),
                manifest.tag
            ),
        )
    })?;

    if launch_cmd.is_empty() {
        return Err(
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "launch_cmd is empty").into(),
        );
    }

    let program = &launch_cmd[0];
    let args = &launch_cmd[1..];

    debug!(
        "Running node '{}:{}' with command: {} {:?}",
        manifest.name.as_str(),
        manifest.tag,
        program,
        args
    );

    let child = Command::new(program)
        .args(args)
        .env("PEPPY_RUNTIME_CONFIG", runtime_config_json5)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    Ok(child)
}

/// Performs a health check on a newly started node instance.
/// Polls the node's health service with a timeout and returns Ok if the node responds.
async fn perform_health_check(
    messenger: &MessengerHandle,
    master_node_name: &str,
    caller_instance_id: &str,
    target_node_name: &str,
    target_master_node: &str,
    target_instance_id: &str,
    timeout: Duration,
) -> Result<()> {
    let request = NodeHealthRequest::new();
    let request_payload = request.encode()?;

    ServiceMessenger::poll(
        messenger,
        master_node_name,
        caller_instance_id,
        target_node_name,
        NODE_HEALTH_SERVICE,
        Some(target_master_node),
        Some(target_instance_id),
        request_payload,
        timeout,
    )
    .await?;

    Ok(())
}
