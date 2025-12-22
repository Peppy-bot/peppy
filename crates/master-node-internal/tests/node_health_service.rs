mod common;

use common::{CALLER_INSTANCE_ID, setup_test_master_node};
use master_node::encoding::{NodeHealthRequest, NodeHealthResponse};
use peppylib::messaging::ServiceMessenger;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_node_health_request_response_roundtrip() {
    let (client, _server) = setup_test_master_node().await;

    // Build and encode the health request
    let request = NodeHealthRequest::new();
    let request_payload = request.encode().expect("failed to encode health request");

    // Client sends a health request and receives the response
    let response = ServiceMessenger::poll(
        &client.caller_handle,
        &client.master_node_name,
        CALLER_INSTANCE_ID,
        &client.master_node_name,
        "node_health",
        None,
        Some(&client.instance_id),
        request_payload,
        Duration::from_secs(2),
    )
    .await
    .expect("caller should receive response");

    // Decode and verify the response
    let _health_response = NodeHealthResponse::decode(&response.payload().to_bytes())
        .expect("should decode health response");

    assert_eq!(response.instance_id(), client.instance_id);
}
