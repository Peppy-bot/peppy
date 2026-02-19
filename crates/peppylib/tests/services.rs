use peppylib::messaging::{MessengerHandle, ServiceMessenger};
use peppylib::types::Payload;
use pmi::ZenohAdapter;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_messenger_communication() {
    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (host, port) = (instance.host.clone(), instance.port);

    let daemon_node = "test_daemon";
    let instance_id = "test_instance";
    let node_name = "test_node";
    let service_name = "test_service";
    let request_payload = Payload::from_static(b"Hello request");
    let response_payload = Payload::from_static(b"Hello response");

    let server_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("failed to create server handle");
    let client_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("failed to create client handle");

    // Start the service listener
    let mut service = ServiceMessenger::listen(
        &server_handle,
        daemon_node,
        instance_id,
        node_name,
        service_name,
    )
    .await
    .expect("listen should succeed");

    // Allow listener to propagate
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Spawn the handler so we can poll concurrently
    let response_clone = response_payload.clone();
    let handler = tokio::spawn(async move {
        service
            .handle_next_request(|_request| async move { Ok(response_clone) })
            .await
            .expect("handle_next_request should succeed");
    });

    // Poll the service as a client
    let response = ServiceMessenger::poll(
        &client_handle,
        daemon_node,
        instance_id,
        node_name,
        service_name,
        Some(daemon_node),
        Some(instance_id),
        request_payload,
        Duration::from_secs(2),
    )
    .await
    .expect("poll should succeed");

    handler.await.expect("handler task should not panic");

    assert_eq!(response.payload(), &response_payload);
    assert_eq!(response.instance_id(), instance_id);
    assert_eq!(response.daemon_node(), daemon_node);
}
