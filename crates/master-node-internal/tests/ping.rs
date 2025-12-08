//! Integration tests for the ping command.
//!
//! This test verifies that the ping service correctly receives requests and sends responses
//! using the messaging system with Cap'n Proto encoding/decoding.

use master_node::MasterNode;
use master_node::encoding::{PingRequest, PingResponse};
use peppylib::messaging::{MessengerHandle, ServiceMessenger};
use pmi::{Messenger, MessengerAdapter, MessengerBackend, MockAdapter};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

/// Creates a mock messenger with an active session.
async fn create_mock_messenger() -> Arc<Mutex<Messenger>> {
    let adapter = MockAdapter::default();
    let mut messenger = Messenger::new(MessengerAdapter::Mock(adapter));
    messenger
        .start_session()
        .await
        .expect("failed to start mock session");
    Arc::new(Mutex::new(messenger))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ping_request_response_roundtrip() {
    // Create a shared mock messenger for both MasterNode and client
    let shared_messenger = create_mock_messenger().await;

    // Create and start the MasterNode (listener)
    let master_node = MasterNode::new(Arc::clone(&shared_messenger), Some("test_master_node"));
    let master_node_name = master_node.node_name().to_string();
    let instance_id = master_node.instance_id().to_string();

    // Spawn the MasterNode in a background task
    let master_node_task = tokio::spawn(async move { master_node.start().await });

    // Allow the MasterNode services to fully establish their listeners
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Client (caller) configuration
    const CALLER_INSTANCE_ID: &str = "caller_instance";

    // Use current time in milliseconds as the request timestamp
    let request_timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_millis() as u64;

    // Build and encode the ping request
    let request = PingRequest::new(request_timestamp_ms);
    let request_payload = request.encode().expect("failed to encode ping request");

    // Create a client messenger handle from the same shared messenger
    let caller_handle = MessengerHandle::from_shared(Arc::clone(&shared_messenger));

    // Client sends a ping request and receives the response
    let response = ServiceMessenger::poll(
        &caller_handle,
        &master_node_name,
        CALLER_INSTANCE_ID,
        &master_node_name,
        "ping",
        None,
        Some(&instance_id),
        request_payload,
        Duration::from_secs(2),
    )
    .await
    .expect("caller should receive response");

    // Decode and verify the response
    let ping_response =
        PingResponse::decode(&response.payload().to_bytes()).expect("should decode ping response");

    assert_eq!(ping_response.message, "pong");
    assert_eq!(response.instance_id(), instance_id);

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

    // Cancel the MasterNode task (it runs forever otherwise)
    master_node_task.abort();
}
