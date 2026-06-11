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

/// Bidirectional `from_any` at the wire layer: two consumers each subscribe
/// to the other's topic as a `from_any` wildcard (`is_from_any = true`,
/// `ConsumerFilter::Any`), exactly as a generated `from_any` interface
/// consumed-topic module does. Messages flow independently in both
/// directions with no binding wiring the pair together, and a producer that
/// joins *after* the consumer is already listening is picked up through the
/// same subscription, distinguished only by its `instance_id`. This is the
/// runtime counterpart to the launch-time `FromAnyUnbound` materialization
/// checked in `crates/peppy/tests/stack_launch.rs`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bidirectional_from_any_topics_with_late_producer() {
    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (host, port) = (instance.host.clone(), instance.port);

    let core_node = "test_core";
    // One topic per direction, mirroring the docs' robot arm control loop.
    let joint_states = "joint_states"; // emitted by robot_arm, consumed by arm_controller
    let joint_commands = "joint_commands"; // emitted by arm_controller, consumed by robot_arm

    let controller_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("failed to create arm_controller handle");
    let arm_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("failed to create robot_arm handle");

    // arm_controller consumes joint_states from any robot_arm instance.
    let mut controller_sub = TopicMessenger::subscribe(
        &controller_handle,
        core_node,
        "ctrl_1",
        Some(test_node_target("robot_arm")),
        true, // from_any
        joint_states,
        None, // from any core node
        &ConsumerFilter::Any,
        QoSProfile::Reliable,
    )
    .await
    .expect("arm_controller subscription should succeed");

    // robot_arm consumes joint_commands from any arm_controller instance.
    let mut arm_sub = TopicMessenger::subscribe(
        &arm_handle,
        core_node,
        "arm_1",
        Some(test_node_target("arm_controller")),
        true, // from_any
        joint_commands,
        None,
        &ConsumerFilter::Any,
        QoSProfile::Reliable,
    )
    .await
    .expect("robot_arm subscription should succeed");

    // Allow both subscriptions to propagate.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Direction 1: robot_arm (arm_1) -> arm_controller.
    let state_payload = Payload::from_static(b"joint_states@arm_1");
    TopicMessenger::emit(
        &arm_handle,
        core_node,
        "arm_1",
        test_node_target("robot_arm"),
        joint_states,
        QoSProfile::Reliable,
        state_payload.clone(),
    )
    .await
    .expect("robot_arm emit should succeed");

    let msg = tokio::time::timeout(Duration::from_secs(2), controller_sub.on_next_message())
        .await
        .expect("arm_controller should receive joint_states within timeout")
        .expect("message should not be None");
    assert_eq!(msg.payload(), &state_payload);
    assert_eq!(msg.instance_id(), "arm_1");
    assert_eq!(msg.core_node(), core_node);

    // Direction 2: arm_controller (ctrl_1) -> robot_arm. The reverse stream
    // flows independently; nothing bound the two nodes to each other.
    let command_payload = Payload::from_static(b"joint_commands@ctrl_1");
    TopicMessenger::emit(
        &controller_handle,
        core_node,
        "ctrl_1",
        test_node_target("arm_controller"),
        joint_commands,
        QoSProfile::Reliable,
        command_payload.clone(),
    )
    .await
    .expect("arm_controller emit should succeed");

    let msg = tokio::time::timeout(Duration::from_secs(2), arm_sub.on_next_message())
        .await
        .expect("robot_arm should receive joint_commands within timeout")
        .expect("message should not be None");
    assert_eq!(msg.payload(), &command_payload);
    assert_eq!(msg.instance_id(), "ctrl_1");

    // A second robot_arm instance joins *after* arm_controller is already
    // subscribed. With no binding to update, the wildcard subscription picks
    // it up automatically; only the returned instance_id distinguishes it
    // from the first producer.
    let late_arm_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("failed to create late robot_arm handle");
    tokio::time::sleep(Duration::from_millis(50)).await;

    let late_payload = Payload::from_static(b"joint_states@arm_2");
    TopicMessenger::emit(
        &late_arm_handle,
        core_node,
        "arm_2",
        test_node_target("robot_arm"),
        joint_states,
        QoSProfile::Reliable,
        late_payload.clone(),
    )
    .await
    .expect("late robot_arm emit should succeed");

    let msg = tokio::time::timeout(Duration::from_secs(2), controller_sub.on_next_message())
        .await
        .expect("arm_controller should receive the late producer within timeout")
        .expect("message should not be None");
    assert_eq!(msg.payload(), &late_payload);
    assert_eq!(
        msg.instance_id(),
        "arm_2",
        "the late producer must be picked up through the same subscription, \
         distinguished only by its instance_id",
    );
}

/// End-to-end typed zero-copy: a Cap'n Proto message is encoded straight into
/// a loaned shared-memory buffer ([`peppylib::encoding::encode_message_to_loan`]),
/// published without further copies, and the subscriber parses it IN PLACE
/// over the borrowed payload view ([`peppylib::encoding::decode_message_in_place`])
/// — the white-box proof that neither side materializes an owned buffer on the
/// access path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_capnp_message_round_trips_zero_copy_through_shared_memory() {
    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (host, port) = (instance.host.clone(), instance.port);

    let core_node = "test_core";
    let instance_id = "test_instance";
    let node_name = "test_node";
    let topic_name = "capnp_shm_topic";

    let receiver_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("receiver handle");
    let sender_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("sender handle");

    let mut subscription = TopicMessenger::subscribe(
        &receiver_handle,
        core_node,
        instance_id,
        Some(test_node_target(node_name)),
        false,
        topic_name,
        None,
        &ConsumerFilter::Any,
        QoSProfile::Reliable,
    )
    .await
    .expect("subscription should succeed");
    tokio::time::sleep(Duration::from_millis(50)).await;

    let publisher = TopicMessenger::declare_publisher(
        &sender_handle,
        core_node,
        instance_id,
        test_node_target(node_name),
        None,
        topic_name,
        QoSProfile::Reliable,
    )
    .await
    .expect("declared publisher");

    // A blob comfortably above the SHM publish threshold, carried as a capnp
    // `Data` root.
    let blob: Vec<u8> = (0..32 * 1024).map(|i| (i % 251) as u8).collect();
    let mut builder = capnp::message::Builder::new_default();
    builder
        .init_root::<capnp::any_pointer::Builder>()
        .set_as::<capnp::data::Owned>(&blob[..])
        .expect("set data root");

    let loan =
        peppylib::encoding::encode_message_to_loan(&publisher, &builder).expect("encode into loan");
    assert!(
        loan.is_shm(),
        "an above-threshold capnp encode should land in shared memory by default"
    );
    publisher.publish_loaned(loan).await.expect("publish loan");

    let message = tokio::time::timeout(Duration::from_secs(2), subscription.on_next_message())
        .await
        .expect("should receive message within timeout")
        .expect("message should not be None");
    assert!(
        message.payload_is_shm_backed(),
        "the subscriber must read the producer's shared-memory buffer, not a TCP copy"
    );

    // Parse in place over the borrowed view — no owned segments, no copy.
    let view = message.payload();
    let reader = peppylib::encoding::decode_message_in_place(&view).expect("in-place capnp parse");
    let decoded: capnp::data::Reader<'_> = reader
        .get_root::<capnp::any_pointer::Reader>()
        .expect("any_pointer root")
        .get_as()
        .expect("data root");
    assert_eq!(decoded, blob.as_slice());
}
