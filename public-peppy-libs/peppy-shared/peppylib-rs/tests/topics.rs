mod common;

use common::{publish_once, test_node_target, wait_for_topic_subscriber};
use config::node::QoSProfile;
use peppylib::messaging::{MessengerHandle, ProducerRef, TopicMessenger};
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

    let receiver_handle = MessengerHandle::connect(&host, port)
        .await
        .expect("failed to create receiver handle");
    let sender_handle = MessengerHandle::connect(&host, port)
        .await
        .expect("failed to create sender handle");

    // Subscribe to the topic first, pinned to the publishing producer.
    let producer = ProducerRef::new(core_node, instance_id);
    let mut subscription = TopicMessenger::subscribe(
        &receiver_handle,
        core_node,
        instance_id,
        test_node_target(node_name),
        topic_name,
        &producer,
        QoSProfile::Reliable,
    )
    .await
    .expect("subscription should succeed");

    // Wait until the publisher's session sees the subscription before emitting.
    wait_for_topic_subscriber(
        &sender_handle,
        core_node,
        instance_id,
        test_node_target(node_name),
        topic_name,
    )
    .await;

    // Emit a message
    publish_once(
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
/// `connect(..).reconnecting()`, so this exercises the actual reconnecting
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
    let receiver_handle = MessengerHandle::connect(&host, port)
        .reconnecting()
        .await
        .expect("failed to create reconnecting receiver handle");
    let producer = ProducerRef::new(core_node, instance_id);
    let mut subscription = TopicMessenger::subscribe(
        &receiver_handle,
        core_node,
        instance_id,
        test_node_target(node_name),
        topic_name,
        &producer,
        QoSProfile::Reliable,
    )
    .await
    .expect("subscription should succeed");

    // Baseline: a publisher reaches the subscriber through the router.
    {
        let sender_handle = MessengerHandle::connect(&host, port)
            .await
            .expect("failed to create sender handle");
        wait_for_topic_subscriber(
            &sender_handle,
            core_node,
            instance_id,
            test_node_target(node_name),
            topic_name,
        )
        .await;
        publish_once(
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
    // A non-reconnecting `connect` would race the respawn: the freshly
    // started router can accept a TCP connection before its protocol handshake
    // has settled, failing the one-shot session open. A reconnecting publisher
    // opens immediately and connects in the background instead, so the
    // emit-until-delivered loop below drives recovery without a hand-rolled
    // connect-retry loop here.
    let sender_handle = MessengerHandle::connect(&host, port)
        .reconnecting()
        .await
        .expect("failed to create post-restart sender handle");

    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut recovered = false;
    while std::time::Instant::now() < deadline {
        // Ignore emit errors: the publisher link may still be settling.
        let _ = publish_once(
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

/// Bidirectional explicit bindings at the wire layer: two consumers each
/// subscribe to the other's topic through their slot's one bound producer,
/// exactly as a generated consumed-topic module does. The controller wants
/// two arms, so it declares two slots — one pinned subscription per slot,
/// never one subscription over several producers. Messages flow
/// independently in both directions, and a producer that joins *after* the
/// consumer is already listening is received because its slot was BOUND up
/// front — a wire pin does not require the producer to exist at subscribe
/// time. A producer that was never bound is dropped — there is no wildcard
/// fallback. This is the runtime counterpart to the launch-time binding
/// materialization checked in `crates/peppy/tests/stack_launch.rs`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bidirectional_bound_topics_with_late_bound_producer() {
    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (host, port) = (instance.host.clone(), instance.port);

    let core_node = "test_core";
    // One topic per direction, mirroring the docs' robot arm control loop.
    let joint_states = "joint_states"; // emitted by robot_arm, consumed by arm_controller
    let joint_commands = "joint_commands"; // emitted by arm_controller, consumed by robot_arm

    let controller_handle = MessengerHandle::connect(&host, port)
        .await
        .expect("failed to create arm_controller handle");
    let arm_handle = MessengerHandle::connect(&host, port)
        .await
        .expect("failed to create robot_arm handle");

    // arm_controller consumes joint_states from two robot_arm instances
    // through two declared slots, one pinned subscription each; arm_2's
    // slot is bound now but the producer joins the mesh later.
    let arm_1_producer = ProducerRef::new(core_node, "arm_1");
    let mut controller_sub_arm_1 = TopicMessenger::subscribe(
        &controller_handle,
        core_node,
        "ctrl_1",
        test_node_target("robot_arm"),
        joint_states,
        &arm_1_producer,
        QoSProfile::Reliable,
    )
    .await
    .expect("arm_controller arm_1-slot subscription should succeed");
    let arm_2_producer = ProducerRef::new(core_node, "arm_2");
    let mut controller_sub_arm_2 = TopicMessenger::subscribe(
        &controller_handle,
        core_node,
        "ctrl_1",
        test_node_target("robot_arm"),
        joint_states,
        &arm_2_producer,
        QoSProfile::Reliable,
    )
    .await
    .expect("arm_controller arm_2-slot subscription should succeed");

    // robot_arm consumes joint_commands from its bound arm_controller.
    let ctrl_producer = ProducerRef::new(core_node, "ctrl_1");
    let mut arm_sub = TopicMessenger::subscribe(
        &arm_handle,
        core_node,
        "arm_1",
        test_node_target("arm_controller"),
        joint_commands,
        &ctrl_producer,
        QoSProfile::Reliable,
    )
    .await
    .expect("robot_arm subscription should succeed");

    // Wait until each publisher's session sees the subscription it will emit to,
    // covering both directions before either emit.
    wait_for_topic_subscriber(
        &arm_handle,
        core_node,
        "arm_1",
        test_node_target("robot_arm"),
        joint_states,
    )
    .await;
    wait_for_topic_subscriber(
        &controller_handle,
        core_node,
        "ctrl_1",
        test_node_target("arm_controller"),
        joint_commands,
    )
    .await;

    // Direction 1: robot_arm (arm_1) -> arm_controller.
    let state_payload = Payload::from_static(b"joint_states@arm_1");
    publish_once(
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

    let msg = tokio::time::timeout(
        Duration::from_secs(2),
        controller_sub_arm_1.on_next_message(),
    )
    .await
    .expect("arm_controller should receive joint_states within timeout")
    .expect("message should not be None");
    assert_eq!(msg.payload(), &state_payload);
    assert_eq!(msg.instance_id(), "arm_1");
    assert_eq!(msg.core_node(), core_node);

    // Direction 2: arm_controller (ctrl_1) -> robot_arm. The reverse stream
    // flows independently through its own slot binding.
    let command_payload = Payload::from_static(b"joint_commands@ctrl_1");
    publish_once(
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

    // The second robot_arm instance joins *after* arm_controller is
    // already subscribed. Because its slot was bound (the arm_2-pinned
    // subscription already exists), the pin picks it up as soon as it
    // publishes.
    let late_arm_handle = MessengerHandle::connect(&host, port)
        .await
        .expect("failed to create late robot_arm handle");
    wait_for_topic_subscriber(
        &late_arm_handle,
        core_node,
        "arm_2",
        test_node_target("robot_arm"),
        joint_states,
    )
    .await;

    let late_payload = Payload::from_static(b"joint_states@arm_2");
    publish_once(
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

    let msg = tokio::time::timeout(
        Duration::from_secs(2),
        controller_sub_arm_2.on_next_message(),
    )
    .await
    .expect("arm_controller should receive the late bound producer within timeout")
    .expect("message should not be None");
    assert_eq!(msg.payload(), &late_payload);
    assert_eq!(
        msg.instance_id(),
        "arm_2",
        "the late bound producer must be picked up through its slot's pinned subscription",
    );

    // An UNBOUND robot_arm (arm_3) publishing the same topic must never
    // reach the consumer on either slot: explicit bindings are the whole
    // contract, with no wildcard fallback. Stronger still, no subscription
    // matches arm_3 on the wire at all — both of the consumer's
    // subscriptions pin another producer's keyexpr — so the publisher sees
    // no matching subscriber.
    let unbound_arm_handle = MessengerHandle::connect(&host, port)
        .await
        .expect("failed to create unbound robot_arm handle");
    let matched = TopicMessenger::wait_for_subscriber(
        &unbound_arm_handle,
        core_node,
        "arm_3",
        test_node_target("robot_arm"),
        joint_states,
        Duration::from_millis(500),
    )
    .await
    .expect("wait_for_subscriber should not error");
    assert!(
        !matched,
        "an unbound producer must have no matching subscriber on the wire",
    );
    publish_once(
        &unbound_arm_handle,
        core_node,
        "arm_3",
        test_node_target("robot_arm"),
        joint_states,
        QoSProfile::Reliable,
        Payload::from_static(b"joint_states@arm_3"),
    )
    .await
    .expect("unbound robot_arm emit should succeed");

    for sub in [&mut controller_sub_arm_1, &mut controller_sub_arm_2] {
        let unbound = tokio::time::timeout(Duration::from_millis(500), sub.on_next_message()).await;
        assert!(
            unbound.is_err(),
            "an unbound producer must not reach the consumer; got: {:?}",
            unbound
                .ok()
                .flatten()
                .map(|m| m.payload().as_ref().to_vec()),
        );
    }
}
