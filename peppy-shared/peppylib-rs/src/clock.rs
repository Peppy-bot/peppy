//! High-level wrapper around the `CLOCK` service.
//!
//! Performs an NTP-style 4-timestamp exchange with the core node and returns
//! the offset of the local clock relative to the core node's clock plus the
//! round-trip delay. `synchronize` does not adjust the local clock — it only
//! measures. Callers that want a "core-node-aligned" timestamp use
//! `local_now() + sync.offset_ns`.
//!
//! Unlike a raw [`crate::core_node::transport::poll`], which returns the
//! wire response and requires the caller to thread routing parameters and
//! timestamp stamping through by hand, this layer takes a [`NodeRunner`]
//! directly and performs the t0/t3 stamping itself.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use config::node::QoSProfile;
use config::runtime::{ClockBinding, ClockDomainId, ClockRole, ProducerRef};
use core_node_api::encoding::{ClockRequest, ClockResponse, ClockTick};
use core_node_api::{TopicId, names};

use crate::core_node::transport::poll;
use crate::error::{Error, Result};
use crate::messaging::{
    SenderTarget, ServiceRequestContext, Subscription, TopicMessenger, TopicPublisher,
};
use crate::runtime::{NodeRunner, TaskHandle, spawn};
use crate::types::Payload;

const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

/// Failures observable from a [`ClockSource`]. Wall mode propagates a system
/// clock error; sim mode reports a missing first tick.
#[derive(Debug, thiserror::Error)]
pub enum ClockSourceError {
    #[error("system clock unavailable: {0}")]
    Wall(String),
    #[error("clock not ready: this clock domain has published no tick yet")]
    NotReady,
}

/// Server-side abstraction over "what time is it". The `clock` service
/// handler ([`handle_clock_request`]) and the periodic tick publishers go
/// through this trait so a server can serve sim/replay timestamps without
/// changing the wire. Lives here (not in the daemon) so the daemon and the
/// test harness's clock stand-in serve literally the same semantics.
pub trait ClockSource: Send + Sync {
    fn now_ns(&self) -> std::result::Result<u64, ClockSourceError>;
}

/// Reads OS wall time. Used when the serving side resolves the clock source
/// to `wall` (the default).
pub struct WallClockSource;

impl ClockSource for WallClockSource {
    fn now_ns(&self) -> std::result::Result<u64, ClockSourceError> {
        wall_now_ns().map_err(|e| ClockSourceError::Wall(e.to_string()))
    }
}

/// Serves timestamps from a domain's cache, filled by the subscription a
/// consumer feeds or by the instant a publisher commits. `0` is reserved as
/// "no tick observed yet" and reads back as [`ClockSourceError::NotReady`].
struct SimClockSource {
    cache: Arc<AtomicU64>,
}

