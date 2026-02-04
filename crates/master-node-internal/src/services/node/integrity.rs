use crate::{Result, names};
use bytes::Bytes;
use node_stack::NodeStack;
use peppylib::{MessengerHandle, PeppyResult, ServiceMessenger, messaging::ServiceRequestContext};
use std::{sync::Arc, time::Duration};
use tokio::task::JoinHandle;

/// This calls return the sha256 of each exposed interface,
/// allowing subscribers to validate the integrity of the interfaces they subscribe to
pub async fn listen_for_node_integrity(
    messenger: &MessengerHandle,
    master_node_name: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    timeout: Duration,
) -> Result<JoinHandle<Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        master_node_name,
        instance_id,
        node_name,
        names::NODE_INTEGRITY,
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(|context| {
                handle_node_integrity_request(context, Arc::clone(&node_stack), timeout)
            })
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_node_integrity_request(
    context: ServiceRequestContext,
    node_stack: Arc<NodeStack>,
    timeout: Duration,
) -> PeppyResult<Bytes> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let request =
        NodeIntegrityRequest::decode(&payload.as_bytes()).map_err(|e| format!("{}", e))?;

    debug!("Received `node_integrity` request from {sender_instance_id}");
    // TODO: For each exposed topic/service/action, compute the sha256
}
