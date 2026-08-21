use crate::Result;
use config::node::QoSProfile;
use core_node_api::encoding::ClockTick;
use core_node_api::names;
use core_node_api::{ServiceId, TopicId};
// The clock-source abstraction and the NTP-style request handler live in
// `peppylib::clock`, shared with the test harness's clock stand-in
// (`peppylib::testing::MockClock`) so both serve identical semantics.
use peppylib::clock::handle_clock_request;
pub use peppylib::clock::{ClockSource, SimClockSource, WallClockSource};
use peppylib::messaging::{SenderTarget, Subscription, TopicPublisher};
use peppylib::{MessengerHandle, ServiceMessenger, TopicMessenger};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::warn;

pub async fn listen_for_clock(
    messenger: &MessengerHandle,
    core_node_node: &str,
    instance_id: &str,
    node_name: &str,
    source: Arc<dyn ClockSource>,
) -> Result<JoinHandle<Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        core_node_node,
        instance_id,
        SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
        ServiceId::Clock.name(),
    )
    .await?;

    let handle = tokio::spawn(async move {
        endpoint
            .handle_requests(move |context| {
                let source = Arc::clone(&source);
                async move { handle_clock_request(source.as_ref(), context) }
            })
            .await
            .map_err(Into::into)
    });

    Ok(handle)
}

/// Spawns a task that emits a `ClockTick` on the `clock` topic at every
/// `interval`. `SensorData` QoS so a slow subscriber gets newer ticks dropped
/// rather than back-pressuring the publisher; stale clock values are useless.
///
/// Pre-binds a [`TopicPublisher`] outside the loop: the wire key is formatted
/// once at startup, and per-tick `publish` skips the central messenger mutex.
///
/// Wall mode only. In sim mode the daemon does not publish; an external
/// simulator does, and the daemon merely subscribes (see
/// [`subscribe_external_clock`]).
///
/// `cancel` stops the loop on daemon shutdown so it does not spin against a
/// closed session, logging a failed publish on every tick.
pub async fn publish_clock(
    messenger: MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_name: &str,
    interval: Duration,
    source: Arc<dyn ClockSource>,
    cancel: CancellationToken,
) -> Result<JoinHandle<Result<()>>> {
    let publisher = declare_sensor_publisher(
        &messenger,
        core_node_name,
        instance_id,
        node_name,
        TopicId::Clock.name(),
    )
    .await?;
    Ok(tokio::spawn(run_clock_publisher(
        publisher, interval, source, cancel,
    )))
}

/// Declares a pre-bound `SensorData` publisher on a core-node topic: the wire
/// key is formatted once at startup, and per-tick `publish` skips the central
/// messenger mutex. Shared by the clock and daemon-heartbeat publishers.
async fn declare_sensor_publisher(
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_name: &str,
    topic: &str,
) -> Result<TopicPublisher> {
    TopicMessenger::declare_publisher(
        messenger,
        core_node_name,
        instance_id,
        SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
        None,
        topic,
        QoSProfile::SensorData,
    )
    .await
    .map_err(Into::into)
}

async fn run_clock_publisher(
    publisher: TopicPublisher,
    interval: Duration,
    source: Arc<dyn ClockSource>,
    cancel: CancellationToken,
) -> Result<()> {
    let mut ticker = tokio::time::interval(interval);
    // Skip catch-up bursts after a backlog (e.g. test pause / GC stall).
    // A clock-tick ten ticks late is uninteresting; we want fresh time.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            // Check cancellation first so a shutdown that races a due tick wins,
            // stopping the loop instead of emitting one last doomed publish.
            biased;
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {}
        }
        let now_ns = match source.now_ns() {
            Ok(t) => t,
            Err(e) => {
                warn!("clock tick skipped, {e}");
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
    Ok(())
}

/// Spawns a task that emits a liveness beat on the `daemon_heartbeat` topic at
/// every `interval`, in BOTH wall and sim mode (liveness must not depend on the
/// clock topology; in sim mode the daemon does not publish the clock at all).
///
/// Each spawned node runs a watchdog subscribed to this topic; if the beats go
/// silent past the configured grace period the node shuts itself down, so an
/// uncatchable daemon death (SIGKILL / OOM / crash) does not leave orphans. The
/// payload is a constant `ClockTick(0)` reused purely as a cheap carrier; the
/// node only cares that a message arrived, not its value.
///
/// `SensorData` QoS (best-effort, newest-wins, no back-pressure) is correct for
/// a beacon: a missed beat is harmless as long as the next arrives inside grace.
pub async fn publish_daemon_heartbeat(
    messenger: MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_name: &str,
    interval: Duration,
    cancel: CancellationToken,
) -> Result<JoinHandle<Result<()>>> {
    let publisher = declare_sensor_publisher(
        &messenger,
        core_node_name,
        instance_id,
        node_name,
        TopicId::DaemonHeartbeat.name(),
    )
    .await?;
    Ok(tokio::spawn(run_heartbeat_publisher(
        publisher, interval, cancel,
    )))
}

async fn run_heartbeat_publisher(
    publisher: TopicPublisher,
    interval: Duration,
    cancel: CancellationToken,
) -> Result<()> {
    // The value is never read by the node (only the message's arrival matters)
    // so encode the constant payload once, outside the loop.
    let payload = ClockTick::new(0).encode()?;
    let mut ticker = tokio::time::interval(interval);
    // A late beat is uninteresting; skip catch-up bursts after a stall.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            // Stop cleanly on shutdown rather than beating into a closed session.
            biased;
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {}
        }
        if let Err(e) = publisher.publish(payload.clone()).await {
            warn!("daemon heartbeat emit failed: {e}");
        }
    }
    Ok(())
}

