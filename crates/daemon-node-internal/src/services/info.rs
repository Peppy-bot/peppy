use crate::Result;
use crate::encoding::{InfoRequest, InfoResponse};
use crate::names;
use node_stack::NodeStack;
use peppylib::messaging::ServiceRequestContext;
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use std::sync::Arc;
use std::time::Instant;
use tokio::task::JoinHandle;
use tracing::debug;

pub async fn listen_for_info(
    messenger: &MessengerHandle,
    daemon_node_name: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    start_time: Instant,
) -> Result<JoinHandle<Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        daemon_node_name,
        instance_id,
        node_name,
        names::INFO,
    )
    .await?;

    let daemon_node_name = daemon_node_name.to_owned();
    let instance_id = instance_id.to_owned();

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(move |context| {
                let daemon_node_name = daemon_node_name.clone();
                let instance_id = instance_id.clone();
                let node_stack = Arc::clone(&node_stack);
                async move {
                    handle_info_request(
                        context,
                        &daemon_node_name,
                        &instance_id,
                        start_time,
                        &node_stack,
                    )
                    .await
                }
            })
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_info_request(
    context: ServiceRequestContext,
    daemon_node_name: &str,
    instance_id: &str,
    start_time: Instant,
    node_stack: &NodeStack,
) -> PeppyResult<Payload> {
    let sender_instance_id = context.message().instance_id();
    handle_info_request_inner(
        &context,
        daemon_node_name,
        instance_id,
        start_time,
        node_stack,
    )
    .map_err(|e| PeppyError::InvalidServiceRequest {
        identifier: sender_instance_id.to_string(),
        reason: e.to_string(),
    })
}

fn handle_info_request_inner(
    context: &ServiceRequestContext,
    daemon_node_name: &str,
    instance_id: &str,
    start_time: Instant,
    node_stack: &NodeStack,
) -> Result<Payload> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let _request = InfoRequest::decode(payload.as_ref())?;

    debug!("Received `info` request from {sender_instance_id}");

    let uptime_secs = start_time.elapsed().as_secs();
    let host_name = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "unknown".to_string());
    let node_count = node_stack.len() as u32;
    let git_version = option_env!("PEPPY_GIT_TAG").unwrap_or("unknown");

    InfoResponse::new(
        uptime_secs,
        daemon_node_name,
        instance_id,
        host_name,
        node_count,
        git_version,
    )
    .encode()
}
