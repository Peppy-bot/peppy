use crate::Result;
use crate::names;
use crate::services::response::into_service_response;
use core_node_api::encoding::{HealthRequest, HealthResponse};
use peppylib::messaging::SenderTarget;
use peppylib::messaging::ServiceRequestContext;
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyResult, ServiceMessenger};
use std::time::Instant;
use tokio::task::JoinHandle;
use tracing::debug;

/// Liveness service polled by an external prober (the platform backend, over the
/// federated zenoh link). A well-formed reply is the liveness signal; the
/// returned uptime is informational. Registered under the same identity as every
/// other core-node service so callers address it as `(core_node_name, core)`.
pub async fn listen_for_health(
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_name: &str,
    start_time: Instant,
) -> Result<JoinHandle<Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        core_node_name,
        instance_id,
        SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
        names::HEALTH,
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(move |context| async move {
                handle_health_request(context, start_time).await
            })
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_health_request(
    context: ServiceRequestContext,
    start_time: Instant,
) -> PeppyResult<Payload> {
    into_service_response(&context, handle_health_request_inner(&context, start_time))
}

fn handle_health_request_inner(
    context: &ServiceRequestContext,
    start_time: Instant,
) -> Result<Payload> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    HealthRequest::decode(payload.as_ref())?;

    debug!("Received `health` request from {sender_instance_id}");

    let uptime_secs = start_time.elapsed().as_secs();

    HealthResponse::new("healthy", uptime_secs)
        .encode()
        .map_err(Into::into)
}
