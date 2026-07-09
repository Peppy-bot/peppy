mod common;

use common::{CALLER_INSTANCE_ID, start_core_node_with_mock_messenger};
use core_node_api::ServiceId;
use core_node_api::encoding::{HealthRequest, HealthResponse};
use peppylib::ServiceMessenger;
use peppylib::messaging::ServiceTarget;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_health_reports_healthy() {
    let started = start_core_node_with_mock_messenger().await;

    let request_payload = HealthRequest::new()
        .encode()
        .expect("encode should succeed");

    let response = ServiceMessenger::poll(
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        common::core_node_target(&started.core_node_name),
        ServiceId::Health.name(),
        ServiceTarget::Any,
        request_payload,
        Duration::from_secs(5),
    )
    .await
    .expect("health request should succeed");

    let health = HealthResponse::decode(&response.payload()).expect("decode should succeed");

    assert_eq!(health.status, "healthy", "status should be healthy");
    // The node was just started, so its uptime should be a small, real value
    // rather than garbage decoded from the wire.
    assert!(
        health.uptime_secs < 60,
        "freshly started node should report a small uptime, got {}",
        health.uptime_secs
    );
}
