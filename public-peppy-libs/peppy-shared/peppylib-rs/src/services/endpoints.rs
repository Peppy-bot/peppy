//! `node_endpoints` framework service.
//!
//! A node whose manifest declares `execution.endpoints` exposes this once
//! its setup returned and the announced set was sealed; the daemon reads the
//! set once at start and expands each binding into the URLs it reports.
//! Like `node_health`, this is framework plumbing that never invokes user
//! code.

use tracing::debug;

use crate::encoding::endpoints::{NodeEndpointsRequest, NodeEndpointsResponse};
use crate::messaging::{NODE_ENDPOINTS_SERVICE, SenderTarget, ServiceRequestContext};
use crate::runtime::{AnnouncedEndpoint, TaskHandle};
use crate::types::Payload;
use crate::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};

/// Answers every `node_endpoints` request with `endpoints`, the sealed set
/// of the instance at `(core_node, instance_id)`.
pub async fn listen_for_node_endpoints(
    messenger: &MessengerHandle,
    core_node: &str,
    instance_id: &str,
    as_identity: SenderTarget,
    endpoints: Vec<AnnouncedEndpoint>,
) -> PeppyResult<TaskHandle<PeppyResult<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        core_node,
        instance_id,
        as_identity,
        NODE_ENDPOINTS_SERVICE,
    )
    .await?;
    let response = NodeEndpointsResponse::new(endpoints).encode()?;

    let handle = crate::runtime::spawn(async move {
        endpoint
            .handle_requests(move |context| {
                let response = response.clone();
                async move { handle_endpoints_request(context, response) }
            })
            .await
    });
    Ok(handle)
}

fn handle_endpoints_request(
    context: ServiceRequestContext,
    response: Payload,
) -> PeppyResult<Payload> {
    let sender_instance_id = context.message().instance_id().to_string();
    // The request carries nothing; decoding it still catches wire-schema skew
    // instead of answering a request that was never one.
    NodeEndpointsRequest::decode(context.message().payload_bytes().as_ref()).map_err(|err| {
        PeppyError::InvalidServiceRequest {
            identifier: sender_instance_id.clone(),
            reason: format!("invalid node_endpoints request: {err}"),
        }
    })?;
    debug!("Received `node_endpoints` request from {sender_instance_id}");
    Ok(response)
}
