mod common;

use common::start_core_node_with_mock_messenger;
use core_node_api::encoding::HealthRequest;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_health_reports_healthy() {
    let started = start_core_node_with_mock_messenger().await;

    let health = common::poll_core_node(&started, &HealthRequest::new()).await;

    assert_eq!(health.status, "healthy", "status should be healthy");
    // The node was just started, so its uptime should be a small, real value
    // rather than garbage decoded from the wire.
    assert!(
        health.uptime_secs < 60,
        "freshly started node should report a small uptime, got {}",
        health.uptime_secs
    );
}
