use config::node::QoSProfile;
use peppylib::messaging::{MessengerHandle, TopicMessenger};
use peppylib::types::Payload;
use pmi::ZenohAdapter;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn topic_messenger_communication() {
    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (host, port) = (instance.host.clone(), instance.port);

    let core_node = "test_core";
    let instance_id = "test_instance";
    let node_name = "test_node";
    let topic_name = "test_topic";
    let payload = Payload::from_static(b"Hello world");

    let receiver_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("failed to create receiver handle");
    let sender_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("failed to create sender handle");

    // Subscribe to the topic first
    let mut subscription = TopicMessenger::subscribe(
        &receiver_handle,
        core_node,
        instance_id,
        node_name,
        topic_name,
        None, // Accept messages from any core node
        None, // Accept messages from any instance
        None,
        QoSProfile::Reliable,
    )
    .await
    .expect("subscription should succeed");

    // Allow subscription to propagate
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Emit a message
    TopicMessenger::emit(
        &sender_handle,
        core_node,
        instance_id,
        config::runtime::DEFAULT_VARIANT,
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

    assert_eq!(message.payload(), &payload);
    assert_eq!(message.instance_id(), instance_id);
    assert_eq!(message.core_node(), core_node);
}
