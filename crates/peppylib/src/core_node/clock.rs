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

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use config::node::QoSProfile;
use core_node_api::encoding::{ClockRequest, ClockResponse, ClockSource, ClockTick};
use core_node_api::names;

use crate::core_node::transport::poll_clock;
use crate::error::Result;
use crate::messaging::{Subscription, TopicMessenger};
use crate::runtime::NodeRunner;

const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

/// Result of an NTP-style clock-sync exchange with the core node.
#[derive(Debug, Clone)]
pub struct ClockSync {
    /// `local + offset_ns ≈ core_node`. Signed because the local clock can lead
    /// the core node's clock.
    pub offset_ns: i64,
    /// Round-trip network delay observed during the exchange.
    pub round_trip_delay_ns: u64,
    /// Which clock source the core node served the response from.
    pub clock_source: ClockSource,
    /// Raw wire response, exposed for callers that want the individual t0/t1/t2.
    pub raw: ClockResponse,
}

pub async fn synchronize(
    node_runner: &NodeRunner,
    response_timeout: impl Into<Option<Duration>> + Send,
) -> Result<ClockSync> {
    let timeout = response_timeout.into().unwrap_or(DEFAULT_RESPONSE_TIMEOUT);
    let processor = node_runner.processor();
    let core_node = processor.bound_core_node();

    let t0 = now_ns();
    let response = poll_clock(
        &ClockRequest::new(t0),
        node_runner.messenger(),
        core_node,
        processor.bound_instance_id(),
        core_node,
        timeout,
    )
    .await?;
    let t3 = now_ns();

    let (offset_ns, round_trip_delay_ns) =
        compute_sync(t0, response.server_recv_time, response.server_send_time, t3);

    Ok(ClockSync {
        offset_ns,
        round_trip_delay_ns,
        clock_source: response.clock_source,
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
                let tick = ClockTick::decode(message.payload().as_ref())?;
                Ok(Some(tick))
            }
            None => Ok(None),
        }
    }
}

/// Subscribe to the periodic `clock` topic on `node_runner`'s bound core node.
///
/// Mirrors the shape of [`info`](super::info::info) and
/// [`stack_list`](super::stack::stack_list): the helper takes a [`NodeRunner`]
/// and threads the routing parameters and `SensorData` QoS profile through
/// itself, so callers don't see them.
pub async fn subscribe(node_runner: &NodeRunner) -> Result<ClockSubscription> {
    let processor = node_runner.processor();
    let core_node = processor.bound_core_node();
    let inner = TopicMessenger::subscribe(
        node_runner.messenger(),
        // The publisher's wire key hard-codes `*` into the caller-identity
        // slots (see `emit_topic_message`); the mock matcher is unidirectional,
        // so subscribers must mirror the wildcards on their side. Real Zenoh
        // would accept either form.
        "*",
        "*",
        core_node,
        names::CLOCK,
        Some(core_node),
        None,
        QoSProfile::SensorData,
    )
    .await?;
    Ok(ClockSubscription { inner })
}

fn compute_sync(t0: u64, t1: u64, t2: u64, t3: u64) -> (i64, u64) {
    // i128 widening: subtracting two u64s can underflow, and the standard NTP
    // formula sums two such differences before halving — we need headroom.
    // Saturating narrow on the way back down: t1/t2 come from an unauthenticated
    // peer, so a misbehaving server could otherwise wrap us silently.
    let offset = ((t1 as i128 - t0 as i128) + (t2 as i128 - t3 as i128)) / 2;
    let delay = (t3 as i128 - t0 as i128) - (t2 as i128 - t1 as i128);

    let offset = offset.clamp(i64::MIN as i128, i64::MAX as i128) as i64;
    let delay = delay.clamp(0, u64::MAX as i128) as u64;
    (offset, delay)
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
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
