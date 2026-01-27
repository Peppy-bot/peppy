use crate::Result;
use crate::encoding::{LaunchRequest, LaunchResponse};
use crate::names;
use bytes::Bytes;
use node_stack::NodeStack;
use peppylib::MessengerHandle;
use peppylib::messaging::ServiceRequestContext;
use peppylib::{PeppyError, PeppyResult, ServiceMessenger};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::debug;

pub async fn listen_for_stack_launch(
    messenger: &MessengerHandle,
    master_node_node: &str,
    instance_id: &str,
    node_name: &str,
    _node_stack: Arc<NodeStack>,
    _node_startup_timeout: Duration,
    _node_start_health_timeout: Duration,
) -> Result<JoinHandle<Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        master_node_node,
        instance_id,
        node_name,
        names::STACK_LAUNCH,
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(handle_stack_launch_request)
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_stack_launch_request(context: ServiceRequestContext) -> PeppyResult<Bytes> {
    let sender_instance_id = context.message().instance_id();
    handle_stack_launch_request_inner(&context).map_err(|e| PeppyError::InvalidServiceRequest {
        identifier: sender_instance_id.to_string(),
        reason: e.to_string(),
    })
}

fn handle_stack_launch_request_inner(context: &ServiceRequestContext) -> Result<Bytes> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let _request = LaunchRequest::decode(&payload.as_bytes())?;

    debug!("Received `stack_launch` request from {sender_instance_id}");

    LaunchResponse::error("stack_launch is not implemented yet").encode()
}
