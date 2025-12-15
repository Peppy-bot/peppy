use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use node_stack::NodeStack;
use peppylib::messaging::ServiceRequestContext;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use tokio::task::JoinHandle;
use tracing::debug;

use crate::Result;
use crate::encoding::{InfoRequest, InfoResponse, InfoType};

pub async fn listen_for_info(
    messenger: &MessengerHandle,
    master_node_name: &str,
    instance_id: &str,
    node_name: &str,
    _node_stack: Arc<NodeStack>,
) -> Result<JoinHandle<Result<()>>> {
    let service_name = "info";
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        master_node_name,
        instance_id,
        node_name,
        &service_name,
    )
    .await?;

    let master_node_name = master_node_name.to_owned();
    let instance_id = instance_id.to_owned();
    let start_time = Instant::now();

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(move |context| {
                let master_node_name = master_node_name.clone();
                let instance_id = instance_id.clone();
                async move {
                    handle_info_request(context, &master_node_name, &instance_id, start_time).await
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
) -> PeppyResult<Bytes> {
    let sender_instance_id = context.message().instance_id();
    handle_info_request_inner(&context, master_node_name, instance_id, start_time).map_err(|e| {
        PeppyError::InvalidServiceRequest {
            identifier: sender_instance_id.to_string(),
            reason: e.to_string(),
        }
    })
}

fn handle_info_request_inner(
    context: &ServiceRequestContext,
    master_node_name: &str,
    instance_id: &str,
    start_time: Instant,
) -> Result<Bytes> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let request = InfoRequest::decode(&payload.as_bytes())?;

    debug!(
        "Received `info` request from {sender_instance_id}, info_type={:?}",
        request.info_type
    );

    let value = match request.info_type {
        InfoType::Uptime => {
            let uptime_secs = start_time.elapsed().as_secs();
            uptime_secs.to_string()
        }
        InfoType::MasterNodeName => master_node_name.to_string(),
        InfoType::MasterNodeInstanceId => instance_id.to_string(),
        InfoType::HostName => hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_else(|| "unknown".to_string()),
    };

    InfoResponse::new(request.info_type, value).encode()
}
