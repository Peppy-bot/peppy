use crate::Result;
use crate::names;
use config::node::QoSProfile;
use core_node_api::encoding::{ClockRequest, ClockResponse, ClockTick, wall_now_ns};
use peppylib::messaging::{ServiceRequestContext, TopicPublisher};
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger, TopicMessenger};
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

pub async fn listen_for_clock(
    messenger: &MessengerHandle,
    core_node_node: &str,
    instance_id: &str,
    node_name: &str,
) -> Result<JoinHandle<Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        core_node_node,
        instance_id,
        node_name,
        names::CLOCK,
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(handle_clock_request)
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

async fn handle_clock_request(context: ServiceRequestContext) -> PeppyResult<Payload> {
    // Stamp t1 first — every line after this point inflates server processing
    // time and corrupts the offset estimate the client computes.
    let server_recv_time = wall_now_ns().map_err(|e| PeppyError::InvalidServiceRequest {
        identifier: context.message().instance_id().to_string(),
        reason: format!("server clock unavailable: {e}"),
    })?;
    let instance_id = context.message().instance_id().to_string();
    handle_clock_request_inner(&context, server_recv_time).map_err(|e| {
        PeppyError::InvalidServiceRequest {
            identifier: instance_id,
            reason: e.to_string(),
        }
    })
}

fn handle_clock_request_inner(
    context: &ServiceRequestContext,
    server_recv_time: u64,
) -> Result<Payload> {
    let request = ClockRequest::decode(context.message().payload().as_ref())?;

    debug!(
        "Received clock request from {}, t0={}",
        context.message().instance_id(),
        request.client_send_time,
    );

    // Stamp t2 last — the response encode + send happens after this point and
    // is part of the round-trip delay the client measures, not server time.
    let server_send_time = wall_now_ns()?;

    ClockResponse::new(request.client_send_time, server_recv_time, server_send_time)
        .encode()
        .map_err(Into::into)
}

/// Spawns a task that emits a `ClockTick` on the `clock` topic at every
/// `interval`. `SensorData` QoS so a slow subscriber gets newer ticks dropped
/// rather than back-pressuring the publisher — stale clock values are useless.
///
/// Pre-binds a [`TopicPublisher`] outside the loop: the wire key is formatted
/// once at startup, and per-tick `publish` skips the central messenger mutex.
pub async fn publish_clock(
    messenger: MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_name: &str,
    interval: Duration,
) -> Result<JoinHandle<Result<()>>> {
    let publisher = TopicMessenger::declare_publisher(
        &messenger,
        core_node_name,
        instance_id,
        node_name,
        names::CLOCK,
        QoSProfile::SensorData,
    )
    .await?;
    Ok(tokio::spawn(run_clock_publisher(publisher, interval)))
}

async fn run_clock_publisher(publisher: TopicPublisher, interval: Duration) -> Result<()> {
    let mut ticker = tokio::time::interval(interval);
    // Skip catch-up bursts after a backlog (e.g. test pause / GC stall).
    // A clock-tick ten ticks late is uninteresting — we want fresh time.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;
        let now_ns = match wall_now_ns() {
            Ok(t) => t,
            Err(e) => {
                warn!("clock tick skipped, system clock unavailable: {e}");
                continue;
            }
        };
        let payload = match ClockTick::new(now_ns).encode() {
            Ok(p) => p,
            Err(e) => {
                warn!("clock tick encode failed: {e}");
                continue;
            }
        };
        if let Err(e) = publisher.publish(payload).await {
            warn!("clock tick emit failed: {e}");
        }
    }
}
