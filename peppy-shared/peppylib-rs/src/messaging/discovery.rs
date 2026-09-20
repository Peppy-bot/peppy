use super::{MessengerHandle, PROBE_TIMEOUT};
use crate::error::{Error, Result};
use crate::messaging::ProducerRef;
use crate::types::Payload;
use pmi::{ServiceQueryKind, ServiceWireSender};
use tokio::time::{Duration, Instant};

/// Resolves a wildcard service or action target to a single concrete
/// producer [`ProducerRef`] before the real request is dispatched. Only
/// genuine wildcards (core-node infra calls scoped wider than one
/// producer) reach this path — pinned targets carry their full
/// `(core_node, instance_id)` and never discover; generated dep-slot call
/// sites always pin.
///
/// Sends a probe (empty payload, `ServiceQueryKind::Probe` on the
/// attachment) to `probe_sender`; the producer-side transport adapter
/// auto-responds to probes in its query dispatch — even while user code
/// holds the producer's request loop — so the discovery is side-effect-free
/// and starvation-free even when multiple producers match the wildcard.
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
) -> Result<ProducerRef> {
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
    Ok(ProducerRef::new(
        response.core_node(),
        response.instance_id(),
    ))
}

/// Probe `sender` once and classify reachability. The probe is auto-answered by
/// the producer-side transport, so the user handler never runs. A successful
/// reply or a [`Error::ServiceTimeout`] (producer reached but slow) both count
/// as reachable; only [`Error::ServiceUnreachable`] is `false`. Shared by
/// [`ServiceMessenger::is_reachable`](super::ServiceMessenger::is_reachable) and
/// [`ActionMessenger::is_reachable`](super::ActionMessenger::is_reachable),
/// which probes the action's goal service.
pub(super) async fn probe_reachable(
    messenger: &MessengerHandle,
    sender: &ServiceWireSender,
) -> Result<bool> {
    match messenger
        .poll_service(
            sender,
            Payload::new(),
            ServiceQueryKind::Probe,
            PROBE_TIMEOUT,
        )
        .await
    {
        Ok(_) => Ok(true),
        Err(Error::ServiceUnreachable { .. }) => Ok(false),
        Err(Error::ServiceTimeout { .. }) => Ok(true),
        Err(e) => Err(e),
    }
}

/// Measure the round-trip latency of a single sized `Probe` to `sender`. Like
/// [`probe_reachable`], the probe is auto-answered and the user handler never
/// runs, so the sample is a single-clock round-trip of the messaging/routing
/// path only. `request_size`/`response_size` make the probe carry a
/// real-payload-sized body and ask the producer to reply with `response_size`
/// bytes. Returns `(elapsed, response_bytes_received)`. Shared by the service
/// and action `probe_latency` methods.
pub(super) async fn probe_round_trip(
    messenger: &MessengerHandle,
    sender: &ServiceWireSender,
    request_size: usize,
    response_size: u32,
    response_timeout: Duration,
) -> Result<(Duration, usize)> {
    let request = Payload::from(pmi::build_sized_probe_request(request_size, response_size));
    let started = Instant::now();
    let reply = messenger
        .poll_service(sender, request, ServiceQueryKind::Probe, response_timeout)
        .await?;
    Ok((started.elapsed(), reply.payload_bytes().len()))
}