/// Subscribes to the `clock` topic and feeds the latest observed timestamp
/// into `cache`. Spawned in sim mode in lieu of [`publish_clock`]: the daemon
/// is one of many subscribers to the external simulator's tick stream, and
/// uses the cached value to answer `synchronize` requests via
/// [`SimClockSource`].
///
/// `cache` is shared with the `SimClockSource` instance handed to
/// [`listen_for_clock`]. The two halves are decoupled: this task can fall
/// behind without blocking the service handler, which simply observes a
/// stale (or missing) value.
pub async fn subscribe_external_clock(
    messenger: MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_name: &str,
    cache: Arc<AtomicU64>,
    cancel: CancellationToken,
) -> Result<JoinHandle<Result<()>>> {
    let mut subscription: Subscription = TopicMessenger::subscribe_target_scoped(
        &messenger,
        core_node_name,
        instance_id,
        SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
        TopicId::Clock.name(),
        QoSProfile::SensorData,
    )
    .await?;

    Ok(tokio::spawn(async move {
        loop {
            // The subscription also ends on session close, but selecting on the
            // shutdown token makes the exit deterministic and matches the
            // publisher loops.
            let message = tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                message = subscription.on_next_message() => match message {
                    Some(message) => message,
                    None => break,
                },
            };
            match ClockTick::decode(message.payload_bytes().as_ref()) {
                Ok(tick) => {
                    // `0` is reserved as "not ready"; a simulator publishing
                    // a literal zero would be a bug, and clamping it to 1 is a
                    // safer surprise than silently masking the not-ready state.
                    let stored = if tick.time == 0 { 1 } else { tick.time };
                    cache.store(stored, Ordering::Relaxed);
                }
                Err(e) => warn!("dropped malformed clock tick: {e}"),
            }
        }
        Ok(())
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::tests::started_mock_messenger;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn clock_publisher_stops_promptly_on_cancel() {
        let cancel = CancellationToken::new();
        let handle = publish_clock(
            started_mock_messenger().await,
            "test_core_node",
            "test_instance",
            "test_core_node",
            Duration::from_millis(10),
            Arc::new(WallClockSource),
            cancel.clone(),
        )
        .await
        .expect("clock publisher should spawn");

        // Let it run a few ticks, then ask it to stop.
        tokio::time::sleep(Duration::from_millis(35)).await;
        cancel.cancel();

        let outcome = tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("publisher must stop promptly after cancel, not spin")
            .expect("publisher task should not panic");
        outcome.expect("publisher should exit Ok after a clean cancel");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn heartbeat_publisher_stops_promptly_on_cancel() {
        let cancel = CancellationToken::new();
        let handle = publish_daemon_heartbeat(
            started_mock_messenger().await,
            "test_core_node",
            "test_instance",
            "test_core_node",
            Duration::from_millis(10),
            cancel.clone(),
        )
        .await
        .expect("heartbeat publisher should spawn");

        tokio::time::sleep(Duration::from_millis(35)).await;
        cancel.cancel();

        let outcome = tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("heartbeat must stop promptly after cancel, not spin")
            .expect("heartbeat task should not panic");
        outcome.expect("heartbeat should exit Ok after a clean cancel");
    }
}
