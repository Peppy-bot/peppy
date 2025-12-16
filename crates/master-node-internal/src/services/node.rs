use std::sync::Arc;

use bytes::Bytes;
use node_stack::NodeStack;
use peppylib::messaging::ServiceRequestContext;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use tokio::task::JoinHandle;
use tracing::debug;

use crate::Result;
use crate::encoding::{
    NodeAddRequest, NodeAddResponse, NodeListRequest, NodeListResponse, NodeSyncRequest,
    NodeSyncResponse,
};

// ============================================================================
// Node List Service
// ============================================================================

pub async fn listen_for_node_list(
    messenger: &MessengerHandle,
    master_node_node: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
) -> Result<JoinHandle<Result<()>>> {
    let service_name = "node_list";
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        master_node_node,
        instance_id,
        node_name,
        &service_name,
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(|context| handle_node_list_request(context, node_stack.clone()))
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_node_list_request(
    context: ServiceRequestContext,
    node_stack: Arc<NodeStack>,
) -> PeppyResult<Bytes> {
    let sender_instance_id = context.message().instance_id();
    handle_node_list_request_inner(&context, node_stack).map_err(|e| {
        PeppyError::InvalidServiceRequest {
            identifier: sender_instance_id.to_string(),
            reason: e.to_string(),
        }
    })
}

fn handle_node_list_request_inner(
    context: &ServiceRequestContext,
    node_stack: Arc<NodeStack>,
) -> Result<Bytes> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let _request = NodeListRequest::decode(&payload.as_bytes())?;

    debug!("Received `node_list` request from {sender_instance_id}");

    let dot_graph = node_stack.to_dot();
    NodeListResponse::new(dot_graph).encode()
}

// ============================================================================
// Node Add Service
// ============================================================================

pub async fn listen_for_node_add(
    messenger: &MessengerHandle,
    master_node_node: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
) -> Result<JoinHandle<Result<()>>> {
    let service_name = "node_add";
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        master_node_node,
        instance_id,
        node_name,
        &service_name,
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
    handle_node_add_request_inner(&context, node_stack).map_err(|e| {
        PeppyError::InvalidServiceRequest {
            identifier: sender_instance_id.to_string(),
            reason: e.to_string(),
        }
    })
}

fn handle_node_add_request_inner(
    context: &ServiceRequestContext,
    _node_stack: Arc<NodeStack>,
) -> Result<Bytes> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let request = NodeAddRequest::decode(&payload.as_bytes())?;

    debug!(
        "Received `node_add` request from {sender_instance_id}, from_dir={}",
        request.from_dir.display()
    );

    // TODO: Implement actual node addition logic
    NodeAddResponse::success("").encode()
}

// ============================================================================
// Node Sync Service
// ============================================================================

pub async fn listen_for_node_sync(
    messenger: &MessengerHandle,
    master_node_node: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
) -> Result<JoinHandle<Result<()>>> {
    let service_name = "node_sync";
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        master_node_node,
        instance_id,
        node_name,
        &service_name,
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(|context| handle_node_sync_request(context, node_stack.clone()))
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_node_sync_request(
    context: ServiceRequestContext,
    node_stack: Arc<NodeStack>,
) -> PeppyResult<Bytes> {
    let sender_instance_id = context.message().instance_id();
    handle_node_sync_request_inner(&context, node_stack).map_err(|e| {
        PeppyError::InvalidServiceRequest {
            identifier: sender_instance_id.to_string(),
            reason: e.to_string(),
        }
    })
}

fn handle_node_sync_request_inner(
    context: &ServiceRequestContext,
    _node_stack: Arc<NodeStack>,
) -> Result<Bytes> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let _request = NodeSyncRequest::decode(&payload.as_bytes())?;

    debug!("Received `node_sync` request from {sender_instance_id}");

    // TODO: Implement actual node synchronization logic
    NodeSyncResponse::success().encode()
}
