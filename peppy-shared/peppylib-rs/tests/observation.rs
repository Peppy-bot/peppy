//! Observer runtime semantics over the mock adapter: an observer slot follows
//! one wire subscription per member of its set, a delivery replaces that set
//! wholesale, and every member's publishes fan into one stream tagged with the
//! member that sent them.

mod common;

use common::get_client_server;
use config::node::QoSProfile;
use peppylib::messaging::{
    MessengerHandle, ObservationState, ObservedMemberState, ObservedSource, PeerInfo, ProducerRef,
    SenderTarget, TopicPublisher,
};
use peppylib::runtime::{ObservedTopicSubscription, subscribe_observed_with_watch};
use peppylib::types::Payload;
use std::time::Duration;
use tokio::sync::watch;

const CORE: &str = "test_core_node";
const PAIRING_NAME: &str = "arm_link";
const PAIRING_TAG: &str = "v1";
const TOPIC: &str = "joint_states";
/// The source-side participant slot the observed role publishes under. The
/// observer's own link_id never appears on the wire.
const SOURCE_SLOT_LINK_ID: &str = "controller";
const OBSERVER_INSTANCE: &str = "commander_1";

fn pairing_target() -> SenderTarget {
    SenderTarget::pairing(PAIRING_NAME, PAIRING_TAG).expect("test pairing target")
}

/// The peer every source publishes to. An observer hears a source's emission
/// whichever peer it is for, so the tests address one nominal controller.
fn nominal_peer() -> PeerInfo {
    PeerInfo {
        producer: ProducerRef::new(CORE, "ctrl_x"),
        peer_link_id: "arm".to_string(),
    }
}

fn member(instance_id: &str, generation: u64) -> ObservedMemberState {
    member_on_link(instance_id, SOURCE_SLOT_LINK_ID, generation)
}

/// A member observed through `source_link_id`, for slots whose members share
/// one instance and differ only in the source slot they publish under.
fn member_on_link(instance_id: &str, source_link_id: &str, generation: u64) -> ObservedMemberState {
    ObservedMemberState {
        source: ObservedSource {
            producer: ProducerRef::new(CORE, instance_id),
            source_link_id: source_link_id.to_string(),
            peer: None,
        },
        source_generation: generation,
        source_live: true,
    }
}

fn state(sequence: u64, members: Vec<ObservedMemberState>) -> ObservationState {
    ObservationState { sequence, members }
}

async fn declare_source_publisher(handle: &MessengerHandle, instance_id: &str) -> TopicPublisher {
    declare_source_publisher_on_link(handle, instance_id, SOURCE_SLOT_LINK_ID).await
}

async fn declare_source_publisher_on_link(
    handle: &MessengerHandle,
    instance_id: &str,
    source_link_id: &str,
) -> TopicPublisher {
    common::declare_pinned_publisher(
        handle,
        CORE,
        instance_id,
        pairing_target(),
        source_link_id,
        TOPIC,
        &nominal_peer(),
    )
    .await
}

/// Observer-side subscription driven by a hand-held watch channel (standing in
/// for the processor-owned slot the daemon mutates).
fn subscribe(
    handle: &MessengerHandle,
    watch_rx: watch::Receiver<ObservationState>,
) -> ObservedTopicSubscription {
    subscribe_observed_with_watch(
        handle.clone(),
        CORE.to_string(),
        OBSERVER_INSTANCE.to_string(),
        watch_rx,
        pairing_target(),
        TOPIC.to_string(),
        QoSProfile::Reliable,
    )
}

/// Waits until the observer's wire subscription pinned to `source_instance` is
/// visible to the publisher's session.
async fn wait_for_source_wire_sub(handle: &MessengerHandle, source_instance: &str) {
    wait_for_source_wire_sub_on_link(handle, source_instance, SOURCE_SLOT_LINK_ID).await
}