impl SimClockSource {
    fn new(cache: Arc<AtomicU64>) -> Self {
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

/// Answers one `clock` service request from `source`: the server half of the
/// NTP-style exchange [`synchronize`] performs. The single implementation of
/// the t1/t2 stamping discipline, shared by the daemon's clock service and
/// the test harness's stand-in.
pub fn handle_clock_request(
    source: &dyn ClockSource,
    context: ServiceRequestContext,
) -> Result<Payload> {
    // Stamp t1 first: every line after this point inflates server processing
    // time and corrupts the offset estimate the client computes.
    let server_recv_time = source.now_ns().map_err(|e| Error::InvalidServiceRequest {
        identifier: context.message().instance_id().to_string(),
        reason: e.to_string(),
    })?;
    let instance_id = context.message().instance_id().to_string();
    handle_clock_request_inner(source, &context, server_recv_time).map_err(|e| {
        Error::InvalidServiceRequest {
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
    let request = ClockRequest::decode(context.message().payload_bytes().as_ref())?;

    // Stamp t2 last: the response encode + send happens after this point and
    // is part of the round-trip delay the client measures, not server time.
    let server_send_time = source.now_ns().map_err(|e| Error::InvalidServiceRequest {
        identifier: context.message().instance_id().to_string(),
        reason: e.to_string(),
    })?;

    ClockResponse::new(request.client_send_time, server_recv_time, server_send_time)
        .encode()
        .map_err(Into::into)
}

/// Wall-clock "now" in nanoseconds since the UNIX epoch — the canonical reader
/// on the publish/poll paths and in tests. Returns an error if the system clock
/// is set before the epoch; saturates to `u64::MAX` if the timestamp would
/// overflow `u64` (post-year-2554, unreachable in practice).
///
/// Lives in `peppylib` (the lowest crate shared by both the daemon and clients)
/// rather than in `core-node-api`: reading the system clock is a side effect a
/// pure wire-codec crate should not perform.
pub fn wall_now_ns() -> Result<u64> {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    Ok(u64::try_from(nanos).unwrap_or(u64::MAX))
}

/// Result of an NTP-style clock-sync exchange with the core node.
#[derive(Debug, Clone)]
pub struct ClockSync {
    /// `local + offset_ns ≈ core_node`. Signed because the local clock can lead
    /// the core node's clock.
    pub offset_ns: i64,
    /// Round-trip network delay observed during the exchange.
    pub round_trip_delay_ns: u64,
    /// Raw wire response, exposed for callers that want the individual t0/t1/t2.
    pub raw: ClockResponse,
}

pub async fn synchronize(
    node_runner: &NodeRunner,
    response_timeout: Option<Duration>,
) -> Result<ClockSync> {
    let timeout = response_timeout.unwrap_or(DEFAULT_RESPONSE_TIMEOUT);
    let processor = node_runner.processor();
    let core_node = processor.bound_core_node();

    let t0 = wall_now_ns()?;
    let response = poll(
        &ClockRequest::new(t0),
        node_runner.messenger(),
        core_node,
        processor.bound_instance_id(),
        core_node,
        timeout,
    )
    .await?;
    let t3 = wall_now_ns()?;

    let (offset_ns, round_trip_delay_ns) =
        compute_sync(t0, response.server_recv_time, response.server_send_time, t3);

    Ok(ClockSync {
        offset_ns,
        round_trip_delay_ns,
        raw: response,
    })
}

/// Subscription handle returned by [`subscribe`]. Each call to
/// [`ClockSubscription::on_next_tick`] yields the next decoded [`ClockTick`].
pub struct ClockSubscription {
    inner: Subscription,
}

impl ClockSubscription {
    /// Wait for the next tick from the core node's `clock` topic. Returns
    /// `Ok(None)` if the underlying subscription closes.
    pub async fn on_next_tick(&mut self) -> Result<Option<ClockTick>> {
        match self.inner.on_next_message().await {
            Some(message) => {
                let tick = ClockTick::decode(message.payload_bytes().as_ref())?;
                Ok(Some(tick))
            }
            None => Ok(None),
        }
    }

    /// Unwrap the typed wrapper to get the raw `Subscription` underneath.
    /// Used by the Python bindings so they can lock the subscription directly
    /// instead of stacking a second `Arc<Mutex<_>>` over this thin wrapper.
    pub fn into_inner(self) -> Subscription {
        self.inner
    }
}

/// User-facing clock handle used by hot-path code that needs "what time is
/// it now" without knowing which clock its deployment gave it. `now_ns` is
/// sync, allocation-free, and safe to call repeatedly.
///
/// Build via [`for_node`]; the constructor reads the instance's one binding
/// and installs the matching source. The generator emits a pre-bound
/// `peppygen::clock::now_ns()` free function so user code never threads a
/// `PeppyClock` around.
pub struct PeppyClock {
    inner: PeppyClockInner,
}

enum PeppyClockInner {
    Wall,
    /// Reads the domain's ticks as they arrive, cached by a feeder task.
    Consumer {
        source: SimClockSource,
        // tokio's `JoinHandle` only detaches on drop, so the `Drop` impl
        // below must `abort()` to actually cancel the subscriber task.
        feeder: TaskHandle<Result<()>>,
    },
    /// Reads what this instance last committed through its
    /// [`ClockPublisher`], which shares the same cache.
    Publisher {
        source: SimClockSource,
    },
}

impl Drop for PeppyClockInner {
    fn drop(&mut self) {
        match self {
            PeppyClockInner::Wall | PeppyClockInner::Publisher { .. } => {}
            PeppyClockInner::Consumer { feeder, .. } => feeder.abort(),
        }
    }
}

impl PeppyClock {
    /// Read the current time in nanoseconds since the Unix epoch, on this
    /// instance's own clock. Wall time is the local OS clock. A simulated
    /// domain is the last instant its publisher supplied, or
    /// [`Error::ClockNotReady`] before the first one.
    pub fn now_ns(&self) -> Result<u64> {
        let source = match &self.inner {
            PeppyClockInner::Wall => return Ok(wall_now_ns()?),
            PeppyClockInner::Consumer { source, .. } | PeppyClockInner::Publisher { source } => {
                source
            }
        };
        source.now_ns().map_err(|error| match error {
            ClockSourceError::NotReady => Error::ClockNotReady,
            ClockSourceError::Wall(reason) => Error::Io(std::io::Error::other(reason)),
        })
    }
}

/// Build a [`PeppyClock`] for `node_runner` from the clock its deployment
/// bound it to.
///
/// A consumer opens its domain's subscription up front, so the first
/// `now_ns()` after a tick lands returns without setup latency. A publisher
/// opens nothing: it reads the cache its own [`ClockPublisher::publish`]
/// commits to, because a source that waited for its own tick to return
/// through a subscription could never produce the first one. The async
/// surface is in the constructor so the hot-path read stays sync.
pub async fn for_node(node_runner: &NodeRunner) -> Result<PeppyClock> {
    let processor = node_runner.processor();
    let binding = processor.clock().clone();
    let cache = Arc::clone(processor.clock_cache());
    let inner = match binding {
        ClockBinding::Wall => PeppyClockInner::Wall,
        ClockBinding::Sim {
            role: ClockRole::Publisher,
            ..
        } => PeppyClockInner::Publisher {
            source: SimClockSource::new(cache),
        },
        ClockBinding::Sim {
            domain,
            role: ClockRole::Consumer { publisher },
        } => {
            let mut subscription = subscribe_domain(node_runner, &domain, &publisher).await?;
            let feeder_cache = Arc::clone(&cache);
            // The subscriber is detached: subscription drop happens via the
            // TaskHandle field on PeppyClock, which aborts the task and walks
            // the Subscription destructor.
            let feeder = spawn(async move {
                while let Some(message) = subscription.on_next_message().await {
                    // A decoded tick is never `0` (`ClockTick` clamps on
                    // decode), so storing it can never write the cache's
                    // not-ready sentinel.
                    if let Ok(tick) = ClockTick::decode(message.payload_bytes().as_ref()) {
                        feeder_cache.store(tick.time(), Ordering::Relaxed);
                    }
                }
                Ok(())
            });
            PeppyClockInner::Consumer {
                source: SimClockSource::new(cache),
                feeder,
            }
        }
    };
    Ok(PeppyClock { inner })
}

/// The wire target a domain's ticks are published on: the `clock` topic of
/// the machine hosting the domain, exactly where that machine's daemon
/// publishes its own wall ticks. The `link_id` segment tells the two apart: a
/// domain's carries its name and incarnation, the daemon's is the reserved
/// default, and each subscription names the one it reads.
fn domain_target(domain: &ClockDomainId) -> Result<crate::messaging::SenderTarget> {
    Ok(SenderTarget::node(
        domain.core_node.as_str(),
        names::CORE_NODE_TAG,
    )?)
}

/// Subscribe to `domain`'s stream, pinned to the instance that publishes it.
async fn subscribe_domain(
    node_runner: &NodeRunner,
    domain: &ClockDomainId,
    publisher: &ProducerRef,
) -> Result<Subscription> {
    let processor = node_runner.processor();
    subscribe_domain_stream(
        node_runner.messenger(),
        processor.bound_core_node(),
        processor.bound_instance_id(),
        domain,
        publisher,
    )
    .await
}

/// Subscribe to one domain's tick stream from outside a node runtime.
///
/// A node reads its own clock through [`for_node`], which needs no argument
/// because its binding says which domain and whose ticks. This is the same
/// subscription for a caller that holds only a messenger: the daemon hosting a
/// domain watches it this way, so `peppy clock list` can say whether the
/// domain has published an instant and what it was.
pub async fn subscribe_domain_stream(
    messenger: &crate::MessengerHandle,
    as_core_node: &str,
    as_instance_id: &str,
    domain: &ClockDomainId,
    publisher: &ProducerRef,
) -> Result<Subscription> {
    TopicMessenger::subscribe_publisher_pinned(
        messenger,
        as_core_node,
        as_instance_id,
        publisher,
        domain_target(domain)?,
        &domain.link_id(),
        TopicId::Clock.name(),
        QoSProfile::SensorData,
    )
    .await
}

/// Subscribe to the ticks of this instance's own clock.
///
/// Under wall time that is the bound core node's periodic `clock` topic, on
/// the reserved default `link_id` its daemon publishes under, so a domain
/// hosted on that machine stays out of this stream. Under a simulated domain
/// it is that domain's stream, whichever machine its publisher runs on, so a
/// source reading this sees exactly what it sent.
pub async fn subscribe(node_runner: &NodeRunner) -> Result<ClockSubscription> {
    let processor = node_runner.processor();
    let inner = match processor.clock().clone() {
        ClockBinding::Wall => {
            crate::core_node::subscribe_core_topic(node_runner, TopicId::Clock.name()).await?
        }
        ClockBinding::Sim { domain, role } => {
            let publisher = match role {
                ClockRole::Publisher => {
                    ProducerRef::new(processor.bound_core_node(), processor.bound_instance_id())
                }
                ClockRole::Consumer { publisher } => publisher,
            };
            subscribe_domain(node_runner, &domain, &publisher).await?
        }
    };
    Ok(ClockSubscription { inner })
}

/// The instance that supplies one simulated clock domain.
///
/// Held only by the instance a domain declaration named, so holding one IS
/// being that domain's source. Each `publish` commits the instant locally and
/// sends one tick on the domain's stream; every instance bound to the domain
/// reads that stream, wherever it runs.
pub struct ClockPublisher {
    domain: ClockDomainId,
    cache: Arc<AtomicU64>,
    publisher: TopicPublisher,
}

impl ClockPublisher {
    /// Builds the publisher for `node_runner` from its resolved binding, or
    /// `None` when its deployment did not name it a domain's publisher. A node
    /// that may or may not be a source branches on this one call.
    pub async fn for_node(node_runner: &NodeRunner) -> Result<Option<Self>> {
        let processor = node_runner.processor();
        let ClockBinding::Sim {
            domain,
            role: ClockRole::Publisher,
        } = processor.clock()
        else {
            return Ok(None);
        };
        let domain = domain.clone();
        let publisher = TopicMessenger::declare_publisher(
            node_runner.messenger(),
            processor.bound_core_node(),
            processor.bound_instance_id(),
            domain_target(&domain)?,
            Some(&domain.link_id()),
            TopicId::Clock.name(),
            QoSProfile::SensorData,
        )
        .await?;
        Ok(Some(Self {
            domain,
            cache: Arc::clone(processor.clock_cache()),
            publisher,
        }))
    }

    /// The domain this instance supplies.
    pub fn domain(&self) -> &ClockDomainId {
        &self.domain
    }

    /// Commits `time_ns` as this domain's current instant and sends it.
    ///
    /// The commit happens first, so the source's own `now_ns()` reports the
    /// instant it is publishing rather than waiting for the tick to return
    /// through a subscription, which is what lets it stamp the very data it
    /// emits for this step.
    pub async fn publish(&self, time_ns: u64) -> Result<()> {
        let tick = ClockTick::new(time_ns);
        self.cache.store(tick.time(), Ordering::Relaxed);
        self.publisher.publish(tick.encode()?).await
    }
}

fn compute_sync(t0: u64, t1: u64, t2: u64, t3: u64) -> (i64, u64) {
    // i128 widening: subtracting two u64s can underflow, and the standard NTP
    // formula sums two such differences before halving — we need headroom.
    // t1/t2 come from an unauthenticated peer, so saturate (don't wrap) on the
    // narrow back to i64/u64 — a misbehaving server could otherwise flip signs.
    let i = |x: u64| x as i128;
    let offset = ((i(t1) - i(t0)) + (i(t2) - i(t3))) / 2;
    let delay = (i(t3) - i(t0)) - (i(t2) - i(t1));

    let offset = i64::try_from(offset).unwrap_or(if offset > 0 { i64::MAX } else { i64::MIN });
    let delay = u64::try_from(delay.max(0)).unwrap_or(u64::MAX);
    (offset, delay)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peppy_clock_wall_returns_a_value() {
        let clock = PeppyClock {
            inner: PeppyClockInner::Wall,
        };
        let now = clock.now_ns().expect("wall should always succeed");
        assert!(now > 0);
    }

    #[test]
    fn wall_now_ns_is_past_2020_and_non_decreasing() {
        // Deterministic without sleeping: assert only invariants true of any
        // real clock — a timestamp past 2020-01-01 (1_577_836_800 s) and a
        // monotonic-ish non-regression between two back-to-back reads.
        const Y2020_NS: u64 = 1_577_836_800_000_000_000;
        let first = wall_now_ns().expect("clock read");
        let second = wall_now_ns().expect("clock read");
        assert!(first >= Y2020_NS, "clock before 2020: {first}");
        assert!(second >= first, "clock went backwards: {first} -> {second}");
    }

    #[tokio::test]
    async fn a_consumer_reports_not_ready_until_its_domain_ticks() {
        // Build the consumer shape without a live subscription by skipping
        // `for_node`; the feeder slot holds a parked task so the type is the
        // one a real consumer carries.
        let cache = Arc::new(AtomicU64::new(0));
        let cache_clone = Arc::clone(&cache);
        let feeder = spawn(async move {
            std::future::pending::<()>().await;
            Ok(())
        });
        let clock = PeppyClock {
            inner: PeppyClockInner::Consumer {
                source: SimClockSource::new(cache_clone),
                feeder,
            },
        };

        let err = clock
            .now_ns()
            .expect_err("empty cache must surface ClockNotReady");
        assert!(matches!(err, Error::ClockNotReady), "got {err:?}");

        cache.store(42, Ordering::Relaxed);
        assert_eq!(clock.now_ns().expect("populated cache reads ok"), 42);
    }

    /// A publisher reads the instant it committed with nothing published and
    /// no subscription open: producing the first tick cannot depend on reading
    /// a clock only this instance advances.
    #[test]
    fn a_publisher_reads_what_it_committed_without_a_transport_echo() {
        let cache = Arc::new(AtomicU64::new(0));
        let clock = PeppyClock {
            inner: PeppyClockInner::Publisher {
                source: SimClockSource::new(Arc::clone(&cache)),
            },
        };

        let err = clock
            .now_ns()
            .expect_err("nothing committed yet is not ready");
        assert!(matches!(err, Error::ClockNotReady), "got {err:?}");

        cache.store(7, Ordering::Relaxed);
        assert_eq!(clock.now_ns().expect("committed instant reads back"), 7);
    }
}

#[cfg(test)]
mod clock_source_tests {
    use super::*;

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

#[cfg(test)]
mod compute_sync_tests {
    use super::compute_sync;

    #[test]
    fn zero_offset_zero_delay() {
        let (offset, delay) = compute_sync(100, 100, 100, 100);
        assert_eq!(offset, 0);
        assert_eq!(delay, 0);
    }

    #[test]
    fn local_clock_lags_by_50_ns_with_no_delay() {
        // Local at t0=100, server stamps t1=t2=150 instantly, response at t3=100.
        // offset = ((150-100) + (150-100)) / 2 = 50.
        let (offset, delay) = compute_sync(100, 150, 150, 100);
        assert_eq!(offset, 50);
        assert_eq!(delay, 0);
    }

    #[test]
    fn symmetric_round_trip_with_offset() {
        // Local at t0=0; one-way delay = 10 ns; server processing = 5 ns;
        // server clock leads local by 100 ns.
        // t1 = 0 + 10 + 100 = 110
        // t2 = 110 + 5     = 115
        // t3 = 0 + 10 + 5 + 10 = 25
        // offset = ((110 - 0) + (115 - 25)) / 2 = (110 + 90) / 2 = 100.
        // delay  = (25 - 0) - (115 - 110)       = 25 - 5         =  20.
        let (offset, delay) = compute_sync(0, 110, 115, 25);
        assert_eq!(offset, 100);
        assert_eq!(delay, 20);
    }

    #[test]
    fn local_clock_leads_yields_negative_offset() {
        // Local at t0=200; server clock trails by 100 ns; instantaneous link.
        // t1 = t2 = 100, t3 = 200. offset = ((100-200)+(100-200))/2 = -100.
        let (offset, _) = compute_sync(200, 100, 100, 200);
        assert_eq!(offset, -100);
    }

    #[test]
    fn compute_sync_clamps_offset_overflow() {
        // Adversarial peer returns t1 = t2 = u64::MAX with a normal local clock.
        // Raw offset is ~u64::MAX (≈1.8e19), well above i64::MAX (≈9.2e18) —
        // narrowing without clamping would wrap to a negative value.
        let (offset, _) = compute_sync(0, u64::MAX, u64::MAX, 0);
        assert_eq!(offset, i64::MAX);
    }

    #[test]
    fn compute_sync_clamps_delay_overflow() {
        // delay = (t3 - t0) - (t2 - t1) = u64::MAX - (-u64::MAX) = 2*u64::MAX
        // in i128 — exceeds u64::MAX, so saturate rather than wrap.
        let (_, delay) = compute_sync(0, u64::MAX, 0, u64::MAX);
        assert_eq!(delay, u64::MAX);
    }
}
