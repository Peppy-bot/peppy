use crate::runtime::TaskHandle;
use crate::types::Payload;
use tracing::debug;

use crate::messaging::ServiceRequestContext;
use crate::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
pub async fn listen_for_node_health(
    messenger: &MessengerHandle,
    daemon_node_node: &str,
    instance_id: &str,
    node_name: &str,
) -> PeppyResult<TaskHandle<PeppyResult<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        daemon_node_node,
        instance_id,
        node_name,
        super::super::messaging::NODE_HEALTH_SERVICE,
    )
    .await?;

    let handle =
        crate::runtime::spawn(
            async move { endpoint.handle_requests(handle_node_health_request).await },
        );
    Ok(handle)
}

async fn handle_node_health_request(context: ServiceRequestContext) -> PeppyResult<Payload> {
    let sender_instance_id = context.message().instance_id();
    handle_node_health_request_inner(&context).map_err(|e| PeppyError::InvalidServiceRequest {
        identifier: sender_instance_id.to_string(),
        reason: e.to_string(),
    })
}

fn handle_node_health_request_inner(context: &ServiceRequestContext) -> PeppyResult<Payload> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    debug!("Received `node_health` request from {sender_instance_id}");

    // TODO: Based on `sender_instance_id`, find the node in the NodeStack and return an error if
    // it can't be found.

    Ok(Payload::from(payload.to_bytes()))
}
