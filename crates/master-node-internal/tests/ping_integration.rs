//! Integration tests for the ping command.
//!
//! This test verifies that the ping service correctly receives requests and sends responses
//! using the messaging system with Cap'n Proto encoding/decoding.

use bytes::Bytes;
use capnp::message::Builder;
use master_node::MasterNode;
use master_node::encoding::{decode_message, encode_message};
use master_node::messages_capnp;
use peppylib::messaging::{MessengerHandle, ServiceMessenger};
use pmi::{Messenger, MessengerAdapter, MessengerBackend, MockAdapter};
use std::sync::Arc;
use std::time::Duration;
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

/// Builds a ping request with the given timestamp and encodes it to bytes.
fn build_ping_request(timestamp: u64) -> Bytes {
    let mut builder = Builder::new_default();
    {
        let mut request = builder.init_root::<messages_capnp::ping_request::Builder>();
        request.set_timestamp(timestamp);
    }
    encode_message(&builder).expect("failed to encode ping request")
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

    let test_timestamp: u64 = 1234567890;

    // Build and encode the ping request
    let request_payload = build_ping_request(test_timestamp);

    // Verify request encoding by decoding it back
    {
        let reader = decode_message(&request_payload).expect("should decode request");
        let request = reader
            .get_root::<messages_capnp::ping_request::Reader>()
            .expect("should get ping request");
        assert_eq!(request.get_timestamp(), test_timestamp);
    }

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
    let response_bytes = response.payload().to_bytes();
    let reader = decode_message(&response_bytes).expect("should decode response");
    let ping_response = reader
        .get_root::<messages_capnp::ping_response::Reader>()
        .expect("should get ping response");

    assert_eq!(ping_response.get_timestamp(), test_timestamp);
    assert_eq!(ping_response.get_message().unwrap(), "pong");
    assert_eq!(response.instance_id(), instance_id);

    // Cancel the MasterNode task (it runs forever otherwise)
    master_node_task.abort();
}
