mod common;

use common::test_node_target;
use config::node::QoSProfile;
use peppylib::messaging::{ConsumerFilter, MessengerHandle, TopicMessenger};
use peppylib::types::Payload;
use pmi::{MessengerBackend, ZenohAdapter};
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
        Some(test_node_target(node_name)),
        true, // from_any pattern
        topic_name,
        None, // Accept messages from any core node
        &ConsumerFilter::Any,
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
        test_node_target(node_name),
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

/// Proves a NODE session keeps receiving after the router process is killed and
/// respawned on the same port. The subscriber is created via the node-path
/// `from_host_port_reconnecting`, so this exercises the actual reconnecting
/// config that `NodeRunner` gives every node — confirming a watchdog
/// router-respawn doesn't knock running nodes off the bus.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn node_session_recovers_after_router_restart() {
    let mut instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (host, port) = (instance.host.clone(), instance.port);

    let core_node = "test_core";
    let instance_id = "test_instance";
    let node_name = "test_node";
    let topic_name = "reconnect_topic";

    // Subscriber uses the NODE path: a reconnecting session.
    let receiver_handle = MessengerHandle::from_host_port_reconnecting(&host, port)
        .await
        .expect("failed to create reconnecting receiver handle");
    let mut subscription = TopicMessenger::subscribe(
        &receiver_handle,
        core_node,
        instance_id,
        Some(test_node_target(node_name)),
        true,
        topic_name,
        None,
        &ConsumerFilter::Any,
        QoSProfile::Reliable,
    )
    .await
    .expect("subscription should succeed");

    // Baseline: a publisher reaches the subscriber through the router.
    {
        let sender_handle = MessengerHandle::from_host_port(&host, port)
            .await
            .expect("failed to create sender handle");
        tokio::time::sleep(Duration::from_millis(500)).await;
        TopicMessenger::emit(
            &sender_handle,
            core_node,
            instance_id,
            test_node_target(node_name),
            topic_name,
            QoSProfile::Reliable,
            Payload::from_static(b"before-restart"),
        )
        .await
        .expect("baseline emit should succeed");
        let msg = tokio::time::timeout(Duration::from_secs(5), subscription.on_next_message())
            .await
            .expect("baseline: should receive within timeout")
            .expect("baseline: message should not be None");
        assert_eq!(msg.payload(), &Payload::from_static(b"before-restart"));
    }

    // Kill + respawn zenohd on the same port — exactly what the watchdog does.
    instance
        .messenger()
        .stop_router()
        .await
        .expect("stop_router");
    instance
        .messenger()
        .start_router()
        .await
        .expect("start_router");

    // The reconnecting node session must re-establish and re-declare its
    // subscription. Drive a fresh publisher (on the new router) and poll until
    // delivery, or give up after a generous budget.
    // A non-reconnecting `from_host_port` would race the respawn: the freshly
    // started router can accept a TCP connection before its protocol handshake
    // has settled, failing the one-shot session open. A reconnecting publisher
    // opens immediately and connects in the background instead, so the
    // emit-until-delivered loop below drives recovery without a hand-rolled
    // connect-retry loop here.
    let sender_handle = MessengerHandle::from_host_port_reconnecting(&host, port)
        .await
        .expect("failed to create post-restart sender handle");

    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut recovered = false;
    while std::time::Instant::now() < deadline {
        // Ignore emit errors: the publisher link may still be settling.
        let _ = TopicMessenger::emit(
            &sender_handle,
            core_node,
            instance_id,
            test_node_target(node_name),
            topic_name,
            QoSProfile::Reliable,
            Payload::from_static(b"after-restart"),
        )
        .await;
        // Only the post-restart payload proves recovery: a stale `before-restart`
        // delivery redelivered through the reconnecting session must not count.
        if let Ok(Some(msg)) =
            tokio::time::timeout(Duration::from_millis(800), subscription.on_next_message()).await
            && msg.payload() == Payload::from_static(b"after-restart")
        {
            recovered = true;
            break;
        }
    }

    assert!(
        recovered,
        "node session did not receive after the router was respawned: it failed to reconnect + \
         re-declare its subscription"
    );
}
