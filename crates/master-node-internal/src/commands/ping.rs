use bytes::Bytes;
use peppylib::messaging::ServiceRequestContext;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use tokio::task::JoinHandle;
use tracing::debug;

use crate::Result;
use crate::encoding::{PingRequest, PingResponse};

pub async fn listen_for_ping(
    messenger: &MessengerHandle,
    node_name: &str,
    master_node_node: &str,
    instance_id: &str,
) -> Result<JoinHandle<Result<()>>> {
    let ping_service_name = "ping";
    let mut ping_endpoint = ServiceMessenger::listen(
        messenger,
        master_node_node,
        instance_id,
        node_name,
        &ping_service_name,
    )
    .await?;

    let handle = tokio::spawn(async move {
        ping_endpoint
            .handle_requests(handle_ping_request)
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_ping_request(context: ServiceRequestContext) -> PeppyResult<Bytes> {
    let instance_id = context.message().instance_id();
    handle_ping_request_inner(&context).map_err(|e| PeppyError::InvalidServiceRequest {
        identifier: instance_id.to_string(),
        reason: e.to_string(),
    })
}

fn handle_ping_request_inner(context: &ServiceRequestContext) -> Result<Bytes> {
    let instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let request = PingRequest::decode(&payload.as_bytes())?;

    debug!(
        "Received ping request from {instance_id}, timestamp={}",
        request.timestamp
    );

    PingResponse::new(request.timestamp, "pong").encode()
}
