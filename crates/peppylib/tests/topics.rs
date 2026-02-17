use bytes::Bytes;
use config::node::QoSProfile;
use peppylib::messaging::{MessengerHandle, TopicMessenger};
use pmi::ZenohAdapter;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn topic_messenger_communication() {
    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (host, port) = (instance.host.clone(), instance.port);

    let daemon_node = "test_daemon";
    let instance_id = "test_instance";
    let node_name = "test_node";
    let topic_name = "test_topic";
    let payload = Bytes::from_static(b"Hello world");

    let receiver_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("failed to create receiver handle");
    let sender_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("failed to create sender handle");

    // Subscribe to the topic first
    let mut subscription = TopicMessenger::subscribe(
        &receiver_handle,
        daemon_node,
        instance_id,
        node_name,
        topic_name,
        None, // Accept messages from any daemon node
        None, // Accept messages from any instance
        QoSProfile::Reliable,
    )
    .await
    .expect("subscription should succeed");

    // Allow subscription to propagate
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Emit a message
    TopicMessenger::emit(
        &sender_handle,
        daemon_node,
        instance_id,
        node_name,
        topic_name,
        QoSProfile::Reliable,
        payload.clone(),
    )
    .await
    .expect("emit should succeed");

    // Receive the message with a timeout
    let message = tokio::time::timeout(Duration::from_secs(2), subscription.on_next_message())
        .await
        .expect("should receive message within timeout")
        .expect("message should not be None");

    assert_eq!(message.payload().to_bytes(), payload);
    assert_eq!(message.instance_id(), instance_id);
    assert_eq!(message.daemon_node(), daemon_node);
}
