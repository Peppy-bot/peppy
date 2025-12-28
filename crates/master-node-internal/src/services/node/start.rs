use crate::Result;
use crate::encoding::{NodeStartRequest, NodeStartResponse};
use bytes::Bytes;
use config::node::Name;
use config::runtime::RuntimeConfig;
use node_stack::{NodeEntity, NodeStack};
use peppylib::messaging::ServiceRequestContext;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use tokio::task::JoinHandle;
use tracing::debug;

use crate::services::names;

pub async fn listen_for_node_start(
    messenger: &MessengerHandle,
    master_node_node: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
) -> Result<JoinHandle<Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        master_node_node,
        instance_id,
        node_name,
        names::NODE_START,
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(|context| handle_node_start_request(context, node_stack.clone()))
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_node_start_request(
    context: ServiceRequestContext,
    node_stack: Arc<NodeStack>,
) -> PeppyResult<Bytes> {
    let sender_instance_id = context.message().instance_id();
    handle_node_start_request_inner(&context, node_stack)
        .await
        .map_err(|e| PeppyError::InvalidServiceRequest {
            identifier: sender_instance_id.to_string(),
            reason: e.to_string(),
        })
}

async fn handle_node_start_request_inner(
    context: &ServiceRequestContext,
    node_stack: Arc<NodeStack>,
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
    match run_node(&entity, &request.runtime_config_json5) {
        Ok(_child) => {
            debug!("Successfully started node instance '{}'", instance_id_str);
            NodeStartResponse::success().encode()
        }
        Err(e) => {
            debug!("Failed to start node instance '{}': {}", instance_id_str, e);
            NodeStartResponse::failure(format!("Failed to start node: {}", e)).encode()
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
