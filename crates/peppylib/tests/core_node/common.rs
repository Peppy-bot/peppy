//! Shared test fixtures for `core_node` integration tests: router/runner
//! setup, reachability polling, and a minimal `peppy.json5` writer. Per-test
//! files provide their own `spawn_stub_listener` because the request/response
//! types differ per service.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use config::node::QoSProfile;
use core_node_api::names;
use peppylib::messaging::SenderTarget;
use peppylib::messaging::{
    MessengerHandle, ProducerRef, ServiceMessenger, ServiceTarget, TopicMessenger,
};
use peppylib::runtime::{NodeRunner, Processor, StandaloneConfig};
use peppylib::types::Payload;
use pmi::{ZenohAdapter, ZenohdInstance};
use tempfile::TempDir;

pub(crate) const CORE_NODE: &str = "standalone-core";
pub(crate) const CLIENT_INSTANCE: &str = "test_caller";
pub(crate) const SERVER_INSTANCE: &str = "test_server";

/// Builds a node-shaped [`SenderTarget`] pointing at the core node. The core
/// node uses [`names::CORE_NODE_TAG`] for its tag (not a manifest version), so
/// these tests must mirror that tag to actually route through the wire.
pub(crate) fn test_node_target(name: &str) -> SenderTarget {
    SenderTarget::node(name, names::CORE_NODE_TAG).expect("test node target")
}

/// Declares a publisher and publishes a single payload. The publisher is the
/// only topic-publish path, so a test that publishes once just declares then
/// publishes; the arguments mirror the old one-shot emit.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn publish_once(
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

/// Writes a minimal `peppy.json5` into `dir` suitable for
/// `Processor::new_standalone`.
pub(crate) fn write_standalone_peppy_config(dir: &TempDir) -> PathBuf {
    let path = dir.path().join("peppy.json5");
    std::fs::write(
        &path,
        r#"{
            peppy_schema: "node_v1",
            manifest: { name: "test_node", tag: "v1" },
            execution: { language: "rust", run_cmd: ["./target/debug/test_node"] },
        }"#,
    )
    .expect("peppy config should be written");
    path
}

/// Polls `is_reachable` for `service_name` until it responds, bounded by a
/// 5s deadline. Replaces a fixed sleep: fast when zenoh discovery completes
/// quickly, and fails loudly with a clear panic if it never does.
pub(crate) async fn wait_until_reachable(client: &MessengerHandle, service_name: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if ServiceMessenger::is_reachable(
            client,
            CORE_NODE,
            CLIENT_INSTANCE,
            test_node_target(CORE_NODE),
            service_name,
            ServiceTarget::Producer(&ProducerRef::new(CORE_NODE, SERVER_INSTANCE)),
        )
        .await
        .expect("reachability check should succeed")
        {
            return;
        }
        if Instant::now() >= deadline {
            panic!("{service_name} stub did not become reachable within 5s");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Deterministically waits until `publisher`'s session sees a subscriber for
/// `topic_name` before the test publishes on it, replacing a fixed "settle for
/// zenoh discovery" sleep. Peer-mode discovery is not instantaneous, so a
/// publish sent before routing completes can be dropped. The arguments mirror
/// the subsequent publish. Panics if no subscriber routes within 2s.
pub(crate) async fn wait_for_topic_subscriber(
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

/// Starts an ephemeral zenoh router, builds a `NodeRunner` pointed at it, and
/// returns the router, the temp dir holding `peppy.json5`, the runner, and a
/// server-side `MessengerHandle` the caller uses to spawn its stub listener.
/// The router and temp dir must be held for the duration of the test —
/// dropping them tears down the messaging fabric and config file.
pub(crate) async fn start_router_and_runner()
-> (ZenohdInstance, TempDir, NodeRunner, MessengerHandle) {
    let router = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("start zenoh router");
    let server = MessengerHandle::from_host_port(&router.host, router.port)
        .await
        .expect("server handle");

    let temp_dir = TempDir::new().expect("temp dir should be created");
    let peppy_config_path = write_standalone_peppy_config(&temp_dir);
    let standalone_config = StandaloneConfig::new()
        .with_messaging(&router.host, router.port)
        .with_instance_id(CLIENT_INSTANCE);
    let processor = Processor::new_standalone(&peppy_config_path, &standalone_config)
        .expect("standalone processor");
    let node_runner = NodeRunner::new(processor).await.expect("node runner");

    (router, temp_dir, node_runner, server)
}
