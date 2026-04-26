mod common;

use common::{assert_clock_topic_emits_monotonic_ticks, start_core_node_with_mock_messenger};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publish_clock_emits_monotonic_ticks() {
    let started = start_core_node_with_mock_messenger().await;
    // Mock matcher is unidirectional; subscriber must mirror the publisher's
    // hard-coded `*` literals in the caller-identity slots of the wire key.
    assert_clock_topic_emits_monotonic_ticks(&started, "*", "*", Duration::from_secs(2)).await;
}
