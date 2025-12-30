mod common;

use common::{CALLER_INSTANCE_ID, start_master_node};
use master_node::encoding::{InfoRequest, InfoResponse};
use master_node::names;
use peppylib::ServiceMessenger;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_info_success() {
    let started = start_master_node().await;

    // Send an info request to the master node
    let info_request = InfoRequest::new();
    let request_payload = info_request.encode().expect("encode should succeed");

    let response = ServiceMessenger::poll(
        &started.caller_handle,
        &started.master_node_name,
        CALLER_INSTANCE_ID,
        &started.master_node_name,
        names::INFO,
        Some(&started.master_node_name),
        None,
        request_payload,
        Duration::from_secs(5),
    )
    .await
    .expect("info request should succeed");

    let info_response =
        InfoResponse::decode(&response.payload().to_bytes()).expect("decode should succeed");

    // Verify the response contains expected fields
    assert_eq!(
        info_response.master_node_name, started.master_node_name,
        "master_node_name should match"
    );
    assert!(
        !info_response.host_name.is_empty(),
        "host_name should not be empty"
    );
    // The MasterNode itself is counted in the node stack
    assert_eq!(
        info_response.node_count, 1,
        "node_count should be 1 (just the master node itself)"
    );

    // Clean up
    started.task.abort();
}
