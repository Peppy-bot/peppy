use bytes::Bytes;
use tokio::task::JoinHandle;
use tracing::debug;

use crate::messaging::ServiceRequestContext;
use crate::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};

/// This service is exposed by each Node instance to signal to the daemon node that
/// the node's runner::run() has started (i.e., the node is ready for health checks).
pub async fn listen_for_node_ready(
    messenger: &MessengerHandle,
    daemon_node_node: &str,
    instance_id: &str,
    node_name: &str,
) -> PeppyResult<JoinHandle<PeppyResult<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        daemon_node_node,
        instance_id,
        node_name,
        super::super::messaging::NODE_READY_SERVICE,
    )
    .await?;

    let handle =
        tokio::spawn(async move { endpoint.handle_requests(handle_node_ready_request).await });

    Ok(handle)
}

async fn handle_node_ready_request(context: ServiceRequestContext) -> PeppyResult<Bytes> {
    let sender_instance_id = context.message().instance_id();
    handle_node_ready_request_inner(&context).map_err(|e| PeppyError::InvalidServiceRequest {
        identifier: sender_instance_id.to_string(),
        reason: e.to_string(),
    })
}

fn handle_node_ready_request_inner(context: &ServiceRequestContext) -> PeppyResult<Bytes> {
    let sender_instance_id = context.message().instance_id();

    debug!("Received `node_ready` request from {sender_instance_id}");

    // Echo the payload back to confirm readiness
    Ok(context.message().payload().to_bytes())
}
