use crate::Result;
use crate::names;
use crate::services::response::into_service_response;
use core_node_api::encoding::{PingRequest, PingResponse};
use peppylib::messaging::SenderTarget;
use peppylib::messaging::ServiceRequestContext;
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyResult, ServiceMessenger};
use tokio::task::JoinHandle;
use tracing::debug;

pub async fn listen_for_ping(
    messenger: &MessengerHandle,
    core_node_node: &str,
    instance_id: &str,
    node_name: &str,
) -> Result<JoinHandle<Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        core_node_node,
        instance_id,
        SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
        names::PING,
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(handle_ping_request)
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_ping_request(context: ServiceRequestContext) -> PeppyResult<Payload> {
    into_service_response(&context, handle_ping_request_inner(&context))
}

fn handle_ping_request_inner(context: &ServiceRequestContext) -> Result<Payload> {
    let instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let request = PingRequest::decode(payload.as_ref())?;

    debug!(
        "Received ping request from {instance_id}, timestamp={}",
        request.timestamp
    );

    PingResponse::new(request.timestamp, "pong")
        .encode()
        .map_err(Into::into)
}
