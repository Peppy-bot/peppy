mod common;

use common::{CALLER_INSTANCE_ID, start_core_node_with_mock_messenger};
use config::consts::DEFAULT_MESSAGING_PORT;
use core_node::names;
use core_node_api::encoding::{InfoRequest, InfoResponse};
use peppylib::ServiceMessenger;
use peppylib::messaging::{NATIVE_IFACE_SEGMENT_NAME, NATIVE_IFACE_SEGMENT_TAG};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_info_success() {
    let started = start_core_node_with_mock_messenger().await;

    // Send an info request to the core node
    let info_request = InfoRequest::new();
    let request_payload = info_request.encode().expect("encode should succeed");

    let response = ServiceMessenger::poll(
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        &started.core_node_name,
        NATIVE_IFACE_SEGMENT_NAME,
        NATIVE_IFACE_SEGMENT_TAG,
        names::INFO,
        Some(&started.core_node_name),
        None,
        request_payload,
        Duration::from_secs(5),
    )
    .await
    .expect("info request should succeed");

    let info_response = InfoResponse::decode(&response.payload()).expect("decode should succeed");

    // Verify the response contains expected fields
    assert_eq!(
        info_response.core_node_name, started.core_node_name,
        "core_node_name should match"
    );
    assert!(
        !info_response.host_name.is_empty(),
        "host_name should not be empty"
    );
    // The CoreNode itself is counted in the node stack
    assert_eq!(
        info_response.node_count, 1,
        "node_count should be 1 (just the core node itself)"
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
    assert_eq!(
        info_response.messaging_port, DEFAULT_MESSAGING_PORT,
        "messaging_port should match the mock adapter's default"
    );
}
