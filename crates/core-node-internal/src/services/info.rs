use crate::Result;
use crate::names;
use crate::services::response::into_service_response;
use core_node_api::encoding::{ContainerInfo, InfoRequest, InfoResponse};
use node_stack::NodeStack;
use peppylib::messaging::SenderTarget;
use peppylib::messaging::ServiceRequestContext;
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyResult, ServiceMessenger};
use std::sync::Arc;
use std::time::Instant;
use tokio::task::JoinHandle;
use tracing::debug;

pub async fn listen_for_info(
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    start_time: Instant,
) -> Result<JoinHandle<Result<()>>> {
    let messaging_port = messenger.messaging_port().await;

    let mut endpoint = ServiceMessenger::listen(
        messenger,
        core_node_name,
        instance_id,
        SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
        names::INFO,
    )
    .await?;

    let core_node_name = core_node_name.to_owned();
    let instance_id = instance_id.to_owned();

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(move |context| {
                let core_node_name = core_node_name.clone();
                let instance_id = instance_id.clone();
                let node_stack = Arc::clone(&node_stack);
                async move {
                    handle_info_request(
                        context,
                        &core_node_name,
                        &instance_id,
                        start_time,
                        &node_stack,
                        messaging_port,
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
    core_node_name: &str,
    instance_id: &str,
    start_time: Instant,
    node_stack: &NodeStack,
    messaging_port: u16,
) -> PeppyResult<Payload> {
    into_service_response(
        &context,
        handle_info_request_inner(
            &context,
            core_node_name,
            instance_id,
            start_time,
            node_stack,
            messaging_port,
        ),
    )
}

fn handle_info_request_inner(
    context: &ServiceRequestContext,
    core_node_name: &str,
    instance_id: &str,
    start_time: Instant,
    node_stack: &NodeStack,
    messaging_port: u16,
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

    let container_info = ContainerInfo {
        apptainer_version: containers::APPTAINER_VERSION.to_owned(),
        lima_version: containers::LIMA_VERSION.to_owned(),
    };

    InfoResponse::new(
        uptime_secs,
        core_node_name,
        instance_id,
        host_name,
        node_count,
        git_version,
        container_info,
        messaging_port,
    )
    .encode()
    .map_err(Into::into)
}
