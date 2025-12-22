use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use node_stack::NodeStack;
use peppylib::messaging::ServiceRequestContext;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use tokio::task::JoinHandle;
use tracing::debug;

use crate::Result;
use crate::encoding::{InfoRequest, InfoResponse};

use super::names;

pub async fn listen_for_info(
    messenger: &MessengerHandle,
    master_node_name: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    start_time: Instant,
) -> Result<JoinHandle<Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        master_node_name,
        instance_id,
        node_name,
        names::INFO,
    )
    .await?;

    let master_node_name = master_node_name.to_owned();
    let instance_id = instance_id.to_owned();

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(move |context| {
                let master_node_name = master_node_name.clone();
                let instance_id = instance_id.clone();
                let node_stack = Arc::clone(&node_stack);
                async move {
                    handle_info_request(
                        context,
                        &master_node_name,
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
    master_node_name: &str,
    instance_id: &str,
    start_time: Instant,
    node_stack: &NodeStack,
) -> PeppyResult<Bytes> {
    let sender_instance_id = context.message().instance_id();
    handle_info_request_inner(
        &context,
        master_node_name,
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
    master_node_name: &str,
    instance_id: &str,
    start_time: Instant,
    node_stack: &NodeStack,
) -> Result<Bytes> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let _request = InfoRequest::decode(&payload.as_bytes())?;

    debug!("Received `info` request from {sender_instance_id}");

    let uptime_secs = start_time.elapsed().as_secs();
    let host_name = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "unknown".to_string());
    let node_count = node_stack.len() as u32;

    InfoResponse::new(
        uptime_secs,
        master_node_name,
        instance_id,
        host_name,
        node_count,
    )
    .encode()
}
