//! High-level wrapper around the `CLOCK` service.
//!
//! Performs an NTP-style 4-timestamp exchange with the core node and returns
//! the offset of the local clock relative to the core node's clock plus the
//! round-trip delay. `synchronize` does not adjust the local clock — it only
//! measures. Callers that want a "core-node-aligned" timestamp use
//! `local_now() + sync.offset_ns`.
//!
//! Unlike [`crate::core_node::transport::poll_clock`], which returns the raw
//! wire response and requires the caller to thread routing parameters and
//! timestamp stamping through by hand, this layer takes a [`NodeRunner`]
//! directly and performs the t0/t3 stamping itself.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use core_node_api::encoding::{ClockRequest, ClockResponse, ClockTick};
use core_node_api::names;

use crate::core_node::transport::poll_clock;
use crate::error::{Error, Result};
use crate::messaging::Subscription;
use crate::runtime::{NodeRunner, TaskHandle, spawn};

const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

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
    let response = poll_clock(
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
/// it now" without knowing whether the node was launched in wall or sim
/// mode. `now_ns` is sync, allocation-free, and safe to call repeatedly.
///
/// Build via [`for_node`]; the constructor decides which underlying
/// source to install based on the daemon's resolved
/// `framework.use_sim_time`. The generator emits a pre-bound
/// `peppygen::clock::now_ns()` free function so user code never has to
/// thread a `PeppyClock` instance around.
pub struct PeppyClock {
    inner: PeppyClockInner,
}

enum PeppyClockInner {
    Wall,
    Sim {
        cache: Arc<AtomicU64>,
        // tokio's `JoinHandle` only detaches on drop, so the `Drop` impl
        // below must `abort()` to actually cancel the subscriber task.
        feeder: TaskHandle<Result<()>>,
    },
}

impl Drop for PeppyClockInner {
    fn drop(&mut self) {
        match self {
            PeppyClockInner::Wall => {}
            PeppyClockInner::Sim { feeder, .. } => feeder.abort(),
        }
    }
}

impl PeppyClock {
    /// Read the current core-node-aligned time in nanoseconds since the
    /// Unix epoch. In wall mode this is the local OS clock. In sim mode
    /// this is the most recently observed `ClockTick`, or
    /// [`Error::ClockNotReady`] if no tick has arrived yet.
    pub fn now_ns(&self) -> Result<u64> {
        match &self.inner {
            PeppyClockInner::Wall => Ok(wall_now_ns()?),
            PeppyClockInner::Sim { cache, .. } => match cache.load(Ordering::Relaxed) {
                0 => Err(Error::ClockNotReady),
                ns => Ok(ns),
            },
        }
    }
}

/// Build a [`PeppyClock`] for `node_runner`. Reads
/// `framework.use_sim_time` off the daemon-resolved runtime config and
/// installs either a wall-clock wrapper or a sim-clock subscriber.
///
/// In sim mode the constructor opens the `clock` subscription up front so
/// the first `now_ns()` call after a tick has been published returns
/// immediately without setup latency. The async surface is part of the
/// constructor so the hot-path read stays sync.
pub async fn for_node(node_runner: &NodeRunner) -> Result<PeppyClock> {
    if !node_runner.processor().use_sim_time() {
        return Ok(PeppyClock {
            inner: PeppyClockInner::Wall,
        });
    }

    let cache = Arc::new(AtomicU64::new(0));
    let mut subscription = subscribe(node_runner).await?.into_inner();
    let feeder_cache = Arc::clone(&cache);
    // The subscriber is detached: subscription drop happens via the
    // TaskHandle field on PeppyClock, which aborts the task and walks the
    // Subscription destructor.
    let feeder = spawn(async move {
        while let Some(message) = subscription.on_next_message().await {
            match ClockTick::decode(message.payload_bytes().as_ref()) {
                Ok(tick) => {
                    // 0 is the not-ready sentinel, so clamp to 1 if a
                    // simulator ever publishes a literal zero.
                    let stored = if tick.time == 0 { 1 } else { tick.time };
                    feeder_cache.store(stored, Ordering::Relaxed);
                }
                Err(_) => continue,
            }
        }
        Ok(())
    });

    Ok(PeppyClock {
        inner: PeppyClockInner::Sim { cache, feeder },
    })
}

/// Subscribe to the periodic `clock` topic on `node_runner`'s bound core node.
pub async fn subscribe(node_runner: &NodeRunner) -> Result<ClockSubscription> {
    let inner = crate::core_node::subscribe_core_topic(node_runner, names::CLOCK).await?;
    Ok(ClockSubscription { inner })
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
    async fn peppy_clock_sim_reports_not_ready_until_first_tick() {
        // Construct a sim clock without spawning a real subscriber by
        // skipping `for_node`. The feeder slot is filled with a
        // parked task that keeps the type signature consistent.
        let cache = Arc::new(AtomicU64::new(0));
        let cache_clone = Arc::clone(&cache);
        let feeder = spawn(async move {
            std::future::pending::<()>().await;
            Ok(())
        });
        let clock = PeppyClock {
            inner: PeppyClockInner::Sim {
                cache: cache_clone,
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
