mod common;

use common::{assert_clock_round_trip, start_core_node_with_mock_messenger};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_clock_roundtrip_succeed() {
    let started = start_core_node_with_mock_messenger().await;
    assert_clock_round_trip(&started).await;
}
