//! Shared helpers for the `tests/*.rs` integration tests.
//!
//! Each `tests/*.rs` file is its own test binary, so this module is compiled
//! into each one separately; `#[allow(dead_code)]` silences warnings for
//! helpers a given binary doesn't use.

#![cfg(feature = "build_zenoh")]
#![allow(dead_code)]

use pmi::{
    IncomingRequest, MessengerBackend, SenderTarget, ServiceQueryable, Subscription, TopicMessage,
    TopicWireReceiver, TopicWireSender, ZenohAdapter,
};
use std::time::Duration;
use tokio::sync::Mutex;

/// Each integration test spawns a zenohd process. Their transient handshakes
/// step on each other when run in parallel, so each test takes this mutex
/// before starting its router. Within-binary serialization only — cargo test
/// already runs different test binaries sequentially.
pub static ZENOH_SERIAL: Mutex<()> = Mutex::const_new(());

pub const RECV_TIMEOUT: Duration = Duration::from_secs(5);

/// The next message `subscription` reads, within `bound`. Panics, naming
/// `label`, when the bound elapses first or the subscription closes.
pub async fn next_message_within(
    bound: Duration,
    subscription: &Subscription,
    label: &str,
) -> TopicMessage {
    tokio::time::timeout(bound, subscription.rx.recv_async())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {label}"))
        .unwrap_or_else(|_| panic!("the subscription closed before {label}"))
}

/// The next message `subscription` reads, within [`RECV_TIMEOUT`].
pub async fn next_message(subscription: &Subscription, label: &str) -> TopicMessage {
    next_message_within(RECV_TIMEOUT, subscription, label).await
}

/// The next request `queryable` receives, within [`RECV_TIMEOUT`]. Panics,
/// naming `label`, when the bound elapses first or the queryable closes.
pub async fn next_request(queryable: &ServiceQueryable, label: &str) -> IncomingRequest {
    tokio::time::timeout(RECV_TIMEOUT, queryable.rx.recv_async())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {label}"))
        .unwrap_or_else(|_| panic!("the queryable closed before {label}"))
}

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

/// How many times [`start_session_with_retry`] tries to start a session, and
/// how long it waits between two tries: ten seconds in all.
const SESSION_START_ATTEMPTS: usize = 40;
const SESSION_START_RETRY_INTERVAL: Duration = Duration::from_millis(250);

/// Starts the session of an adapter that `build` makes, retrying while a
/// router that just started or restarted settles. Panics with
/// "could not open {label}" when no attempt succeeds.
pub async fn start_session_with_retry(
    build: impl Fn() -> ZenohAdapter,
    label: &str,
) -> ZenohAdapter {
    for _ in 0..SESSION_START_ATTEMPTS {
        let mut adapter = build();
        if adapter.start_session().await.is_ok() {
            return adapter;
        }
        tokio::time::sleep(SESSION_START_RETRY_INTERVAL).await;
    }
    panic!("could not open {label}");
}

/// Sleeps long enough for zenoh's subscriber discovery to propagate before
/// publishing. The value is empirical — shorter sleeps surface as missed
/// first messages in CI.
pub async fn wait_for_subscriber_discovery() {
    tokio::time::sleep(Duration::from_millis(500)).await;
}
