//! Pairing runtime semantics over the mock adapter: unpaired slots are
//! silent, pairing pins the wire triple live, re-pins swap without duplicate
//! or stale delivery, clears silence the slot again, a multi slot hears every
//! peer it holds and reaches each one alone, and the `peer_update` service
//! applies daemon deliveries end to end.

mod common;

use common::get_client_server;
use config::node::QoSProfile;
use peppylib::messaging::{
    MessengerHandle, PEER_UPDATE_SERVICE, PeerInfo, PeerMember, PeerSetState, ProducerRef,
    SenderTarget, ServiceMessenger, ServiceTarget, TopicMessenger, TopicPublisher,
};
use peppylib::runtime::{
    PeerSubscription, declare_peer_publisher_with_watch, subscribe_peer_with_watch,
};
use peppylib::types::Payload;
use std::time::Duration;
use tokio::sync::watch;

const CORE: &str = "test_core_node";
const PAIRING_NAME: &str = "arm_link";
const PAIRING_TAG: &str = "v1";
const TOPIC: &str = "joint_states";
/// These tests play the controller consuming the arm role's `joint_states`
/// on its own slot `arm`: the recipient every arm publishes to.
const ARM_SLOT_LINK_ID: &str = "controller";
const CONSUMER_SLOT_LINK_ID: &str = "arm";
const CONSUMER_INSTANCE: &str = "ctrl_1";

fn pairing_target() -> SenderTarget {
    SenderTarget::pairing(PAIRING_NAME, PAIRING_TAG).expect("test pairing target")
}

fn pin_to(instance_id: &str) -> PeerInfo {
    PeerInfo {
        producer: ProducerRef::new(CORE, instance_id),
        peer_link_id: ARM_SLOT_LINK_ID.to_string(),
    }
}

/// The consumer as its peers address it.
fn consumer() -> PeerInfo {
    PeerInfo {
        producer: ProducerRef::new(CORE, CONSUMER_INSTANCE),
        peer_link_id: CONSUMER_SLOT_LINK_ID.to_string(),
    }
}

/// The slot holding one pair per named arm, in that order.
fn paired_with(sequence: u64, arms: &[&str]) -> PeerSetState {
    PeerSetState {
        sequence,
        members: arms
            .iter()
            .map(|arm| PeerMember {
                info: pin_to(arm),
                copy: None,
            })
            .collect(),
    }
}

/// Declares a peer instance's pairing publisher to the consumer: the wire
/// link_id segment carries the peer's OWN slot link_id, and the key names
/// the consumer's slot.
async fn declare_peer_publisher(handle: &MessengerHandle, instance_id: &str) -> TopicPublisher {
    common::declare_pinned_publisher(
        handle,
        CORE,
        instance_id,
        pairing_target(),
        ARM_SLOT_LINK_ID,
        TOPIC,
        &consumer(),
    )
    .await
}

/// Consumer-side pairing subscription driven by a hand-held watch channel
/// (standing in for the processor-owned slot the daemon mutates).
fn subscribe(
    handle: &MessengerHandle,
    watch_rx: watch::Receiver<PeerSetState>,
) -> PeerSubscription {
    subscribe_peer_with_watch(
        handle.clone(),
        CORE.to_string(),
        CONSUMER_INSTANCE.to_string(),
        CONSUMER_SLOT_LINK_ID.to_string(),
        watch_rx,
        pairing_target(),
        TOPIC.to_string(),
        QoSProfile::Reliable,
    )
    .expect("the consumer's slot is a valid link id")
}

/// Waits until the consumer's current wire subscription (pinned to
/// `peer_instance`) is visible to the publisher's session.
async fn wait_for_peer_wire_sub(handle: &MessengerHandle, peer_instance: &str) {
    common::wait_for_pinned_wire_sub(
        handle,
        CORE,
        peer_instance,
        pairing_target(),
        ARM_SLOT_LINK_ID,
        TOPIC,
        &consumer(),
    )
    .await;
}

/// Inverse of [`wait_for_peer_wire_sub`]: the deterministic sync point for a
/// clear, since the forwarding task drops the old wire sub asynchronously.
async fn wait_for_peer_wire_sub_gone(handle: &MessengerHandle, peer_instance: &str) {
    common::wait_for_pinned_wire_sub_gone(
        handle,
        CORE,
        peer_instance,
        pairing_target(),
        ARM_SLOT_LINK_ID,
        TOPIC,
        &consumer(),
    )
    .await;
}

