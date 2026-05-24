use std::time::Duration;

use core_node_api::encoding::{ContainerInfo, InfoRequest, InfoResponse};
use core_node_api::names;
use peppylib::info;
use peppylib::messaging::{MessengerHandle, ServiceMessenger};
use peppylib::runtime::NodeRunner;
use pmi::ZenohdInstance;
use tempfile::TempDir;

use super::common::{
    CORE_NODE, SERVER_INSTANCE, start_router_and_runner, test_node_target, wait_until_reachable,
};

/// Spins up a single-shot `INFO` listener that returns `response` verbatim.
async fn spawn_stub_listener(server: MessengerHandle, response: InfoResponse) {
    let mut endpoint = ServiceMessenger::listen(
        &server,
        CORE_NODE,
        SERVER_INSTANCE,
        test_node_target(CORE_NODE),
        names::INFO,
    )
    .await
    .expect("listen should succeed");

    tokio::spawn(async move {
        endpoint
            .handle_next_request(|request| async move {
                let payload = request.message().payload();
                let _inbound = InfoRequest::decode(payload.as_ref()).expect("decode InfoRequest");
                Ok(response.encode().expect("encode InfoResponse"))
            })
            .await
            .expect("handle_next_request should succeed");
    });
}

/// Spawns the stub listener for `response` on a shared router/runner, and
/// waits for reachability. The router and temp dir are returned so callers
/// hold them for the duration of the test — dropping them tears down the
/// messaging fabric / config file.
async fn setup_stub(response: InfoResponse) -> (ZenohdInstance, TempDir, NodeRunner) {
    let (router, temp_dir, node_runner, server) = start_router_and_runner().await;
    spawn_stub_listener(server, response).await;
    wait_until_reachable(node_runner.messenger(), names::INFO).await;
    (router, temp_dir, node_runner)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn info_returns_typed_response_fields() {
    let response = InfoResponse::new(
        1234,
        "standalone-core",
        "core-instance-1",
        "test-host",
        7,
        "v0.9.9",
        ContainerInfo {
            apptainer_version: "1.3.0".to_string(),
            lima_version: "0.20.0".to_string(),
        },
        7447,
    );

    let (_router, _temp_dir, node_runner) = setup_stub(response.clone()).await;

    let result = info(&node_runner, Duration::from_secs(3))
        .await
        .expect("info should succeed");

    assert_eq!(result, response);
    assert_eq!(result.uptime_secs, 1234);
    assert_eq!(result.core_node_name, "standalone-core");
    assert_eq!(result.core_node_instance_id, "core-instance-1");
    assert_eq!(result.host_name, "test-host");
    assert_eq!(result.node_count, 7);
    assert_eq!(result.git_version, "v0.9.9");
    assert_eq!(result.container_info.apptainer_version, "1.3.0");
    assert_eq!(result.container_info.lima_version, "0.20.0");
    assert_eq!(result.messaging_port, 7447);
}
