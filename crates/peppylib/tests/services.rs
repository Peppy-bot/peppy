use peppylib::messaging::SenderTarget;
use peppylib::messaging::{MessengerHandle, ServiceMessenger};
use peppylib::types::Payload;
use pmi::ZenohAdapter;
use std::time::Duration;
use tokio::sync::oneshot;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_messenger_communication() {
    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (host, port) = (instance.host.clone(), instance.port);

    let core_node = "test_core";
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
        core_node,
        instance_id,
        SenderTarget::node(node_name, "v1").expect("test target"),
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
        core_node,
        instance_id,
        SenderTarget::node(node_name, "v1").expect("test target"),
        service_name,
        Some(core_node),
        Some(instance_id),
        request_payload,
        Duration::from_secs(2),
    )
    .await
    .expect("poll should succeed");

    handler.await.expect("handler task should not panic");

    assert_eq!(response.payload(), &response_payload);
    assert_eq!(response.instance_id(), instance_id);
    assert_eq!(response.core_node(), core_node);
}

/// A single node exposes the *same* service name under two distinct iface
/// scopes (native + a conformed interface). The wire-path scoping must keep
/// them independently addressable: a caller targeting one scope must never see
/// responses from the other.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_iface_scoped_native_and_conformed_do_not_collide() {
    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (host, port) = (instance.host.clone(), instance.port);

    let core_node = "test_core";
    let instance_id = "test_instance";
    let node_name = "test_node";
    let service_name = "control";
    let iface_name = "camera";
    let iface_tag = "v1";

    let native_response = Payload::from_static(b"from_native");
    let iface_response = Payload::from_static(b"from_iface");

    let native_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("failed to create native handle");
    let iface_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("failed to create iface handle");
    let caller_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("failed to create caller handle");

    let (native_ready_tx, native_ready_rx) = oneshot::channel();
    let mut native_endpoint = ServiceMessenger::listen(
        &native_handle,
        core_node,
        instance_id,
        SenderTarget::node(node_name, "v1").expect("test target"),
        service_name,
    )
    .await
    .expect("native listen should succeed");

    let native_response_clone = native_response.clone();
    let native_handler = tokio::spawn(async move {
        native_ready_tx.send(()).unwrap();
        native_endpoint
            .handle_next_request(|_req| async move { Ok(native_response_clone) })
            .await
            .expect("native handler should succeed");
    });
    native_ready_rx.await.unwrap();

    let (iface_ready_tx, iface_ready_rx) = oneshot::channel();
    let mut iface_endpoint = ServiceMessenger::listen(
        &iface_handle,
        core_node,
        instance_id,
        SenderTarget::interface(iface_name, iface_tag).expect("test target"),
        service_name,
    )
    .await
    .expect("iface listen should succeed");

    let iface_response_clone = iface_response.clone();
    let iface_handler = tokio::spawn(async move {
        iface_ready_tx.send(()).unwrap();
        iface_endpoint
            .handle_next_request(|_req| async move { Ok(iface_response_clone) })
            .await
            .expect("iface handler should succeed");
    });
    iface_ready_rx.await.unwrap();

    // Allow both subscriptions to propagate.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Poll the native scope and assert we get the native response.
    let from_native = ServiceMessenger::poll(
        &caller_handle,
        core_node,
        instance_id,
        SenderTarget::node(node_name, "v1").expect("test target"),
        service_name,
        Some(core_node),
        Some(instance_id),
        Payload::from_static(b"ping_native"),
        Duration::from_secs(2),
    )
    .await
    .expect("native poll should succeed");
    assert_eq!(
        from_native.payload(),
        &native_response,
        "native scope must receive the native handler's response"
    );

    // Poll the iface scope and assert we get the iface response.
    let from_iface = ServiceMessenger::poll(
        &caller_handle,
        core_node,
        instance_id,
        SenderTarget::interface(iface_name, iface_tag).expect("test target"),
        service_name,
        Some(core_node),
        Some(instance_id),
        Payload::from_static(b"ping_iface"),
        Duration::from_secs(2),
    )
    .await
    .expect("iface poll should succeed");
    assert_eq!(
        from_iface.payload(),
        &iface_response,
        "iface scope must receive the iface handler's response"
    );

    native_handler.await.expect("native handler task panicked");
    iface_handler.await.expect("iface handler task panicked");
}

/// Hyphens in `iface_tag` must be normalized to underscores at the wire-format
/// boundary, so a caller that passes `"v2-stable"` and a listener that passes
/// `"v2_stable"` end up on the same wire path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_iface_tag_hyphen_normalized() {
    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (host, port) = (instance.host.clone(), instance.port);

    let core_node = "test_core";
    let instance_id = "test_instance";
    let node_name = "test_node";
    let service_name = "control";
    let iface_name = "camera";

    let response_payload = Payload::from_static(b"ack");

    let server_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("failed to create server handle");
    let client_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("failed to create client handle");

    // Listener uses hyphen.
    let mut endpoint = ServiceMessenger::listen(
        &server_handle,
        core_node,
        instance_id,
        SenderTarget::interface(iface_name, "v2-stable").expect("test target"),
        service_name,
    )
    .await
    .expect("listen should succeed");

    let response_clone = response_payload.clone();
    let handler = tokio::spawn(async move {
        endpoint
            .handle_next_request(|_req| async move { Ok(response_clone) })
            .await
            .expect("handler should succeed");
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Caller uses underscore. Both should normalize to the same wire segment.
    let response = ServiceMessenger::poll(
        &client_handle,
        core_node,
        instance_id,
        SenderTarget::interface(iface_name, "v2_stable").expect("test target"),
        service_name,
        Some(core_node),
        Some(instance_id),
        Payload::from_static(b"ping"),
        Duration::from_secs(2),
    )
    .await
    .expect("poll should succeed");

    handler.await.expect("handler task panicked");
    assert_eq!(response.payload(), &response_payload);
}
