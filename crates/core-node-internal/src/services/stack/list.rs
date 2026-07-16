use crate::Result;
use crate::services::current_host_name;
use crate::services::response::into_service_response;
use core_node_api::ServiceId;
use core_node_api::encoding::{StackListRequest, StackListResponse};
use core_node_api::names;
use node_stack::NodeStack;
use peppylib::messaging::SenderTarget;
use peppylib::messaging::ServiceRequestContext;
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyResult, ServiceMessenger};
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
        SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
        ServiceId::StackList.name(),
    )
    .await?;

    // The response self-reports the presence identity this endpoint listens
    // as, so aggregating callers can attribute each stack to a live daemon
    // generation without joining on request targeting.
    let core_node = core_node_node.to_string();
    let instance_id = instance_id.to_string();
    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(|context| {
                handle_stack_list_request(
                    context,
                    Arc::clone(&node_stack),
                    core_node.clone(),
                    instance_id.clone(),
                )
            })
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_stack_list_request(
    context: ServiceRequestContext,
    node_stack: Arc<NodeStack>,
    core_node: String,
    instance_id: String,
) -> PeppyResult<Payload> {
    into_service_response(
        &context,
        handle_node_list_request_inner(&context, node_stack, core_node, instance_id),
    )
}

fn handle_node_list_request_inner(
    context: &ServiceRequestContext,
    node_stack: Arc<NodeStack>,
    core_node: String,
    instance_id: String,
) -> Result<Payload> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let _request = StackListRequest::decode(payload.as_ref())?;

    debug!("Received `stack_list` request from {sender_instance_id}");

    // `to_serialized_graph` carries each instance's last health-monitor result,
    // so `stack list` reports health without a separate `node_health` round-trip.
    let serialized_graph = node_stack.to_serialized_graph();
    let graph_json = serde_json::to_string(&serialized_graph).unwrap_or_else(|_| "{}".to_string());
    StackListResponse::new(graph_json, core_node, instance_id, current_host_name())
        .encode()
        .map_err(Into::into)
}
