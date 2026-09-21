//! Producer-binding runtime semantics over the mock adapter: a consumer slot's
//! subscription follows one wire subscription per bound producer, a delivery
//! replaces the slot's set wholesale, and every producer's publishes fan into
//! one stream tagged with the producer that sent them.

mod common;

use common::{
    get_client_server, test_node_target, wait_for_topic_subscriber, wait_for_topic_subscriber_gone,
};
use config::node::QoSProfile;
use config::runtime::BoundProducers;
use peppylib::messaging::{
    BoundSetState, MessengerHandle, ProducerRef, TopicMessenger, TopicPublisher,
};
use peppylib::runtime::{BoundSetSubscription, CancellationToken, subscribe_bound_set_with_watch};
use peppylib::types::Payload;
use std::time::Duration;
use tokio::sync::watch;

const CORE: &str = "test_core_node";
const PRODUCER_NODE: &str = "robot_arm";
const TOPIC: &str = "joint_states";
const CONSUMER_INSTANCE: &str = "monitor_1";

fn producers(instance_ids: &[&str]) -> BoundProducers {
    BoundProducers::try_from(
        instance_ids
            .iter()
            .map(|instance_id| ProducerRef::new(CORE, *instance_id))
            .collect::<Vec<_>>(),
    )
    .expect("distinct producers")
}

fn state(sequence: u64, instance_ids: &[&str]) -> BoundSetState {
    BoundSetState {
        sequence,
        producers: producers(instance_ids),
    }
}

async fn declare_producer_publisher(handle: &MessengerHandle, instance_id: &str) -> TopicPublisher {
    TopicMessenger::declare_publisher(
        handle,
        CORE,
        instance_id,
        test_node_target(PRODUCER_NODE),
        None,
        TOPIC,
        QoSProfile::Reliable,
    )
    .await
    .expect("declare producer publisher")
}

/// Consumer-side subscription driven by a hand-held watch channel (standing in
/// for the processor-owned slot the daemon replaces).
async fn subscribe(
    handle: &MessengerHandle,
    watch_rx: watch::Receiver<BoundSetState>,
) -> BoundSetSubscription {
    subscribe_bound_set_with_watch(
        handle.clone(),
        CORE.to_string(),
        CONSUMER_INSTANCE.to_string(),
        watch_rx,
        test_node_target(PRODUCER_NODE),
        TOPIC.to_string(),
        QoSProfile::Reliable,
        CancellationToken::new(),
    )
    .await
    .expect("every bound producer declares its subscription")
}

async fn wait_for_consumer(handle: &MessengerHandle, producer: &str) {
    wait_for_topic_subscriber(
        handle,
        CORE,
        producer,
        test_node_target(PRODUCER_NODE),
        TOPIC,
    )
    .await;
}

async fn wait_for_consumer_gone(handle: &MessengerHandle, producer: &str) {
    wait_for_topic_subscriber_gone(
        handle,
        CORE,
        producer,
        test_node_target(PRODUCER_NODE),
        TOPIC,
    )
    .await;
}

async fn expect_message(subscription: &mut BoundSetSubscription, producer: &str, payload: &[u8]) {
    let (from, message) =
        tokio::time::timeout(Duration::from_secs(2), subscription.on_next_message())
            .await
            .expect("should receive a message within 2s")
            .expect("subscription should not close");
    assert_eq!(
        from,
        ProducerRef::new(CORE, producer),
        "every message is tagged with the producer that published it"
    );
    assert_eq!(&*message.payload_bytes(), payload);
}

async fn expect_silence(subscription: &mut BoundSetSubscription) {
    let outcome =
        tokio::time::timeout(Duration::from_millis(300), subscription.on_next_message()).await;
    assert!(
        outcome.is_err(),
        "expected no delivery, got: {:?}",
        outcome.unwrap().map(|(from, _)| from)
    );
}

