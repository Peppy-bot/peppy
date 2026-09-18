mod common;

use common::{
    CALLER_INSTANCE_ID, TEST_CORE_NODE_NAME, TEST_INSTANCE_ID, TEST_NODE_NAME, get_client_server,
    test_node_target,
};
use peppylib::types::Payload;
use peppylib::{
    messaging::{MessengerHandle, ProducerRef, SHUTDOWN_SERVICE, ServiceMessenger, ServiceTarget},
    services::shutdown::listen_for_shutdown,
};
use std::sync::Arc;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_node() {
    let (client, shared_messenger) = get_client_server().await;

    // Set up the shutdown service on the server side
    let server_handle = MessengerHandle::from_shared(Arc::clone(&shared_messenger));

    let (shutdown_task, shutdown_rx) = listen_for_shutdown(
        &server_handle,
        TEST_CORE_NODE_NAME,
        TEST_INSTANCE_ID,
        test_node_target(TEST_NODE_NAME),
    )
    .await
    .expect("failed to start shutdown service");

    // Allow the service to fully establish its listeners
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Send a shutdown request (payload can be empty or contain shutdown info)
    let request_payload = Payload::from_static(b"shutdown");

    // Client sends a shutdown request and receives the response
    let response = ServiceMessenger::poll(
        &client.caller_handle,
        &client.core_node_name,
        CALLER_INSTANCE_ID,
        test_node_target(TEST_NODE_NAME),
        SHUTDOWN_SERVICE,
        ServiceTarget::Producer(&ProducerRef::new(
            client.core_node_name.as_str(),
            client.instance_id.as_str(),
        )),
        request_payload.clone(),
        Duration::from_secs(2),
    )
    .await
    .expect("caller should receive response");

    // Verify the response contains the same payload (echoed back)
    assert_eq!(response.payload(), request_payload);
    assert_eq!(response.instance_id(), client.instance_id);

    // Verify the shutdown signal was sent
    // The receiver should complete immediately since the signal was already sent
    tokio::time::timeout(Duration::from_millis(100), shutdown_rx)
        .await
        .expect("shutdown signal should be received within timeout")
        .expect("shutdown channel should not be dropped");

    // The shutdown task should still be running (it handles multiple requests)
    assert!(!shutdown_task.is_finished());
}
