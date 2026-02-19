mod common;

use common::{CALLER_INSTANCE_ID, start_daemon_node_with_mock_messenger};
use daemon_node::encoding::{PingRequest, PingResponse};
use daemon_node::names;
use peppylib::ServiceMessenger;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_ping_roundtrip_succeed() {
    let started = start_daemon_node_with_mock_messenger().await;

    // Send a ping request to the daemon node
    let timestamp = 12345u64;
    let ping_request = PingRequest::new(timestamp);
    let request_payload = ping_request.encode().expect("encode should succeed");

    let response = ServiceMessenger::poll(
        &started.caller_handle,
        &started.daemon_node_name,
        CALLER_INSTANCE_ID,
        &started.daemon_node_name,
        names::PING,
        Some(&started.daemon_node_name),
        None,
        request_payload,
        Duration::from_secs(5),
    )
    .await
    .expect("ping request should succeed");

    let ping_response = PingResponse::decode(&response.payload()).expect("decode should succeed");

    // Verify the response echoes back the timestamp
    assert_eq!(
        ping_response.timestamp, timestamp,
        "timestamp should match the request"
    );
    assert!(
        !ping_response.message.is_empty(),
        "message should not be empty"
    );
}
