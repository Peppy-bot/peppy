use crate::Result;
use crate::names;
use core_node_api::encoding::{StackListRequest, StackListResponse};
use node_stack::NodeStack;
use peppylib::messaging::ServiceRequestContext;
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use std::sync::Arc;
use tokio::task::JoinHandle;
use tracing::debug;

pub async fn listen_for_stack_list(
    messenger: &MessengerHandle,
    core_node_node: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
) -> Result<JoinHandle<Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        core_node_node,
        instance_id,
        node_name,
        names::STACK_LIST,
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(|context| handle_stack_list_request(context, Arc::clone(&node_stack)))
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_stack_list_request(
    context: ServiceRequestContext,
    node_stack: Arc<NodeStack>,
) -> PeppyResult<Payload> {
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
) -> Result<Payload> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let request = StackListRequest::decode(payload.as_ref())?;

    debug!("Received `stack_list` request from {sender_instance_id}");

    let dot_graph = if request.with_dot_graph() {
        Some(node_stack.to_dot())
    } else {
        None
    };
    let serialized_graph = node_stack.to_serialized_graph();
    let graph_json = serde_json::to_string(&serialized_graph).unwrap_or_else(|_| "{}".to_string());
    StackListResponse::new(dot_graph, graph_json)
        .encode()
        .map_err(Into::into)
}
