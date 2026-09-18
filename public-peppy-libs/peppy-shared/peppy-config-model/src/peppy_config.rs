//! Shared remnants of the daemon-global `peppy_config.json5` model.
//!
//! The document model itself (`PeppyConfig`, `load_or_create`, the bundled
//! template and its comment-preserving completion) lives in the peppy
//! `daemon-config` crate: only the daemon reads or writes that file. What
//! stays here is the slice consumed beyond the daemon:
//! [`SubscriberBufferConfig`] (pmi builds its subscriber buffer sizes from
//! it), the buffer and grace defaults `runtime.rs` uses for runtime-config
//! serde fallbacks, and the shutdown timing contract
//! ([`EVENT_LOOP_JOIN_BUDGET_SECS`] read by `peppylib-py`, plus
//! [`RUNTIME_FINALIZE_MARGIN_SECS`] which the daemon stacks on top of it when
//! sizing force-kill deadlines).

use serde::{Deserialize, Serialize};

/// Default subscriber channel buffer for the `Standard` QoS tier (number of
/// in-flight messages). Mirrors the historical hardcoded value.
pub const DEFAULT_STANDARD_BUFFER_SIZE: usize = 128;
/// Default subscriber channel buffer for the `HighThroughput` QoS tier (e.g.
/// sensor-data streams). Mirrors the historical hardcoded value.
pub const DEFAULT_HIGH_THROUGHPUT_BUFFER_SIZE: usize = 1024;

/// Default daemon-liveness grace period, in seconds (180 = 3 minutes). A
/// spawned node that sees no daemon heartbeat for this long shuts itself down
/// to avoid lingering as an orphan after an uncatchable daemon death.
pub const DEFAULT_DAEMON_GRACE_SECS: u64 = 180;

/// Default cooperative-shutdown grace period, in seconds. How long the daemon
/// (on a clean ctrl+C / `systemctl stop`) and `peppy node stop` wait for a node
/// to run its cleanup hooks before force-killing its process group. 5s gives a
/// robot node room to park actuators and release hardware before it is killed.
pub const DEFAULT_SHUTDOWN_GRACE_SECS: u64 = 5;

/// Worst-case time a node runtime needs to tear down its asyncio event-loop
/// thread after its shutdown hooks finish, before the OS process can exit. A
/// background task may be executing native code (pycapnp serialization, a pyo3
/// future) that must be joined rather than killed mid-call, so this is a real
/// floor the daemon must allow for. Read by `peppylib-py` to bound the loop-join
/// and by the daemon to size its force-kill deadline above the node's real exit
/// cost. Nodes with no asyncio loop (sync-setup Python, Rust) simply finish well
/// inside it.
pub const EVENT_LOOP_JOIN_BUDGET_SECS: u64 = 5;
/// Slack for interpreter finalize / `Drop` after the loop thread joins, before
/// the OS process actually disappears. Added on top of the grace and join
/// windows when the daemon computes how long to wait before force-killing.
pub const RUNTIME_FINALIZE_MARGIN_SECS: u64 = 2;

/// Per-QoS subscriber channel capacities for local node sessions. They apply
/// under both managed topologies and matter most in peer mode, where no router
/// relay absorbs bursts between a publisher and subscriber.
///
/// `#[serde(default)]` fills any field a partial `subscriber_buffers` block
/// omits from [`SubscriberBufferConfig::default`], so every per-field default
/// flows from the single `Default` impl below rather than parallel
/// `default = "fn"` helpers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SubscriberBufferConfig {
    pub standard_buffer_size: usize,
    pub high_throughput_buffer_size: usize,
}

impl Default for SubscriberBufferConfig {
    fn default() -> Self {
        Self {
            standard_buffer_size: DEFAULT_STANDARD_BUFFER_SIZE,
            high_throughput_buffer_size: DEFAULT_HIGH_THROUGHPUT_BUFFER_SIZE,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscriber_buffer_config_preserves_defaults_and_serialized_fields() {
        let defaults = SubscriberBufferConfig::default();
        assert_eq!(defaults.standard_buffer_size, DEFAULT_STANDARD_BUFFER_SIZE);
        assert_eq!(
            defaults.high_throughput_buffer_size,
            DEFAULT_HIGH_THROUGHPUT_BUFFER_SIZE
        );
        assert_eq!(
            serde_json::to_value(defaults).unwrap(),
            serde_json::json!({
                "standard_buffer_size": DEFAULT_STANDARD_BUFFER_SIZE,
                "high_throughput_buffer_size": DEFAULT_HIGH_THROUGHPUT_BUFFER_SIZE,
            })
        );

        let partial: SubscriberBufferConfig =
            serde_json5::from_str("{ standard_buffer_size: 7 }").unwrap();
        assert_eq!(partial.standard_buffer_size, 7);
        assert_eq!(
            partial.high_throughput_buffer_size,
            DEFAULT_HIGH_THROUGHPUT_BUFFER_SIZE
        );
    }
}
