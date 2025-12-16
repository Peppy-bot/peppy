use std::sync::Arc;

use crate::Result;
use crate::encoding::{
    NodeAddRequest, NodeAddResponse, NodeListRequest, NodeListResponse, NodeSyncRequest,
    NodeSyncResponse,
};
use bytes::Bytes;
use config::node::{Name, NodeConfigParser};
use node_stack::NodeStack;
use peppylib::messaging::ServiceRequestContext;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use tokio::task::JoinHandle;
use tracing::debug;

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

    // Parse the optional instance_id
    let instance_id = match request.instance_id {
        Some(ref id) => match Name::new(id) {
            Ok(name) => Some(name),
            Err(e) => {
                return NodeAddResponse::failure(format!("Invalid instance_id: {}", e)).encode();
            }
        },
        None => None,
    };

    // Add the node to the stack (all dependencies must be satisfied)
    match node_stack.push_config(&node_config, instance_id.as_ref(), false) {
        Ok(instance_id) => {
            debug!(
                "Added node {}:{} with instance_id {}",
                node_config.manifest.name.as_str(),
                node_config.manifest.tag,
                instance_id.as_str()
            );
            NodeAddResponse::success(instance_id.as_str()).encode()
        }
        Err(e) => NodeAddResponse::failure(format!("Failed to add node: {}", e)).encode(),
    }
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

    let request = NodeSyncRequest::decode(&payload.as_bytes())?;

    debug!("Received `node_sync` request from {sender_instance_id}");

    if request.node_root_dir.as_os_str().is_empty() {
        return NodeSyncResponse::failure("Missing `node_root_dir` in node_sync request").encode();
    }

    if !request.node_root_dir.exists() {
        return NodeSyncResponse::failure(format!(
            "`node_root_dir` does not exist: {}",
            request.node_root_dir.display()
        ))
        .encode();
    }

    if !request.node_root_dir.is_dir() {
        return NodeSyncResponse::failure(format!(
            "`node_root_dir` is not a directory: {}",
            request.node_root_dir.display()
        ))
        .encode();
    }

    if let Err(e) =
        generator::generate_lib_for_build_system(request.build_system, &request.node_root_dir)
    {
        return NodeSyncResponse::failure(format!("Failed to generate peppygen: {}", e)).encode();
    }

    NodeSyncResponse::success().encode()
}
