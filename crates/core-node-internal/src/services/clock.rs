use crate::Result;
use crate::names;
use config::node::QoSProfile;
use core_node_api::encoding::{ClockRequest, ClockResponse, ClockTick};
use peppylib::clock::wall_now_ns;
use peppylib::messaging::{SenderTarget, ServiceRequestContext, Subscription, TopicPublisher};
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger, TopicMessenger};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

/// Failures observable from a [`ClockSource`]. Wall mode propagates a system
/// clock error; sim mode reports a missing first tick.
#[derive(Debug, thiserror::Error)]
pub enum ClockSourceError {
    #[error("system clock unavailable: {0}")]
    Wall(String),
    #[error("clock not ready: no external tick observed yet (sim mode)")]
    NotReady,
}

/// Daemon-internal abstraction over "what time is it". The clock service
/// handler and the periodic publisher both go through this trait so the
/// daemon can serve sim/replay timestamps without changing the wire.
pub trait ClockSource: Send + Sync {
    fn now_ns(&self) -> std::result::Result<u64, ClockSourceError>;
}

/// Reads OS wall time. Used when the daemon resolves the clock source to
/// `wall` (the default).
pub struct WallClockSource;

impl ClockSource for WallClockSource {
    fn now_ns(&self) -> std::result::Result<u64, ClockSourceError> {
        wall_now_ns().map_err(|e| ClockSourceError::Wall(e.to_string()))
    }
}

/// Serves timestamps from a daemon-internal cache fed by a subscription to
/// the `clock` topic. `0` is reserved as "no tick observed yet" so the
/// handler can return `NotReady` instead of a misleading zero timestamp.
pub struct SimClockSource {
    cache: Arc<AtomicU64>,
}

impl SimClockSource {
    pub fn new(cache: Arc<AtomicU64>) -> Self {
        Self { cache }
    }
}

impl ClockSource for SimClockSource {
    fn now_ns(&self) -> std::result::Result<u64, ClockSourceError> {
        match self.cache.load(Ordering::Relaxed) {
            0 => Err(ClockSourceError::NotReady),
            ns => Ok(ns),
        }
    }
}

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
        names::CLOCK,
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

fn handle_clock_request(
    source: &dyn ClockSource,
    context: ServiceRequestContext,
) -> PeppyResult<Payload> {
    // Stamp t1 first: every line after this point inflates server processing
    // time and corrupts the offset estimate the client computes.
    let server_recv_time = source
        .now_ns()
        .map_err(|e| PeppyError::InvalidServiceRequest {
            identifier: context.message().instance_id().to_string(),
            reason: e.to_string(),
        })?;
    let instance_id = context.message().instance_id().to_string();
    handle_clock_request_inner(source, &context, server_recv_time).map_err(|e| {
        PeppyError::InvalidServiceRequest {
            identifier: instance_id,
            reason: e.to_string(),
        }
    })
}