async fn publish(publisher: &TopicPublisher, payload: &'static [u8]) {
    publisher
        .publish(Payload::from_static(payload))
        .await
        .expect("publish");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_set_hears_nothing_until_a_producer_joins_it() {
    let (client, shared) = get_client_server().await;
    let producer_handle = MessengerHandle::from_shared(shared);

    let (tx, watch_rx) = watch::channel(BoundSetState::seeded(BoundProducers::default()));
    let mut subscription = subscribe(&client.caller_handle, watch_rx).await;
    let arm_1 = declare_producer_publisher(&producer_handle, "arm_1").await;
    publish(&arm_1, b"before joining").await;
    expect_silence(&mut subscription).await;

    tx.send(state(1, &["arm_1"])).expect("watch send");
    wait_for_consumer(&producer_handle, "arm_1").await;
    publish(&arm_1, b"joined").await;
    expect_message(&mut subscription, "arm_1", b"joined").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_producer_joining_the_set_is_heard_beside_the_ones_already_bound() {
    let (client, shared) = get_client_server().await;
    let producer_handle = MessengerHandle::from_shared(shared);

    let (tx, watch_rx) = watch::channel(BoundSetState::seeded(producers(&["arm_1"])));
    let mut subscription = subscribe(&client.caller_handle, watch_rx).await;
    let arm_1 = declare_producer_publisher(&producer_handle, "arm_1").await;
    let arm_2 = declare_producer_publisher(&producer_handle, "arm_2").await;
    wait_for_consumer(&producer_handle, "arm_1").await;

    tx.send(state(1, &["arm_1", "arm_2"])).expect("watch send");
    wait_for_consumer(&producer_handle, "arm_2").await;
    publish(&arm_2, b"from the joined arm").await;
    expect_message(&mut subscription, "arm_2", b"from the joined arm").await;
    publish(&arm_1, b"from the bound arm").await;
    expect_message(&mut subscription, "arm_1", b"from the bound arm").await;
}

/// A producer leaving the set is silenced, and whatever it had already
/// published into the consumer's buffer never surfaces, while the producers
/// that stay keep delivering.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_producer_leaving_the_set_is_silenced_with_what_it_buffered() {
    let (client, shared) = get_client_server().await;
    let producer_handle = MessengerHandle::from_shared(shared);

    let (tx, watch_rx) = watch::channel(BoundSetState::seeded(producers(&["arm_1", "arm_2"])));
    let mut subscription = subscribe(&client.caller_handle, watch_rx).await;
    let arm_1 = declare_producer_publisher(&producer_handle, "arm_1").await;
    let arm_2 = declare_producer_publisher(&producer_handle, "arm_2").await;
    wait_for_consumer(&producer_handle, "arm_1").await;
    wait_for_consumer(&producer_handle, "arm_2").await;

    // Buffered before the removal, never read.
    publish(&arm_1, b"buffered before leaving").await;
    tx.send(state(1, &["arm_2"])).expect("watch send");
    wait_for_consumer_gone(&producer_handle, "arm_1").await;
    publish(&arm_1, b"after leaving").await;
    expect_silence(&mut subscription).await;

    publish(&arm_2, b"still bound").await;
    expect_message(&mut subscription, "arm_2", b"still bound").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_producer_rejoining_the_set_is_heard_again() {
    let (client, shared) = get_client_server().await;
    let producer_handle = MessengerHandle::from_shared(shared);

    let (tx, watch_rx) = watch::channel(BoundSetState::seeded(producers(&["arm_1"])));
    let mut subscription = subscribe(&client.caller_handle, watch_rx).await;
    let arm_1 = declare_producer_publisher(&producer_handle, "arm_1").await;
    wait_for_consumer(&producer_handle, "arm_1").await;

    tx.send(state(1, &[])).expect("watch send");
    wait_for_consumer_gone(&producer_handle, "arm_1").await;
    tx.send(state(2, &["arm_1"])).expect("watch send");
    wait_for_consumer(&producer_handle, "arm_1").await;
    publish(&arm_1, b"back again").await;
    expect_message(&mut subscription, "arm_1", b"back again").await;
}

/// The node side of `binding_update` over the wire: a delivery from the
/// node's own daemon replaces a set slot's producers whole, a stale retry and
/// a second answer under one sequence change nothing, and a delivery naming a
/// slot the service does not hold or coming from another machine is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binding_update_service_applies_daemon_deliveries_end_to_end() {
    use config::node::Cardinality;
    use peppylib::encoding::binding_update::BindingUpdateRequest;
    use peppylib::encoding::slot_update::SlotUpdateResponse;
    use peppylib::messaging::{
        BINDING_UPDATE_SERVICE, SenderTarget, ServiceMessenger, ServiceTarget,
    };
    use peppylib::services::binding_update::listen_for_binding_update;
    use peppylib::services::slot_update::SlotChannel;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    let (client, shared) = get_client_server().await;
    let daemon_handle = MessengerHandle::from_shared(shared);

    // The "node": one set slot 'robots'. Its scalar slots never reach the
    // service.
    let (slot_tx, slot_rx) = watch::channel(BoundSetState::seeded(BoundProducers::default()));
    let slots = Arc::new(BTreeMap::from([(
        "robots".to_string(),
        SlotChannel::new(Cardinality::ZeroOrMore, slot_tx),
    )]));
    let node_identity = SenderTarget::node("fleet_monitor", "v1").expect("node target");
    let _listener = listen_for_binding_update(
        &client.caller_handle,
        CORE,
        CONSUMER_INSTANCE,
        node_identity.clone(),
        slots,
    )
    .await
    .expect("binding_update listener should register");

    let node_ref = ProducerRef::new(CORE, CONSUMER_INSTANCE);
    let deliver_from = async |caller_core_node: &str, request: BindingUpdateRequest| {
        let reply = ServiceMessenger::poll(
            &daemon_handle,
            caller_core_node,
            "daemon",
            node_identity.clone(),
            BINDING_UPDATE_SERVICE,
            ServiceTarget::Producer(&node_ref),
            request.encode().expect("encode"),
            Duration::from_secs(2),
        )
        .await
        .expect("binding_update delivery should get a reply");
        SlotUpdateResponse::decode(&reply.payload_bytes()).expect("decode response")
    };
    let request = |link_id: &str, sequence: u64, instance_ids: &[&str]| BindingUpdateRequest {
        link_id: link_id.to_string(),
        sequence,
        producers: producers(instance_ids),
    };
    let held = || {
        slot_rx
            .borrow()
            .producers
            .producers()
            .map(|producer| producer.instance_id.clone())
            .collect::<Vec<_>>()
    };

    let response = deliver_from(CORE, request("robots", 7, &["arm_1", "arm_2"])).await;
    assert!(response.accepted, "delivery rejected: {}", response.message);
    assert_eq!(held(), ["arm_1", "arm_2"]);

    let response = deliver_from(CORE, request("robots", 8, &["arm_2"])).await;
    assert!(response.accepted, "{}", response.message);
    assert_eq!(held(), ["arm_2"], "a delivery replaces the set whole");

    let response = deliver_from(CORE, request("robots", 7, &[])).await;
    assert!(!response.accepted && response.stale_sequence);
    assert_eq!(
        held(),
        ["arm_2"],
        "a stale retry must not roll the set back"
    );

    let response = deliver_from(CORE, request("robots", 8, &["arm_1"])).await;
    assert!(!response.accepted, "{response:?}");
    assert_eq!(held(), ["arm_2"], "one sequence holds one set");

    let response = deliver_from(CORE, request("camera", 9, &["arm_1"])).await;
    assert!(!response.accepted, "{response:?}");
    assert!(
        response
            .message
            .contains("this node holds no producer set slot `camera`"),
        "{}",
        response.message
    );

    let response = deliver_from("another_core_node", request("robots", 9, &["arm_1"])).await;
    assert!(!response.accepted, "{response:?}");
    assert!(
        response.message.contains("daemon-only"),
        "{}",
        response.message
    );
    assert_eq!(held(), ["arm_2"]);
}
