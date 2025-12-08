use bytes::Bytes;
use peppylib::messaging::ServiceRequestContext;
use peppylib::{MessengerHandle, PeppyResult, ServiceMessenger};
use tokio::task::JoinHandle;
use tracing::debug;

use crate::Result;

pub async fn listen_for_status(
    messenger: &MessengerHandle,
    node_name: &str,
    master_node_node: &str,
    instance_id: &str,
) -> Result<JoinHandle<Result<()>>> {
    let status_service_name = "status";
    let mut status_endpoint = ServiceMessenger::listen(
        messenger,
        master_node_node,
        instance_id,
        node_name,
        &status_service_name,
    )
    .await?;

    let handle = tokio::spawn(async move {
        status_endpoint
            .handle_requests(handle_status_request)
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_status_request(_context: ServiceRequestContext) -> PeppyResult<Bytes> {
    debug!("Received status request, sending ok response");
    Ok(Bytes::from_static(b"ok"))
}
