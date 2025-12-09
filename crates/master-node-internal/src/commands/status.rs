use bytes::Bytes;
use peppy_core::AppContext;
use peppylib::messaging::ServiceRequestContext;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use tokio::task::JoinHandle;
use tracing::debug;

use crate::Result;

pub async fn listen_for_status(
    messenger: &MessengerHandle,
    master_node_name: &str,
    instance_id: &str,
    node_name: &str,
    _app_context: &AppContext,
) -> Result<JoinHandle<Result<()>>> {
    let status_service_name = "status";
    let mut status_endpoint = ServiceMessenger::listen(
        messenger,
        master_node_name,
        instance_id,
        node_name,
        &status_service_name,
    )
    .await?;

    let handle = tokio::spawn(async move {
        status_endpoint
            .handle_requests(handle_status_request)
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_status_request(context: ServiceRequestContext) -> PeppyResult<Bytes> {
    let instance_id = context.message().instance_id();
    handle_status_request_inner(&context).map_err(|e| PeppyError::InvalidServiceRequest {
        identifier: instance_id.to_string(),
        reason: e.to_string(),
    })
}

fn handle_status_request_inner(context: &ServiceRequestContext) -> Result<Bytes> {
    let _payload = context.message().payload();

    debug!("Received status request, sending ok response");
    Ok(Bytes::from_static(b"ok"))
}
