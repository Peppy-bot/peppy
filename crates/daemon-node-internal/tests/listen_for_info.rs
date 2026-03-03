mod common;

use common::{CALLER_INSTANCE_ID, start_daemon_node_with_mock_messenger};
use daemon_node::encoding::{InfoRequest, InfoResponse};
use daemon_node::names;
use peppylib::ServiceMessenger;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_info_success() {
    let started = start_daemon_node_with_mock_messenger().await;

    // Send an info request to the daemon node
    let info_request = InfoRequest::new();
    let request_payload = info_request.encode().expect("encode should succeed");

    let response = ServiceMessenger::poll(
        &started.caller_handle,
        &started.daemon_node_name,
        CALLER_INSTANCE_ID,
        &started.daemon_node_name,
        names::INFO,
        Some(&started.daemon_node_name),
        None,
        request_payload,
        Duration::from_secs(5),
    )
    .await
    .expect("info request should succeed");

    let info_response = InfoResponse::decode(&response.payload()).expect("decode should succeed");

    // Verify the response contains expected fields
    assert_eq!(
        info_response.daemon_node_name, started.daemon_node_name,
        "daemon_node_name should match"
    );
    assert!(
        !info_response.host_name.is_empty(),
        "host_name should not be empty"
    );
    // The DaemonNode itself is counted in the node stack
    assert_eq!(
        info_response.node_count, 1,
        "node_count should be 1 (just the daemon node itself)"
    );
    assert!(
        !info_response.git_version.is_empty(),
        "git_version should not be empty"
    );
    assert!(
        !info_response.container_info.apptainer_version.is_empty(),
        "apptainer_version should not be empty"
    );
    assert!(
        !info_response.container_info.lima_version.is_empty(),
        "lima_version should not be empty"
    );
}
