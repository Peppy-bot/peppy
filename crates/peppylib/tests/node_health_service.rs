mod common;

use common::{
    CALLER_INSTANCE_ID, TEST_CORE_NODE_NAME, TEST_INSTANCE_ID, TEST_NODE_NAME, get_client_server,
};
use peppylib::messaging::Iface;
use peppylib::{
    encoding::health::{NodeHealthRequest, NodeHealthResponse},
    messaging::{MessengerHandle, ServiceMessenger},
    services::health::listen_for_node_health,
};
use std::sync::Arc;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_health_request_response_roundtrip() {
    let (client, shared_messenger) = get_client_server().await;

    // Set up the health service on the server side
    let server_handle = MessengerHandle::from_shared(Arc::clone(&shared_messenger));

    let _health_task = listen_for_node_health(
        &server_handle,
        TEST_CORE_NODE_NAME,
        TEST_INSTANCE_ID,
        TEST_NODE_NAME,
    )
    .await
    .expect("failed to start health service");

    // Allow the service to fully establish its listeners
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Build and encode the health request
    let request = NodeHealthRequest::new();
    let request_payload = request.encode().expect("failed to encode health request");

    // Client sends a health request and receives the response
    let response = ServiceMessenger::poll(
        &client.caller_handle,
        &client.core_node_name,
        CALLER_INSTANCE_ID,
        TEST_NODE_NAME,
        Iface::native(),
        peppylib::messaging::NODE_HEALTH_SERVICE,
        Some(&client.core_node_name),
        Some(&client.instance_id),
        request_payload,
        Duration::from_secs(2),
    )
    .await
    .expect("caller should receive response");

    // Decode and verify the response
    let _health_response =
        NodeHealthResponse::decode(&response.payload()).expect("should decode health response");

    assert_eq!(response.instance_id(), client.instance_id);
}
