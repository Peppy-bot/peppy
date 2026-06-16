#![allow(dead_code)]

use config::node::QoSProfile;
use peppylib::messaging::{MessengerHandle, SenderTarget, TopicMessenger};
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