async fn wait_for_source_wire_sub_on_link(
    handle: &MessengerHandle,
    source_instance: &str,
    source_link_id: &str,
) {
    common::wait_for_pinned_wire_sub(
        handle,
        CORE,
        source_instance,
        pairing_target(),
        source_link_id,
        TOPIC,
        &nominal_peer(),
    )
    .await;
}

/// Inverse of [`wait_for_source_wire_sub`]: the deterministic sync point for a
/// member leaving the set.
async fn wait_for_source_wire_sub_gone(handle: &MessengerHandle, source_instance: &str) {
    common::wait_for_pinned_wire_sub_gone(
        handle,
        CORE,
        source_instance,
        pairing_target(),
        SOURCE_SLOT_LINK_ID,
        TOPIC,
        &nominal_peer(),
    )
    .await;
}

async fn expect_message(
    subscription: &mut ObservedTopicSubscription,
    expected_producer: &str,
    expected_payload: &[u8],
) {
    expect_message_from_source(
        subscription,
        &ObservedSource {
            producer: ProducerRef::new(CORE, expected_producer),
            source_link_id: SOURCE_SLOT_LINK_ID.to_string(),
            peer: None,
        },
        expected_payload,
    )
    .await
}

async fn expect_message_from_source(
    subscription: &mut ObservedTopicSubscription,
    expected_source: &ObservedSource,
    expected_payload: &[u8],
) {
    let (source, message) =
        tokio::time::timeout(Duration::from_secs(2), subscription.on_next_message())
            .await
            .expect("should receive a message within 2s")
            .expect("subscription should not close");
    assert_eq!(
        &source, expected_source,
        "every message is tagged with the full identity of the member that published it"
    );
    assert_eq!(&*message.payload_bytes(), expected_payload);
}

/// Waits for a message from `expected_producer`, republishing until one lands.
///
/// A generation bump redeclares that member's wire subscription under an
/// IDENTICAL keyexpr, so the admin space cannot tell the new subscription from
/// the old one and there is no state to gate on the way
/// [`wait_for_source_wire_sub`] gates on a first declaration. A single publish
/// can therefore land in the drop-before-redeclare gap and be legitimately lost
/// (observation is a live stream, not a mailbox). Republishing asserts the
/// stream is live again without depending on how fast the redeclare lands.
async fn expect_message_after_redeclare(
    subscription: &mut ObservedTopicSubscription,
    publisher: &TopicPublisher,
    expected_producer: &str,
    payload: &'static [u8],
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        publisher
            .publish(Payload::from_static(payload))
            .await
            .expect("publish");
        if let Ok(Some((source, message))) =
            tokio::time::timeout(Duration::from_millis(50), subscription.on_next_message()).await
        {
            assert_eq!(source.producer.instance_id, expected_producer);
            assert_eq!(source.source_link_id, SOURCE_SLOT_LINK_ID);
            assert_eq!(&*message.payload_bytes(), payload);
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no message from `{expected_producer}` within 5s of the redeclare"
        );
    }
    // The loop may have queued copies behind the one that arrived; drop them so
    // the next expectation reads a message it actually caused.
    while tokio::time::timeout(Duration::from_millis(50), subscription.on_next_message())
        .await
        .is_ok()
    {}
}