async fn expect_message(
    subscription: &mut PeerSubscription,
    expected_peer: &str,
    expected_payload: &[u8],
) {
    let (peer, message) =
        tokio::time::timeout(Duration::from_secs(2), subscription.on_next_message())
            .await
            .expect("should receive a message within 2s")
            .expect("subscription should not close");
    assert_eq!(
        peer,
        PeerInfo {
            producer: ProducerRef::new(CORE, expected_peer),
            peer_link_id: ARM_SLOT_LINK_ID.to_string(),
        },
        "every message is tagged with the full identity of the paired peer"
    );
    assert_eq!(&*message.payload_bytes(), expected_payload);
}

async fn expect_silence(subscription: &mut PeerSubscription) {
    let outcome =
        tokio::time::timeout(Duration::from_millis(300), subscription.on_next_message()).await;
    assert!(
        outcome.is_err(),
        "expected no delivery, got: {:?}",
        outcome.unwrap().map(|(peer, _)| peer)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unpaired_slot_receives_nothing() {
    let (client, shared) = get_client_server().await;
    let peer_handle = MessengerHandle::from_shared(shared);

    let (_tx, watch_rx) = watch::channel(PeerSetState::empty());
    let mut subscription = subscribe(&client.caller_handle, watch_rx);

    // The peer publishes before any pair exists: publish-unpaired is a legal
    // no-op on the publisher side and MUST NOT reach the unpaired consumer.
    let publisher = declare_peer_publisher(&peer_handle, "arm_1").await;
    publisher
        .publish(Payload::from_static(b"pre-pairing"))
        .await
        .expect("publish while unpaired is a legal no-op");

    expect_silence(&mut subscription).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_pair_starts_delivery_without_resubscribe() {
    let (client, shared) = get_client_server().await;
    let peer_handle = MessengerHandle::from_shared(shared);

    let (tx, watch_rx) = watch::channel(PeerSetState::empty());
    let mut subscription = subscribe(&client.caller_handle, watch_rx);
    let publisher = declare_peer_publisher(&peer_handle, "arm_1").await;

    // Pair live — the subscription object predates the pair (the lazy story).
    tx.send(paired_with(1, &["arm_1"])).expect("watch send");
    wait_for_peer_wire_sub(&peer_handle, "arm_1").await;

    publisher
        .publish(Payload::from_static(b"post-pairing"))
        .await
        .expect("publish");
    expect_message(&mut subscription, "arm_1", b"post-pairing").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_identity_on_same_keyexpr_shape_is_never_delivered() {
    // Lock-in proof: a third contract-shape node publishing on the same
    // pairing (name, tag, slot link_id, topic) with a different instance
    // identity must not reach a consumer paired to someone else.
    let (client, shared) = get_client_server().await;
    let peer_handle = MessengerHandle::from_shared(std::sync::Arc::clone(&shared));
    let intruder_handle = MessengerHandle::from_shared(shared);

    let (tx, watch_rx) = watch::channel(PeerSetState::empty());
    let mut subscription = subscribe(&client.caller_handle, watch_rx);

    tx.send(paired_with(1, &["arm_1"])).expect("watch send");
    wait_for_peer_wire_sub(&peer_handle, "arm_1").await;

    let intruder = declare_peer_publisher(&intruder_handle, "intruder_1").await;
    intruder
        .publish(Payload::from_static(b"injected"))
        .await
        .expect("publish");
    expect_silence(&mut subscription).await;

    // The paired peer still flows.
    let publisher = declare_peer_publisher(&peer_handle, "arm_1").await;
    publisher
        .publish(Payload::from_static(b"legit"))
        .await
        .expect("publish");
    expect_message(&mut subscription, "arm_1", b"legit").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repin_swaps_to_the_new_peer_without_stale_or_duplicate_delivery() {
    let (client, shared) = get_client_server().await;
    let peer_handle = MessengerHandle::from_shared(shared);

    let (tx, watch_rx) = watch::channel(PeerSetState::empty());
    let mut subscription = subscribe(&client.caller_handle, watch_rx);

    let old_peer = declare_peer_publisher(&peer_handle, "arm_1").await;
    let new_peer = declare_peer_publisher(&peer_handle, "arm_2").await;

    tx.send(paired_with(1, &["arm_1"])).expect("watch send");
    wait_for_peer_wire_sub(&peer_handle, "arm_1").await;
    old_peer
        .publish(Payload::from_static(b"from arm_1"))
        .await
        .expect("publish");
    expect_message(&mut subscription, "arm_1", b"from arm_1").await;

    // Re-pin to arm_2 (failover: replacement booted with --pair).
    tx.send(paired_with(2, &["arm_2"])).expect("watch send");
    wait_for_peer_wire_sub(&peer_handle, "arm_2").await;

    // The old peer keeps publishing after the swap; nothing may surface.
    old_peer
        .publish(Payload::from_static(b"stale from arm_1"))
        .await
        .expect("publish");
    new_peer
        .publish(Payload::from_static(b"from arm_2"))
        .await
        .expect("publish");

    expect_message(&mut subscription, "arm_2", b"from arm_2").await;
    expect_silence(&mut subscription).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clear_silences_the_slot_until_repaired() {
    let (client, shared) = get_client_server().await;
    let peer_handle = MessengerHandle::from_shared(shared);

    let (tx, watch_rx) = watch::channel(PeerSetState::empty());
    let mut subscription = subscribe(&client.caller_handle, watch_rx);
    let publisher = declare_peer_publisher(&peer_handle, "arm_1").await;

    tx.send(paired_with(1, &["arm_1"])).expect("watch send");
    wait_for_peer_wire_sub(&peer_handle, "arm_1").await;
    publisher
        .publish(Payload::from_static(b"while paired"))
        .await
        .expect("publish");
    expect_message(&mut subscription, "arm_1", b"while paired").await;

    // The daemon clears the pair (peer death / node stop).
    tx.send(paired_with(2, &[])).expect("watch send");
    // Deterministic sync point for the drop: gate on the wire subscription
    // actually disappearing before probing for silence.
    wait_for_peer_wire_sub_gone(&peer_handle, "arm_1").await;
    publisher
        .publish(Payload::from_static(b"after clear"))
        .await
        .expect("publish");
    expect_silence(&mut subscription).await;

    // Re-pair resumes the stream (streams are live, not mailboxes: the
    // message published while cleared stays lost).
    tx.send(paired_with(3, &["arm_1"])).expect("watch send");
    wait_for_peer_wire_sub(&peer_handle, "arm_1").await;
    publisher
        .publish(Payload::from_static(b"after re-pair"))
        .await
        .expect("publish");
    expect_message(&mut subscription, "arm_1", b"after re-pair").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peer_update_service_applies_daemon_deliveries_end_to_end() {
    use peppylib::encoding::peer_update::PeerUpdateRequest;
    use peppylib::encoding::slot_update::SlotUpdateResponse;
    use peppylib::services::peer_update::listen_for_peer_update;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    let (client, shared) = get_client_server().await;
    let daemon_handle = MessengerHandle::from_shared(shared);

    // The "node": one declared pairing slot 'arm', service listening.
    let (slot_tx, slot_rx) = watch::channel(PeerSetState::empty());
    let slots: Arc<BTreeMap<String, watch::Sender<PeerSetState>>> =
        Arc::new(BTreeMap::from([("arm".to_string(), slot_tx)]));
    let node_identity = SenderTarget::node("arm_controller", "v1").expect("node target");
    let _listener = listen_for_peer_update(
        &client.caller_handle,
        CORE,
        CONSUMER_INSTANCE,
        node_identity.clone(),
        slots,
    )
    .await
    .expect("peer_update listener should register");

    // The "daemon" delivers a pair to the node's slot.
    let node_ref = ProducerRef::new(CORE, CONSUMER_INSTANCE);
    let request = PeerUpdateRequest {
        link_id: "arm".to_string(),
        sequence: 7,
        members: vec![PeerMember {
            info: PeerInfo {
                producer: ProducerRef::new(CORE, "arm_1"),
                peer_link_id: "controller".to_string(),
            },
            copy: Some("alpha".to_string()),
        }],
    };
    let reply = ServiceMessenger::poll(
        &daemon_handle,
        CORE,
        "daemon",
        node_identity.clone(),
        PEER_UPDATE_SERVICE,
        ServiceTarget::Producer(&node_ref),
        request.encode().expect("encode"),
        Duration::from_secs(2),
    )
    .await
    .expect("peer_update delivery should succeed");
    let response = SlotUpdateResponse::decode(&reply.payload_bytes()).expect("decode response");
    assert!(response.accepted, "delivery rejected: {}", response.message);
    assert_eq!(slot_rx.borrow().members, request.members);
    assert_eq!(slot_rx.borrow().sequence, 7);

    // A delayed stale retry must be reported stale and change nothing.
    let stale = PeerUpdateRequest {
        link_id: "arm".to_string(),
        sequence: 6,
        members: Vec::new(),
    };
    let reply = ServiceMessenger::poll(
        &daemon_handle,
        CORE,
        "daemon",
        node_identity.clone(),
        PEER_UPDATE_SERVICE,
        ServiceTarget::Producer(&node_ref),
        stale.encode().expect("encode"),
        Duration::from_secs(2),
    )
    .await
    .expect("stale delivery still gets a reply");
    let response = SlotUpdateResponse::decode(&reply.payload_bytes()).expect("decode response");
    assert!(!response.accepted);
    assert!(response.stale_sequence);
    assert_eq!(slot_rx.borrow().sequence, 7, "stale must not roll back");

    // A caller stamped with a foreign core_node is not this node's daemon:
    // it must be rejected before touching slot state, even with a fresher
    // sequence.
    let foreign = PeerUpdateRequest {
        link_id: "arm".to_string(),
        sequence: 99,
        members: Vec::new(),
    };
    let reply = ServiceMessenger::poll(
        &daemon_handle,
        "foreign_core",
        "daemon",
        node_identity,
        PEER_UPDATE_SERVICE,
        ServiceTarget::Producer(&node_ref),
        foreign.encode().expect("encode"),
        Duration::from_secs(2),
    )
    .await
    .expect("foreign delivery still gets a reply");
    let response = SlotUpdateResponse::decode(&reply.payload_bytes()).expect("decode response");
    assert!(!response.accepted, "foreign core_node must be rejected");
    assert!(!response.stale_sequence);
    assert_eq!(
        slot_rx.borrow().sequence,
        7,
        "foreign caller must not mutate the slot"
    );
    assert!(
        !slot_rx.borrow().members.is_empty(),
        "foreign clear must not land"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_multi_slot_hears_every_peer_it_holds() {
    let (client, shared) = get_client_server().await;
    let peer_handle = MessengerHandle::from_shared(shared);

    let (tx, watch_rx) = watch::channel(PeerSetState::empty());
    let mut subscription = subscribe(&client.caller_handle, watch_rx);
    let arm_1 = declare_peer_publisher(&peer_handle, "arm_1").await;
    let arm_2 = declare_peer_publisher(&peer_handle, "arm_2").await;

    tx.send(paired_with(1, &["arm_1", "arm_2"]))
        .expect("watch send");
    wait_for_peer_wire_sub(&peer_handle, "arm_1").await;
    wait_for_peer_wire_sub(&peer_handle, "arm_2").await;

    arm_1
        .publish(Payload::from_static(b"from arm_1"))
        .await
        .expect("publish");
    expect_message(&mut subscription, "arm_1", b"from arm_1").await;
    arm_2
        .publish(Payload::from_static(b"from arm_2"))
        .await
        .expect("publish");
    expect_message(&mut subscription, "arm_2", b"from arm_2").await;

    // arm_1 leaves the set: its stream ends, arm_2's goes on.
    tx.send(paired_with(2, &["arm_2"])).expect("watch send");
    wait_for_peer_wire_sub_gone(&peer_handle, "arm_1").await;
    arm_1
        .publish(Payload::from_static(b"after leaving"))
        .await
        .expect("publish");
    arm_2
        .publish(Payload::from_static(b"still paired"))
        .await
        .expect("publish");
    expect_message(&mut subscription, "arm_2", b"still paired").await;
    expect_silence(&mut subscription).await;
}

/// A scalar slot's publisher names no peer: the one peer holding its pair
/// selects the stream through its own pinned subscription, and an observer of
/// the slot hears it too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scalar_slots_publisher_reaches_the_peer_holding_its_pair() {
    let (client, shared) = get_client_server().await;
    let peer_handle = MessengerHandle::from_shared(shared);

    // arm_1 holds the pair with the controller's `arm` slot, subscribed as its
    // own slot `controller` pinned to the controller.
    let node = ProducerRef::new(CORE, CONSUMER_INSTANCE);
    let mut arm_1 = peppylib::testing::subscribe_peer_pinned(
        &peer_handle,
        CORE,
        "arm_1",
        ARM_SLOT_LINK_ID,
        pairing_target(),
        &node,
        CONSUMER_SLOT_LINK_ID,
        TOPIC,
        QoSProfile::Reliable,
    )
    .await
    .expect("pinned peer subscription");
    let publisher = TopicMessenger::declare_sole_peer_publisher(
        &client.caller_handle,
        CORE,
        CONSUMER_INSTANCE,
        pairing_target(),
        CONSUMER_SLOT_LINK_ID,
        TOPIC,
        QoSProfile::Reliable,
    )
    .await
    .expect("a scalar slot's publisher declares");
    let matched = TopicMessenger::wait_for_pairing_subscriber(
        &client.caller_handle,
        CORE,
        CONSUMER_INSTANCE,
        pairing_target(),
        CONSUMER_SLOT_LINK_ID,
        TOPIC,
        &pin_to("arm_1"),
        Duration::from_secs(2),
    )
    .await
    .expect("wait should not error");
    assert!(matched, "arm_1's subscription did not appear");

    publisher
        .publish(Payload::from_static(b"for my one peer"))
        .await
        .expect("publish");
    let received = tokio::time::timeout(Duration::from_secs(2), arm_1.on_next_message())
        .await
        .expect("a message within 2s")
        .expect("open subscription");
    assert_eq!(&*received.payload_bytes(), b"for my one peer");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_publisher_reaches_each_peer_of_its_slot_alone() {
    use peppylib::PeppyError as Error;

    let (client, shared) = get_client_server().await;
    let peers_handle = MessengerHandle::from_shared(shared);

    // Two arms, each a paired peer of the controller's `arm` slot, subscribed
    // the way a peer's own slot subscribes: pinned to the controller and to
    // their own slot.
    let node = ProducerRef::new(CORE, CONSUMER_INSTANCE);
    let subscribe_arm = |instance: &'static str| {
        let handle = peers_handle.clone();
        let node = node.clone();
        async move {
            peppylib::testing::subscribe_peer_pinned(
                &handle,
                CORE,
                instance,
                ARM_SLOT_LINK_ID,
                pairing_target(),
                &node,
                CONSUMER_SLOT_LINK_ID,
                TOPIC,
                QoSProfile::Reliable,
            )
            .await
            .expect("pinned peer subscription")
        }
    };
    let mut arm_1 = subscribe_arm("arm_1").await;
    let mut arm_2 = subscribe_arm("arm_2").await;

    let (tx, watch_rx) = watch::channel(paired_with(1, &["arm_1", "arm_2"]));
    let publisher = declare_peer_publisher_with_watch(
        client.caller_handle.clone(),
        CORE.to_string(),
        CONSUMER_INSTANCE.to_string(),
        CONSUMER_SLOT_LINK_ID.to_string(),
        watch_rx,
        pairing_target(),
        TOPIC.to_string(),
        QoSProfile::Reliable,
    );
    for arm in ["arm_1", "arm_2"] {
        let matched = TopicMessenger::wait_for_pairing_subscriber(
            &client.caller_handle,
            CORE,
            CONSUMER_INSTANCE,
            pairing_target(),
            CONSUMER_SLOT_LINK_ID,
            TOPIC,
            &pin_to(arm),
            Duration::from_secs(2),
        )
        .await
        .expect("wait should not error");
        assert!(matched, "{arm}'s subscription did not appear");
    }

    publisher
        .publish_to(&pin_to("arm_1"), Payload::from_static(b"for arm_1"))
        .await
        .expect("publish to a held peer");
    publisher
        .publish_to(&pin_to("arm_2"), Payload::from_static(b"for arm_2"))
        .await
        .expect("publish to a held peer");
    async fn received(subscription: &mut peppylib::messaging::Subscription) -> Vec<u8> {
        tokio::time::timeout(Duration::from_secs(2), subscription.on_next_message())
            .await
            .expect("a message within 2s")
            .expect("open subscription")
            .payload_bytes()
            .to_vec()
    }
    assert_eq!(received(&mut arm_1).await, b"for arm_1");
    assert_eq!(received(&mut arm_2).await, b"for arm_2");
    for subscription in [&mut arm_1, &mut arm_2] {
        assert!(
            tokio::time::timeout(Duration::from_millis(300), subscription.on_next_message())
                .await
                .is_err(),
            "a peer heard a message meant for another"
        );
    }

    // A multi slot publishes only to a peer it holds.
    tx.send(paired_with(2, &["arm_2"])).expect("watch send");
    assert!(matches!(
        publisher
            .publish_to(&pin_to("arm_1"), Payload::from_static(b"gone"))
            .await,
        Err(Error::PeerNotPaired { .. })
    ));
    assert_eq!(publisher.peers(), vec![pin_to("arm_2")]);
}
