use crate::Result;
use crate::names;
use core_node_api::encoding::{NodeResetRequest, NodeResetResponse};
use node_stack::NodeStack;
use peppylib::messaging::Iface;
use peppylib::messaging::ServiceRequestContext;
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use std::sync::Arc;
use tokio::task::JoinHandle;
use tracing::debug;

pub async fn listen_for_stack_reset(
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
        Iface::native(),
        names::STACK_RESET,
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(|context| handle_stack_reset_request(context, Arc::clone(&node_stack)))
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_stack_reset_request(
    context: ServiceRequestContext,
    node_stack: Arc<NodeStack>,
) -> PeppyResult<Payload> {
    let sender_instance_id = context.message().instance_id();
    handle_node_reset_request_inner(&context, node_stack)
        .await
        .map_err(|e| PeppyError::InvalidServiceRequest {
            identifier: sender_instance_id.to_string(),
            reason: e.to_string(),
        })
}

async fn handle_node_reset_request_inner(
    context: &ServiceRequestContext,
    node_stack: Arc<NodeStack>,
) -> Result<Payload> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let _request = NodeResetRequest::decode(payload.as_ref())?;

    debug!("Received `node_reset` request from {sender_instance_id}");
    node_stack.reset();
    NodeResetResponse::success().encode().map_err(Into::into)
}
