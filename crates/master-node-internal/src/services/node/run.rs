use crate::Result;
use crate::encoding::{NodeRunRequest, NodeRunResponse};
use bytes::Bytes;
use config::node::Name;
use node_stack::{NodeInstance, NodeStack};
use peppylib::messaging::ServiceRequestContext;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use std::sync::Arc;
use tokio::task::JoinHandle;
use tracing::debug;

use crate::services::names;

pub async fn listen_for_node_run(
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
        names::NODE_RUN,
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(|context| handle_node_run_request(context, node_stack.clone()))
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_node_run_request(
    context: ServiceRequestContext,
    node_stack: Arc<NodeStack>,
) -> PeppyResult<Bytes> {
    let sender_instance_id = context.message().instance_id();
    handle_node_run_request_inner(&context, node_stack)
        .await
        .map_err(|e| PeppyError::InvalidServiceRequest {
            identifier: sender_instance_id.to_string(),
            reason: e.to_string(),
        })
}

async fn handle_node_run_request_inner(
    context: &ServiceRequestContext,
    node_stack: Arc<NodeStack>,
) -> Result<Bytes> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let request = NodeRunRequest::decode(&payload.as_bytes())?;

    debug!(
        "Received `node_run` request from {sender_instance_id}, instance_id={}",
        request.instance_id
    );

    // Parse the instance_id
    let instance_id = match Name::new(&request.instance_id) {
        Ok(name) => name,
        Err(e) => {
            return NodeRunResponse::failure(format!("Invalid instance_id: {}", e)).encode();
        }
    };

    // Find the node in the node_stack based on its instance_id
    let Some(node_instance) = node_stack.find_by_instance_id(&instance_id) else {
        return NodeRunResponse::failure(format!(
            "Node instance '{}' not found in node stack",
            instance_id.as_str()
        ))
        .encode();
    };

    // Run the node
    match run_node(&node_instance).await {
        Ok(_) => NodeRunResponse::success().encode(),
        Err(e) => NodeRunResponse::failure(format!("Failed to run node: {}", e)).encode(),
    }
}

pub async fn run_node(node_instance: &NodeInstance) -> Result<bool> {
    // TODO: Implement actual node run logic
    let _ = node_instance;
    Ok(true)
}