fn handle_clock_request_inner(
    source: &dyn ClockSource,
    context: &ServiceRequestContext,
    server_recv_time: u64,
) -> Result<Payload> {
    let request = ClockRequest::decode(context.message().payload().as_ref())?;

    debug!(
        "Received clock request from {}, t0={}",
        context.message().instance_id(),
        request.client_send_time,
    );

    // Stamp t2 last: the response encode + send happens after this point and
    // is part of the round-trip delay the client measures, not server time.
    let server_send_time = source
        .now_ns()
        .map_err(|e| PeppyError::InvalidServiceRequest {
            identifier: context.message().instance_id().to_string(),
            reason: e.to_string(),
        })?;

    ClockResponse::new(request.client_send_time, server_recv_time, server_send_time)
        .encode()
        .map_err(Into::into)
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
        names::CLOCK,
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
        names::DAEMON_HEARTBEAT,
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

/// Minimum interval between successive name-collision alarms: the heartbeat
/// ticks every few seconds, and one error per minute is enough to be seen
/// without flooding the log.
const NAME_COLLISION_ALARM_COOLDOWN: Duration = Duration::from_secs(60);

/// Decides whether a beat observed on the daemon's own heartbeat topic
/// warrants a name-collision alarm. The daemon receives its own beats via
/// same-session loopback, so the per-boot instance id is the discriminator:
/// a foreign instance id means another daemon publishes under this daemon's
/// core-node name. Alarms are rate-limited to one per
/// [`NAME_COLLISION_ALARM_COOLDOWN`].
fn name_collision_alarm_due(
    own_instance_id: &str,
    beat_instance_id: &str,
    last_alarm: Option<Instant>,
    now: Instant,
) -> bool {
    beat_instance_id != own_instance_id
        && last_alarm.is_none_or(|at| now.duration_since(at) >= NAME_COLLISION_ALARM_COOLDOWN)
}

/// Subscribes to the daemon's **own** `daemon_heartbeat` topic and raises a
/// rate-limited alarm whenever a beat arrives from a foreign instance: two
/// daemons publishing under one core-node name break the name-based routing
/// invariant. This is the runtime complement to the boot-time self-probe
/// (`CoreNode::ensure_name_unclaimed`), catching daemons that join the
/// router/federation after this one booted. Log, don't kill: tearing down a
/// live daemon over a collision would take its spawned nodes down with it.
pub async fn watch_for_name_collision(
    messenger: MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_name: &str,
    cancel: CancellationToken,
) -> Result<JoinHandle<Result<()>>> {
    let mut subscription: Subscription = TopicMessenger::subscribe(
        &messenger,
        core_node_name,
        instance_id,
        Some(SenderTarget::node(node_name, names::CORE_NODE_TAG)?),
        false,
        names::DAEMON_HEARTBEAT,
        &peppylib::messaging::ConsumerFilter::Any,
        QoSProfile::SensorData,
    )
    .await?;

    let own_instance_id = instance_id.to_string();
    let node_name = node_name.to_string();
    Ok(tokio::spawn(async move {
        let mut last_alarm: Option<Instant> = None;
        loop {
            let message = tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                message = subscription.on_next_message() => match message {
                    Some(message) => message,
                    None => break,
                },
            };
            let now = Instant::now();
            if name_collision_alarm_due(&own_instance_id, message.instance_id(), last_alarm, now) {
                last_alarm = Some(now);
                error!(
                    "core-node name collision: daemon instance '{}' is publishing heartbeats \
                     under this daemon's name '{}' (own instance '{}'); core-node API calls \
                     route by name and may land on either daemon. Set `core_node_name` in \
                     `~/.peppy/conf/peppy_config.json5` on one of them to a unique name and \
                     restart it",
                    message.instance_id(),
                    node_name,
                    own_instance_id,
                );
            }
        }
        Ok(())
    }))
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
    let mut subscription: Subscription = TopicMessenger::subscribe(
        &messenger,
        core_node_name,
        instance_id,
        Some(SenderTarget::node(node_name, names::CORE_NODE_TAG)?),
        false,
        names::CLOCK,
        &peppylib::messaging::ConsumerFilter::Any,
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
            match ClockTick::decode(message.payload().as_ref()) {
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
    use pmi::{Messenger, MessengerAdapter, MessengerBackend, MockAdapter};
    use tokio::sync::Mutex;

    /// A messenger handle over a started in-memory mock session, enough for the
    /// publisher loops to declare a publisher and tick.
    async fn started_mock_messenger() -> MessengerHandle {
        let mut messenger = Messenger::new(MessengerAdapter::Mock(MockAdapter::default()));
        messenger
            .start_session()
            .await
            .expect("mock session should start");
        MessengerHandle::from_shared(Arc::new(Mutex::new(messenger)))
    }

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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn collision_watch_stops_promptly_on_cancel() {
        let cancel = CancellationToken::new();
        let handle = watch_for_name_collision(
            started_mock_messenger().await,
            "test_core_node",
            "test_instance",
            "test_core_node",
            cancel.clone(),
        )
        .await
        .expect("collision watch should spawn");

        cancel.cancel();

        let outcome = tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("watch must stop promptly after cancel, not spin")
            .expect("watch task should not panic");
        outcome.expect("watch should exit Ok after a clean cancel");
    }

    /// A heartbeat from a foreign instance under this daemon's name is the
    /// runtime signature of a name collision and must alarm; the daemon's own
    /// loopback beats must not.
    #[test]
    fn collision_alarm_fires_on_foreign_beat_only() {
        let now = Instant::now();
        assert!(name_collision_alarm_due(
            "own_instance",
            "foreign_instance",
            None,
            now
        ));
        assert!(!name_collision_alarm_due(
            "own_instance",
            "own_instance",
            None,
            now
        ));
    }

    #[test]
    fn collision_alarm_is_rate_limited() {
        let fired_at = Instant::now();
        assert!(!name_collision_alarm_due(
            "own_instance",
            "foreign_instance",
            Some(fired_at),
            fired_at + Duration::from_secs(1),
        ));
        assert!(name_collision_alarm_due(
            "own_instance",
            "foreign_instance",
            Some(fired_at),
            fired_at + NAME_COLLISION_ALARM_COOLDOWN,
        ));
    }

    #[test]
    fn wall_clock_source_returns_a_value() {
        let now = WallClockSource.now_ns().expect("system clock available");
        assert!(now > 0);
    }

    #[test]
    fn sim_clock_source_reports_not_ready_until_first_tick() {
        let cache = Arc::new(AtomicU64::new(0));
        let source = SimClockSource::new(Arc::clone(&cache));
        let err = source
            .now_ns()
            .expect_err("empty cache must surface NotReady");
        assert!(matches!(err, ClockSourceError::NotReady));

        cache.store(42, Ordering::Relaxed);
        assert_eq!(source.now_ns().expect("cache populated"), 42);
    }
}
