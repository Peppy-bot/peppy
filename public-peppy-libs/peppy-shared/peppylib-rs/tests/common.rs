#![allow(dead_code)]

use config::node::QoSProfile;
use peppylib::messaging::{MessengerHandle, PeerInfo, SenderTarget, TopicMessenger};
use peppylib::types::Payload;
use pmi::{Messenger, MessengerAdapter, MessengerBackend, MockAdapter};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// Deterministically wait until `publisher`'s session sees a subscriber for the
/// topic it is about to publish on, replacing a fixed "settle for zenoh
/// discovery" sleep. The arguments mirror the subsequent publish.
/// Panics if no subscriber routes within 2s.
pub async fn wait_for_topic_subscriber(
    publisher: &MessengerHandle,
    core_node: &str,
    instance_id: &str,
    target: SenderTarget,
    topic_name: &str,
) {
    let matched = TopicMessenger::wait_for_subscriber(
        publisher,
        core_node,
        instance_id,
        target,
        topic_name,
        Duration::from_secs(2),
    )
    .await
    .expect("wait_for_subscriber should not error");
    assert!(
        matched,
        "no subscriber for topic `{topic_name}` routed within 2s"
    );
}

/// Declares a pairing publisher from the producer's OWN slot `link_id` to
/// `peer`: the wire shape a pinned consumer (the paired peer, or an observer
/// of that slot) subscribes against.
pub async fn declare_pinned_publisher(
    handle: &MessengerHandle,
    core_node: &str,
    instance_id: &str,
    target: SenderTarget,
    link_id: &str,
    topic_name: &str,
    peer: &PeerInfo,
) -> peppylib::messaging::TopicPublisher {
    TopicMessenger::declare_pairing_publisher(
        handle,
        core_node,
        instance_id,
        target,
        link_id,
        topic_name,
        QoSProfile::Reliable,
        peer,
    )
    .await
    .expect("pinned publisher should declare")
}

/// Waits until a pinned consumer's wire subscription for `(core_node,
/// producer_instance, link_id)` is visible to `handle`'s session. Both pinned
/// slot kinds declare their wire subs from the forwarding task, asynchronously
/// after a slot update, so a test must synchronize on the declaration before
/// publishing. Panics if it does not appear within 2s.
pub async fn wait_for_pinned_wire_sub(
    handle: &MessengerHandle,
    core_node: &str,
    producer_instance: &str,
    target: SenderTarget,
    link_id: &str,
    topic_name: &str,
    peer: &PeerInfo,
) {
    let matched = TopicMessenger::wait_for_pairing_subscriber(
        handle,
        core_node,
        producer_instance,
        target,
        link_id,
        topic_name,
        peer,
        Duration::from_secs(2),
    )
    .await
    .expect("wait_for_subscriber should not error");
    assert!(
        matched,
        "wire subscription for `{producer_instance}` did not appear within 2s"
    );
}

/// Inverse of [`wait_for_pinned_wire_sub`]: waits until that subscription has
/// disappeared from `handle`'s session. The forwarding task drops a wire sub
/// asynchronously once the pin leaves the followed set, so a test must gate on
/// the actual teardown before probing for silence. A probe window returning
/// `false` means every poll inside it saw no matching subscriber, i.e. the drop
/// has landed; while the sub still exists the probe returns `true` immediately
/// and we retry until the deadline.
pub async fn wait_for_pinned_wire_sub_gone(
    handle: &MessengerHandle,
    core_node: &str,
    producer_instance: &str,
    target: SenderTarget,
    link_id: &str,
    topic_name: &str,
    peer: &PeerInfo,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let matched = TopicMessenger::wait_for_pairing_subscriber(
            handle,
            core_node,
            producer_instance,
            target.clone(),
            link_id,
            topic_name,
            peer,
            Duration::from_millis(25),
        )
        .await
        .expect("wait_for_subscriber should not error");
        if !matched {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "wire subscription for `{producer_instance}` did not disappear within 2s"
        );
    }
}

pub const CALLER_INSTANCE_ID: &str = "caller_instance";

pub const TEST_CORE_NODE_NAME: &str = "test_core_node";
pub const TEST_NODE_NAME: &str = "test_node";
pub const TEST_INSTANCE_ID: &str = "test_instance";
pub const TEST_NODE_TAG: &str = "v1";

/// Builds a node-shaped [`SenderTarget`] with the standard test tag. Panics on
/// invalid names — tests use known-good values only.
pub fn test_node_target(name: &str) -> SenderTarget {
    SenderTarget::node(name, TEST_NODE_TAG).expect("test node target")
}

/// Declares a publisher and publishes a single payload. The publisher is the
/// only topic-publish path, so a test that publishes once just declares then
/// publishes; the arguments mirror the old one-shot emit.
#[allow(clippy::too_many_arguments)]
pub async fn publish_once(
    messenger: &MessengerHandle,
    core_node: &str,
    instance_id: &str,
    target: SenderTarget,
    topic_name: &str,
    qos: QoSProfile,
    payload: Payload,
) -> Result<(), peppylib::PeppyError> {
    let publisher = TopicMessenger::declare_publisher(
        messenger,
        core_node,
        instance_id,
        target,
        None,
        topic_name,
        qos,
    )
    .await?;
    publisher.publish(payload).await
}

/// Client for sending requests to a test node.
pub struct CoreNodeClient {
    pub caller_handle: MessengerHandle,
    pub core_node_name: String,
    pub instance_id: String,
}

/// Creates a shared mock messenger and returns a client with a MessengerHandle.
pub async fn get_client_server() -> (CoreNodeClient, Arc<Mutex<Messenger>>) {
    let shared_messenger = create_mock_messenger().await;

    let caller_handle = MessengerHandle::from_shared(Arc::clone(&shared_messenger));

    let client = CoreNodeClient {
        caller_handle,
        core_node_name: TEST_CORE_NODE_NAME.to_string(),
        instance_id: TEST_INSTANCE_ID.to_string(),
    };

    (client, shared_messenger)
}

async fn create_mock_messenger() -> Arc<Mutex<Messenger>> {
    let adapter = MockAdapter::default();
    let mut messenger = Messenger::new(MessengerAdapter::Mock(adapter));
    messenger
        .start_session()
        .await
        .expect("failed to start mock session");
    Arc::new(Mutex::new(messenger))
}
