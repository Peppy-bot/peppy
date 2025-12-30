use crate::Result;
use crate::encoding::{NodeAddRequest, NodeAddResponse};
use bytes::Bytes;
use config::node::NodeConfigParser;
use node_stack::NodeStack;
use peppylib::messaging::ServiceRequestContext;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use std::sync::Arc;
use tokio::task::JoinHandle;
use tracing::debug;

use crate::services::names;

pub async fn listen_for_node_add(
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
        names::NODE_ADD,
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(|context| handle_node_add_request(context, node_stack.clone()))
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_node_add_request(
    context: ServiceRequestContext,
    node_stack: Arc<NodeStack>,
) -> PeppyResult<Bytes> {
    let sender_instance_id = context.message().instance_id();
    handle_node_add_request_inner(&context, node_stack)
        .await
        .map_err(|e| PeppyError::InvalidServiceRequest {
            identifier: sender_instance_id.to_string(),
            reason: e.to_string(),
        })
}

async fn handle_node_add_request_inner(
    context: &ServiceRequestContext,
    node_stack: Arc<NodeStack>,
) -> Result<Bytes> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let request = NodeAddRequest::decode(&payload.as_bytes())?;

    debug!(
        "Received `node_add` request from {sender_instance_id}, from_dir={}",
        request.from_dir.display()
    );

    // Parse the node configuration from JSON5
    let node_config = match NodeConfigParser::from_content(&request.peppy_json5) {
        Ok(config) => config,
        Err(e) => {
            return NodeAddResponse::failure(format!("Failed to parse node config: {}", e))
                .encode();
        }
    };

    // Add the node config to the stack (all dependencies must be satisfied)
    // Note: `add` only registers the node configuration, it does not spawn any instance.
    // Use `node_start` to spawn instances after adding a node.
    if let Err(e) = node_stack.push_config(&node_config, false) {
        return NodeAddResponse::failure(format!("Failed to add node config: {}", e)).encode();
    }

    debug!(
        "Added node {}:{}",
        node_config.manifest.name.as_str(),
        node_config.manifest.tag,
    );

    NodeAddResponse::success().encode()
}
