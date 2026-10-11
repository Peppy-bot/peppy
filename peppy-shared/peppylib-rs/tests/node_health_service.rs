mod common;

use common::{
    TEST_CORE_NODE_NAME, TEST_INSTANCE_ID, TEST_NODE_NAME, get_client_server, test_node_target,
};
use peppylib::{
    encoding::health::{NodeHealthRequest, NodeHealthResponse},
    messaging::{MessengerHandle, NODE_HEALTH_SERVICE, ServiceTarget},
    services::health::listen_for_node_health,
};
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_health_request_response_roundtrip() {
    let (client, shared_messenger) = get_client_server().await;

    let server_handle = MessengerHandle::from_shared(Arc::clone(&shared_messenger));

    let _health_task = listen_for_node_health(
        &server_handle,
        TEST_CORE_NODE_NAME,
        TEST_INSTANCE_ID,
        test_node_target(TEST_NODE_NAME),
    )
    .await
    .expect("failed to start health service");

    let request = NodeHealthRequest::new();
    let request_payload = request.encode().expect("failed to encode health request");

    let response = client
        .poll_test_node(
            NODE_HEALTH_SERVICE,
            ServiceTarget::Producer(&client.producer()),
            request_payload,
        )
        .await
        .expect("caller should receive response");

    // The reply decodes as a health response.
    let _health_response =
        NodeHealthResponse::decode(&response.payload()).expect("should decode health response");

    assert_eq!(response.instance_id(), client.instance_id);
}
