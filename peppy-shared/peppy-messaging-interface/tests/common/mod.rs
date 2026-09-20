//! Shared helpers for the `tests/*.rs` integration tests.
//!
//! Each `tests/*.rs` file is its own test binary, so this module is compiled
//! into each one separately; `#[allow(dead_code)]` silences warnings for
//! helpers a given binary doesn't use.

#![cfg(feature = "build_zenoh")]
#![allow(dead_code)]

use pmi::{SenderTarget, TopicWireReceiver, TopicWireSender};
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

/// A wire sender publishing as `as_topic_name` from a fixed test node identity.
/// Shared by the plaintext and TLS round-trip tests so the wire-field shape lives
/// in one place.
pub fn sender(as_topic_name: &str) -> TopicWireSender {
    TopicWireSender::new(
        "test_core_node",
        "test_instance",
        test_node_target("test_node"),
        None,
        as_topic_name,
    )
    .expect("valid wire fields")
}

/// A wire receiver subscribing to `to_topic` from the same fixed test node
/// identity as [`sender`].
pub fn receiver(to_topic: &str) -> TopicWireReceiver {
    TopicWireReceiver::new(
        "test_core_node",
        "test_instance",
        None,
        None,
        Some(test_node_target("test_node")),
        None,
        to_topic,
    )
    .expect("valid wire fields")
}

/// Sleeps long enough for zenoh's subscriber discovery to propagate before
/// publishing. The value is empirical — shorter sleeps surface as missed
/// first messages in CI.
pub async fn wait_for_subscriber_discovery() {
    tokio::time::sleep(Duration::from_millis(500)).await;
}
