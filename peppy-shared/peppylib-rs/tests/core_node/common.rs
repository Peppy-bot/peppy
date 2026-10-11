//! Shared test fixtures for `core_node` integration tests: router/runner
//! setup, reachability polling, and a minimal `peppy.json5` writer. Per-test
//! files provide their own `spawn_stub_listener` because the request/response
//! types differ per service.

use std::path::PathBuf;
use std::time::Duration;

use config::runtime::ClockBinding;
use core_node_api::{ServiceId, names};
use peppylib::messaging::SenderTarget;
use peppylib::messaging::{MessengerHandle, ProducerRef, ServiceEndpoint, ServiceMessenger};
use peppylib::runtime::{NodeRunner, Processor, StandaloneConfig};
use peppylib::testing::EphemeralRouter;
use tempfile::TempDir;

/// The topic helpers take the target as an argument, so this binary uses the
/// crate-wide ones; only [`test_node_target`] differs here.
pub(crate) use crate::shared::{publish_once, wait_for_topic_subscriber};

/// The core node the stubs serve as: the one a standalone runner talks to, so
/// its service calls and topic subscriptions reach them.
pub(crate) const CORE_NODE: &str = peppylib::runtime::STANDALONE_CORE_NODE;
pub(crate) const CLIENT_INSTANCE: &str = "test_caller";
pub(crate) const SERVER_INSTANCE: &str = "test_server";

/// Builds a node-shaped [`SenderTarget`] pointing at the core node. The core
/// node uses [`names::CORE_NODE_TAG`] for its tag (not a manifest version), so
/// these tests must mirror that tag to actually route through the wire.
pub(crate) fn test_node_target(name: &str) -> SenderTarget {
    SenderTarget::node(name, names::CORE_NODE_TAG).expect("test node target")
}

/// Listens for `service` on `server` as the core node, the identity the
/// runner's core-node calls address, which every stub must serve under.
pub(crate) async fn listen_as_core_node(
    server: &MessengerHandle,
    service: ServiceId,
) -> ServiceEndpoint {
    ServiceMessenger::listen(
        server,
        CORE_NODE,
        SERVER_INSTANCE,
        test_node_target(CORE_NODE),
        service.name(),
    )
    .await
    .unwrap_or_else(|e| panic!("listen {} should succeed: {e}", service.name()))
}

/// Writes a minimal `peppy.json5` into `dir` suitable for
/// `Processor::new_standalone`.
pub(crate) fn write_standalone_peppy_config(dir: &TempDir) -> PathBuf {
    let path = dir.path().join("peppy.json5");
    std::fs::write(
        &path,
        r#"{
            peppy_schema: "node/v1",
            manifest: { name: "test_node", tag: "v1" },
            execution: { language: "rust", run_cmd: ["./target/debug/test_node"] },
        }"#,
    )
    .expect("peppy config should be written");
    path
}

/// Polls `is_reachable` for `service_name` until it responds, bounded by a
/// 5s deadline, via the shared `peppylib::testing` wait: fast when zenoh
/// discovery completes quickly, and fails loudly if it never does.
pub(crate) async fn wait_until_reachable(client: &MessengerHandle, service_name: &str) {
    peppylib::testing::wait_service_reachable(
        client,
        CORE_NODE,
        CLIENT_INSTANCE,
        test_node_target(CORE_NODE),
        service_name,
        &ProducerRef::new(CORE_NODE, SERVER_INSTANCE),
        Duration::from_secs(5),
    )
    .await
    .unwrap_or_else(|_| panic!("{service_name} stub did not become reachable within 5s"));
}

/// Starts an ephemeral zenoh router (via `peppylib::testing`, which also
/// serializes meshes within this binary and raises the fd limit), builds a
/// `NodeRunner` pointed at it, and returns the router, the temp dir holding
/// `peppy.json5`, the runner, and a server-side `MessengerHandle` the caller
/// uses to spawn its stub listener. The router and temp dir must be held for
/// the duration of the test. Dropping them tears down the messaging fabric
/// and config file.
pub(crate) async fn start_router_and_runner()
-> (EphemeralRouter, TempDir, NodeRunner, MessengerHandle) {
    start_router_and_runner_with_clock(ClockBinding::Wall).await
}

/// [`start_router_and_runner`] with the runner bound to `clock`, the
/// daemon-less spelling of the binding a launch or a `peppy node run` would
/// have stamped on it.
pub(crate) async fn start_router_and_runner_with_clock(
    clock: ClockBinding,
) -> (EphemeralRouter, TempDir, NodeRunner, MessengerHandle) {
    let router = EphemeralRouter::start().await.expect("start zenoh router");
    // The node runner opens its session under a workspace namespace
    // (`connect(..).reconnecting().scope(SessionScope::Discovery(..))`); the standalone config
    // carries no workspace id, so it resolves to the `local` namespace. The
    // stub server must open under that same namespace (zenoh sessions only
    // interoperate when their namespaces match), or the runner's service
    // queries and topic subscriptions never reach the stub.
    let server = router.connect().await.expect("server handle");

    let temp_dir = TempDir::new().expect("temp dir should be created");
    let peppy_config_path = write_standalone_peppy_config(&temp_dir);
    let standalone_config = StandaloneConfig::new()
        .with_messaging(router.host(), router.port())
        .with_instance_id(CLIENT_INSTANCE)
        .with_clock(clock);
    let processor = Processor::new_standalone(&peppy_config_path, &standalone_config)
        .expect("standalone processor");
    let node_runner = NodeRunner::new(processor).await.expect("node runner");

    (router, temp_dir, node_runner, server)
}
