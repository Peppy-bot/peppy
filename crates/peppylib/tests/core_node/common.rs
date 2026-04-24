//! Shared test fixtures for `core_node` integration tests: router/runner
//! setup, reachability polling, and a minimal `peppy.json5` writer. Per-test
//! files provide their own `spawn_stub_listener` because the request/response
//! types differ per service.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use peppylib::messaging::{MessengerHandle, ServiceMessenger};
use peppylib::runtime::{NodeRunner, Processor, StandaloneConfig};
use pmi::{ZenohAdapter, ZenohdInstance};
use tempfile::TempDir;

pub(crate) const CORE_NODE: &str = "standalone-core";
pub(crate) const CLIENT_INSTANCE: &str = "test_caller";
pub(crate) const SERVER_INSTANCE: &str = "test_server";

/// Writes a minimal `peppy.json5` into `dir` suitable for
/// `Processor::new_standalone`.
pub(crate) fn write_standalone_peppy_config(dir: &TempDir) -> PathBuf {
    let path = dir.path().join("peppy.json5");
    std::fs::write(
        &path,
        r#"{
            schema_version: 1,
            manifest: { name: "test_node", tag: "0.1.0" },
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
            CORE_NODE,
            service_name,
            Some(CORE_NODE),
            None,
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
