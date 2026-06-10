use super::MessengerHandle;
use crate::error::Result;
use crate::types::Payload;
use pmi::{ServiceQueryKind, ServiceWireSender};
use tokio::time::Duration;

/// Resolves a wildcard service or action target to a single concrete producer
/// `(core_node, instance_id)` before the real request is dispatched.
///
/// Sends a probe (empty payload, `ServiceQueryKind::Probe` on the
/// attachment) to `probe_sender`; producer-side request loops auto-respond
/// to probes before the user handler runs (see
/// [`crate::messaging::services::ServiceEndpoint`]), so the discovery is
/// side-effect-free even when multiple producers match the wildcard.
///
/// The first responder wins. Subsequent producer replies are ignored — they
/// drop on the floor when `poll_service` returns. The Zenoh keyexpr embedded
/// in the reply yields the responder's identity via the existing
/// `TopicMessage` parser.
///
/// **Why this exists**: `QueryTarget::All` delivers a wildcard query to every
/// matching producer; without discovery, every producer would also execute
/// the user handler. For services that's wasted work; for actions it is a
/// real-world safety hazard (e.g. two manipulators both executing the same
/// goal). Discovery pins the consumer to one producer so only that
/// producer's handler runs.
pub(super) async fn discover_producer(
    messenger: &MessengerHandle,
    probe_sender: &ServiceWireSender,
    discovery_timeout: Duration,
) -> Result<(String, String)> {
    // `poll_service` itself retries on a peer-mode cold-start miss (the probe's
    // `QueryTarget::All` finalizing with no reply before discovery has settled),
    // bounded by `discovery_timeout`, so a single probe call here waits for a
    // producer to appear rather than failing the instant it runs ahead of
    // discovery.
    let response = messenger
        .poll_service(
            probe_sender,
            Payload::new(),
            ServiceQueryKind::Probe,
            discovery_timeout,
        )
        .await?;
    Ok((
        response.core_node().to_string(),
        response.instance_id().to_string(),
    ))
}
