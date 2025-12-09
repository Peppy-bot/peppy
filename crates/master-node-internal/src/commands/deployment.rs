use bytes::Bytes;
use peppylib::messaging::ServiceRequestContext;
use peppylib::{MessengerHandle, PeppyResult, ServiceMessenger};
use tokio::task::JoinHandle;
use tracing::debug;

use crate::Result;

pub async fn listen_for_launch_deployment(
    messenger: &MessengerHandle,
    master_node_node: &str,
    instance_id: &str,
    node_name: &str,
) -> Result<JoinHandle<Result<()>>> {
    let ping_service_name = "launch_deployment";
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        master_node_node,
        instance_id,
        node_name,
        &ping_service_name,
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(handle_launch_deployment_request)
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_launch_deployment_request(_context: ServiceRequestContext) -> PeppyResult<Bytes> {
    debug!("Received handle_launch_deployment_request request");
    Ok(Bytes::from_static(b"pong"))
}
