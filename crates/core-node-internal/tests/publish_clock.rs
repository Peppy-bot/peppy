mod common;

use common::{
    CALLER_INSTANCE_ID, assert_clock_topic_emits_monotonic_ticks,
    start_core_node_with_mock_messenger,
};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publish_clock_emits_monotonic_ticks() {
    let started = start_core_node_with_mock_messenger().await;
    assert_clock_topic_emits_monotonic_ticks(
        &started,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        Duration::from_secs(2),
    )
    .await;
}
