use std::sync::Arc;

use bytes::Bytes;
use node_stack::NodeStack;
use tokio::task::JoinHandle;
use tracing::debug;

use crate::messaging::ServiceRequestContext;
use crate::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};

pub const NODE_HEALTH: &str = "node_health";

/// This request is sent by each Node instance every 5sec to notify the master node that they are
/// still alive.
pub async fn listen_for_node_health(
    messenger: &MessengerHandle,
    master_node_node: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
) -> PeppyResult<JoinHandle<PeppyResult<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        master_node_node,
        instance_id,
        node_name,
        NODE_HEALTH,
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(|context| handle_node_health_request(context, node_stack.clone()))
            .await
    });

    Ok(handle)
}

async fn handle_node_health_request(
    context: ServiceRequestContext,
    node_stack: Arc<NodeStack>,
) -> PeppyResult<Bytes> {
    let sender_instance_id = context.message().instance_id();
    handle_node_health_request_inner(&context, node_stack).map_err(|e| {
        PeppyError::InvalidServiceRequest {
            identifier: sender_instance_id.to_string(),
            reason: e.to_string(),
        }
    })
}

fn handle_node_health_request_inner(
    context: &ServiceRequestContext,
    node_stack: Arc<NodeStack>,
) -> PeppyResult<Bytes> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    debug!("Received `node_health` request from {sender_instance_id}");

    let _ = node_stack;

    // TODO: Based on `sender_instance_id`, find the node in the NodeStack and return an error if
    // it can't be found.

    Ok(payload.to_bytes())
}
