use bytes::Bytes;
use peppylib::messaging::ServiceRequestContext;
use peppylib::{MessengerHandle, PeppyResult, ServiceMessenger};
use tokio::task::JoinHandle;
use tracing::debug;

use crate::Result;

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

async fn handle_ping_request(_context: ServiceRequestContext) -> PeppyResult<Bytes> {
    debug!("Received ping request, sending pong response");
    Ok(Bytes::from_static(b"pong"))
}
