use crate::Result;
use crate::names;
use crate::services::node::teardown_all_instances;
use crate::services::response::into_service_response;
use core_node_api::encoding::{NodeResetRequest, NodeResetResponse};
use node_stack::NodeStack;
use peppylib::messaging::SenderTarget;
use peppylib::messaging::ServiceRequestContext;
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyResult, ServiceMessenger};
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
        SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
        names::STACK_RESET,
    )
    .await?;

    // Owned copies moved into the spawned task so the reset handler can stop the
    // running instances before clearing them. The reset request itself carries
    // no routing identity, so the daemon's own (messenger, core node, instance)
    // are what the teardown sends cooperative shutdowns from.
    let messenger = messenger.clone();
    let core_node_node = core_node_node.to_owned();
    let instance_id = instance_id.to_owned();

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(|context| {
                handle_stack_reset_request(
                    context,
                    messenger.clone(),
                    core_node_node.clone(),
                    instance_id.clone(),
                    Arc::clone(&node_stack),
                )
            })
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_stack_reset_request(
    context: ServiceRequestContext,
    messenger: MessengerHandle,
    core_node_node: String,
    instance_id: String,
    node_stack: Arc<NodeStack>,
) -> PeppyResult<Payload> {
    into_service_response(
        &context,
        handle_node_reset_request_inner(
            &context,
            &messenger,
            &core_node_node,
            &instance_id,
            node_stack,
        )
        .await,
    )
}

async fn handle_node_reset_request_inner(
    context: &ServiceRequestContext,
    messenger: &MessengerHandle,
    core_node_node: &str,
    instance_id: &str,
    node_stack: Arc<NodeStack>,
) -> Result<Payload> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let _request = NodeResetRequest::decode(payload.as_ref())?;

    debug!("Received `node_reset` request from {sender_instance_id}");
    // Stop the running instances (cooperative shutdown, then force-kill the
    // process group of any straggler) before dropping them from the stack, so a
    // reset never orphans the previous stack's processes.
    teardown_all_instances(messenger, core_node_node, instance_id, &node_stack).await;
    node_stack.reset();
    NodeResetResponse::success().encode().map_err(Into::into)
}
