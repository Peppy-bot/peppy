use bytes::Bytes;
use peppylib::messaging::ServiceRequestContext;
use peppylib::{MessengerHandle, PeppyResult, ServiceMessenger};
use tokio::task::JoinHandle;
use tracing::debug;

use crate::Result;

pub async fn listen_for_add_node(
    messenger: &MessengerHandle,
    node_name: &str,
    master_node_node: &str,
    instance_id: &str,
) -> Result<JoinHandle<Result<()>>> {
    let ping_service_name = "add_node";
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
            .handle_requests(handle_listen_for_add_node)
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_listen_for_add_node(_context: ServiceRequestContext) -> PeppyResult<Bytes> {
    debug!("Received listen_for_add_node request");
    todo!("Finish")
}