async fn expect_silence(subscription: &mut ObservedTopicSubscription) {
    let outcome =
        tokio::time::timeout(Duration::from_millis(300), subscription.on_next_message()).await;
    assert!(
        outcome.is_err(),
        "expected no delivery, got: {:?}",
        outcome.unwrap().map(|(source, _)| source)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_member_set_receives_nothing() {
    let (client, shared) = get_client_server().await;
    let source_handle = MessengerHandle::from_shared(shared);

    let (_tx, watch_rx) = watch::channel(ObservationState::unregistered());
    let mut subscription = subscribe(&client.caller_handle, watch_rx);

    let publisher = declare_source_publisher(&source_handle, "arm_1").await;
    publisher
        .publish(Payload::from_static(b"before observation"))
        .await
        .expect("publish to an unobserved slot is a legal no-op");

    expect_silence(&mut subscription).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_member_of_the_set_fans_into_one_stream() {
    let (client, shared) = get_client_server().await;
    let source_handle = MessengerHandle::from_shared(shared);

    let (tx, watch_rx) = watch::channel(ObservationState::unregistered());
    let mut subscription = subscribe(&client.caller_handle, watch_rx);
    let arm_1 = declare_source_publisher(&source_handle, "arm_1").await;
    let arm_2 = declare_source_publisher(&source_handle, "arm_2").await;

    tx.send(state(1, vec![member("arm_1", 1), member("arm_2", 1)]))
        .expect("watch send");
    wait_for_source_wire_sub(&source_handle, "arm_1").await;
    wait_for_source_wire_sub(&source_handle, "arm_2").await;

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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_member_leaving_the_set_silences_only_itself() {
    let (client, shared) = get_client_server().await;
    let source_handle = MessengerHandle::from_shared(shared);

    let (tx, watch_rx) = watch::channel(ObservationState::unregistered());
    let mut subscription = subscribe(&client.caller_handle, watch_rx);
    let arm_1 = declare_source_publisher(&source_handle, "arm_1").await;
    let arm_2 = declare_source_publisher(&source_handle, "arm_2").await;

    tx.send(state(1, vec![member("arm_1", 1), member("arm_2", 1)]))
        .expect("watch send");
    wait_for_source_wire_sub(&source_handle, "arm_1").await;
    wait_for_source_wire_sub(&source_handle, "arm_2").await;

    // A replan drops arm_1 from the slot; the delivery carries the whole
    // remaining set.
    tx.send(state(2, vec![member("arm_2", 1)]))
        .expect("watch send");
    wait_for_source_wire_sub_gone(&source_handle, "arm_1").await;

    arm_1
        .publish(Payload::from_static(b"after leaving"))
        .await
        .expect("publish");
    expect_silence(&mut subscription).await;

    // The surviving member never lost its subscription.
    arm_2
        .publish(Payload::from_static(b"still observed"))
        .await
        .expect("publish");
    expect_message(&mut subscription, "arm_2", b"still observed").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_generation_bump_redeclares_only_that_member() {
    let (client, shared) = get_client_server().await;
    let source_handle = MessengerHandle::from_shared(shared);

    let (tx, watch_rx) = watch::channel(ObservationState::unregistered());
    let mut subscription = subscribe(&client.caller_handle, watch_rx);
    let arm_1 = declare_source_publisher(&source_handle, "arm_1").await;
    let arm_2 = declare_source_publisher(&source_handle, "arm_2").await;

    tx.send(state(1, vec![member("arm_1", 1), member("arm_2", 1)]))
        .expect("watch send");
    wait_for_source_wire_sub(&source_handle, "arm_1").await;
    wait_for_source_wire_sub(&source_handle, "arm_2").await;
    arm_2
        .publish(Payload::from_static(b"first incarnation"))
        .await
        .expect("publish");
    expect_message(&mut subscription, "arm_2", b"first incarnation").await;

    // arm_2 restarts under the same instance_id: its wire triple is unchanged,
    // so only the generation tells the incarnations apart.
    tx.send(state(2, vec![member("arm_1", 1), member("arm_2", 2)]))
        .expect("watch send");
    expect_message_after_redeclare(&mut subscription, &arm_2, "arm_2", b"second incarnation").await;

    // The untouched member kept delivering across its neighbor's restart: its
    // own subscription was never dropped, so one publish is enough.
    arm_1
        .publish(Payload::from_static(b"undisturbed"))
        .await
        .expect("publish");
    expect_message(&mut subscription, "arm_1", b"undisturbed").await;
}

/// Two members of one slot can be the same instance observed through two
/// different source slots (four `commanded_*` links on one backbone instance
/// is the canonical deployment). The producer pair is identical for both, so
/// the source link_id on the yielded tag is what tells them apart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn members_sharing_one_instance_are_told_apart_by_source_link_id() {
    let (client, shared) = get_client_server().await;
    let source_handle = MessengerHandle::from_shared(shared);

    let (tx, watch_rx) = watch::channel(ObservationState::unregistered());
    let mut subscription = subscribe(&client.caller_handle, watch_rx);
    let left = declare_source_publisher_on_link(&source_handle, "backbone_1", "left_arm").await;
    let right = declare_source_publisher_on_link(&source_handle, "backbone_1", "right_arm").await;

    tx.send(state(
        1,
        vec![
            member_on_link("backbone_1", "left_arm", 1),
            member_on_link("backbone_1", "right_arm", 1),
        ],
    ))
    .expect("watch send");
    wait_for_source_wire_sub_on_link(&source_handle, "backbone_1", "left_arm").await;
    wait_for_source_wire_sub_on_link(&source_handle, "backbone_1", "right_arm").await;

    left.publish(Payload::from_static(b"left setpoints"))
        .await
        .expect("publish");
    expect_message_from_source(
        &mut subscription,
        &ObservedSource {
            producer: ProducerRef::new(CORE, "backbone_1"),
            source_link_id: "left_arm".to_string(),
            peer: None,
        },
        b"left setpoints",
    )
    .await;

    right
        .publish(Payload::from_static(b"right setpoints"))
        .await
        .expect("publish");
    expect_message_from_source(
        &mut subscription,
        &ObservedSource {
            producer: ProducerRef::new(CORE, "backbone_1"),
            source_link_id: "right_arm".to_string(),
            peer: None,
        },
        b"right setpoints",
    )
    .await;
}

/// An observation pinned to one pair of a multi slot hears that pair alone:
/// the engine publishes to two controllers on one slot, the observer named the
/// pair by `ctrl_1`, and the copy for `ctrl_2` never reaches it. Observing the
/// slot itself hears every pair, tagged with the one source.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_observation_pinned_to_a_pair_hears_that_pair_only() {
    let (client, shared) = get_client_server().await;
    let source_handle = MessengerHandle::from_shared(shared);
    let ctrl = |instance: &str| PeerInfo {
        producer: ProducerRef::new(CORE, instance),
        peer_link_id: "arm".to_string(),
    };
    let publisher_to = async |instance: &str| {
        common::declare_pinned_publisher(
            &source_handle,
            CORE,
            "engine_1",
            pairing_target(),
            "limbs",
            TOPIC,
            &ctrl(instance),
        )
        .await
    };
    let to_ctrl_1 = publisher_to("ctrl_1").await;
    let to_ctrl_2 = publisher_to("ctrl_2").await;
    let wait_for_wire_sub_to = async |instance: &str| {
        common::wait_for_pinned_wire_sub(
            &source_handle,
            CORE,
            "engine_1",
            pairing_target(),
            "limbs",
            TOPIC,
            &ctrl(instance),
        )
        .await
    };

    let (tx, watch_rx) = watch::channel(ObservationState::unregistered());
    let mut subscription = subscribe(&client.caller_handle, watch_rx);
    let pinned = ObservedSource {
        producer: ProducerRef::new(CORE, "engine_1"),
        source_link_id: "limbs".to_string(),
        peer: Some(ctrl("ctrl_1")),
    };
    tx.send(state(
        1,
        vec![ObservedMemberState {
            source: pinned.clone(),
            source_generation: 1,
            source_live: true,
        }],
    ))
    .expect("watch send");
    wait_for_wire_sub_to("ctrl_1").await;

    // The copy for the other pair goes out first; the pinned observer's next
    // message is the one for its own pair, and nothing follows it.
    to_ctrl_2
        .publish(Payload::from_static(b"for ctrl_2"))
        .await
        .expect("publish");
    to_ctrl_1
        .publish(Payload::from_static(b"for ctrl_1"))
        .await
        .expect("publish");
    expect_message_from_source(&mut subscription, &pinned, b"for ctrl_1").await;
    expect_silence(&mut subscription).await;

    // The same slot observed whole hears both pairs under one tag.
    let whole_slot = member_on_link("engine_1", "limbs", 1);
    tx.send(state(2, vec![whole_slot.clone()]))
        .expect("watch send");
    wait_for_wire_sub_to("ctrl_2").await;
    to_ctrl_2
        .publish(Payload::from_static(b"for ctrl_2"))
        .await
        .expect("publish");
    expect_message_from_source(&mut subscription, &whole_slot.source, b"for ctrl_2").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn observation_update_service_applies_daemon_deliveries_end_to_end() {
    use peppylib::encoding::observation_update::ObservationUpdateRequest;
    use peppylib::encoding::slot_update::SlotUpdateResponse;
    use peppylib::messaging::{OBSERVATION_UPDATE_SERVICE, ServiceMessenger, ServiceTarget};
    use peppylib::services::observation_update::listen_for_observation_update;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    let (client, shared) = get_client_server().await;
    let daemon_handle = MessengerHandle::from_shared(shared);

    // The "node": one declared observer slot 'observed_joints', service
    // listening.
    let (slot_tx, slot_rx) = watch::channel(ObservationState::unregistered());
    let slots: Arc<BTreeMap<String, watch::Sender<ObservationState>>> =
        Arc::new(BTreeMap::from([("observed_joints".to_string(), slot_tx)]));
    let node_identity = SenderTarget::node("openarm_web_commander", "v1").expect("node target");
    let _listener = listen_for_observation_update(
        &client.caller_handle,
        CORE,
        OBSERVER_INSTANCE,
        node_identity.clone(),
        slots,
    )
    .await
    .expect("observation_update listener should register");

    let node_ref = ProducerRef::new(CORE, OBSERVER_INSTANCE);
    let deliver = async |request: ObservationUpdateRequest| {
        let reply = ServiceMessenger::poll(
            &daemon_handle,
            CORE,
            "daemon",
            node_identity.clone(),
            OBSERVATION_UPDATE_SERVICE,
            ServiceTarget::Producer(&node_ref),
            request.encode().expect("encode"),
            Duration::from_secs(2),
        )
        .await
        .expect("observation_update delivery should get a reply");
        SlotUpdateResponse::decode(&reply.payload_bytes()).expect("decode response")
    };

    let response = deliver(ObservationUpdateRequest {
        link_id: "observed_joints".to_string(),
        sequence: 7,
        members: vec![member("arm_1", 1), member("arm_2", 1)],
    })
    .await;
    assert!(response.accepted, "delivery rejected: {}", response.message);
    assert_eq!(slot_rx.borrow().members.len(), 2);
    assert_eq!(slot_rx.borrow().sequence, 7);

    // A shrinking delivery replaces the set rather than merging into it.
    let response = deliver(ObservationUpdateRequest {
        link_id: "observed_joints".to_string(),
        sequence: 8,
        members: vec![member("arm_2", 1)],
    })
    .await;
    assert!(response.accepted);
    assert_eq!(
        slot_rx
            .borrow()
            .members
            .iter()
            .map(|m| m.source.producer.instance_id.clone())
            .collect::<Vec<_>>(),
        ["arm_2"]
    );

    // A delayed stale retry must be reported stale and change nothing.
    let response = deliver(ObservationUpdateRequest {
        link_id: "observed_joints".to_string(),
        sequence: 7,
        members: Vec::new(),
    })
    .await;
    assert!(!response.accepted);
    assert!(response.stale_sequence);
    assert_eq!(
        slot_rx.borrow().members.len(),
        1,
        "stale must not roll the set back"
    );
}
