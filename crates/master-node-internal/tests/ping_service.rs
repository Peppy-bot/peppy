mod common;

use common::{CALLER_INSTANCE_ID, setup_test_master_node};
use master_node::encoding::{PingRequest, PingResponse};
use peppylib::messaging::ServiceMessenger;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ping_request_response_roundtrip() {
    let test_node = setup_test_master_node().await;

    // Use current time in milliseconds as the request timestamp
    let request_timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_millis() as u64;

    // Build and encode the ping request
    let request = PingRequest::new(request_timestamp_ms);
    let request_payload = request.encode().expect("failed to encode ping request");

    // Client sends a ping request and receives the response
    let response = ServiceMessenger::poll(
        &test_node.caller_handle,
        &test_node.master_node_name,
        CALLER_INSTANCE_ID,
        &test_node.master_node_name,
        "ping",
        None,
        Some(&test_node.instance_id),
        request_payload,
        Duration::from_secs(2),
    )
    .await
    .expect("caller should receive response");

    // Decode and verify the response
    let ping_response =
        PingResponse::decode(&response.payload().to_bytes()).expect("should decode ping response");

    assert_eq!(ping_response.message, "pong");
    assert_eq!(response.instance_id(), test_node.instance_id);

    // Calculate latency from timestamps
    let response_timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_millis() as u64;
    let latency_ms = response_timestamp_ms - ping_response.timestamp;

    // Verify the response echoed back our request timestamp
    assert_eq!(ping_response.timestamp, request_timestamp_ms);

    // Verify latency is reasonable (should be fast with mock messenger)
    assert!(latency_ms < 500, "latency too high: {latency_ms}ms");
}
