//! Shared helpers for the `tests/*.rs` integration tests.
//!
//! Each `tests/*.rs` file is its own test binary, so this module is compiled
//! into each one separately; `#[allow(dead_code)]` silences warnings for
//! helpers a given binary doesn't use.

#![cfg(feature = "build_zenoh")]
#![allow(dead_code)]

use pmi::SenderTarget;
use std::time::Duration;
use tokio::sync::Mutex;

/// Each integration test spawns a zenohd process. Their transient handshakes
/// step on each other when run in parallel, so each test takes this mutex
/// before starting its router. Within-binary serialization only — cargo test
/// already runs different test binaries sequentially.
pub static ZENOH_SERIAL: Mutex<()> = Mutex::const_new(());

pub const RECV_TIMEOUT: Duration = Duration::from_secs(5);

pub fn test_node_target(name: &str) -> SenderTarget {
    SenderTarget::node(name, "v1").expect("test node target")
}

/// Sleeps long enough for zenoh's subscriber discovery to propagate before
/// publishing. The value is empirical — shorter sleeps surface as missed
/// first messages in CI.
pub async fn wait_for_subscriber_discovery() {
    tokio::time::sleep(Duration::from_millis(500)).await;
}
