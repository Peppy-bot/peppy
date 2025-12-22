mod common;

use common::{CALLER_INSTANCE_ID, setup_test_master_node};
use master_node::encoding::{InfoRequest, InfoResponse};
use master_node::names;
use peppylib::messaging::ServiceMessenger;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_info_request() {
    let (client, _server) = setup_test_master_node().await;

    let request = InfoRequest::new();
    let request_payload = request.encode().expect("failed to encode info request");

    let response = ServiceMessenger::poll(
        &client.caller_handle,
        &client.master_node_name,
        CALLER_INSTANCE_ID,
        &client.master_node_name,
        names::INFO,
        None,
        Some(&client.instance_id),
        request_payload,
        Duration::from_secs(2),
    )
    .await
    .expect("caller should receive response");

    let info_response =
        InfoResponse::decode(&response.payload().to_bytes()).expect("should decode info response");

    assert_eq!(info_response.master_node_name, client.master_node_name);
    assert_eq!(info_response.master_node_instance_id, client.instance_id);
    assert_eq!(response.instance_id(), client.instance_id);

    // Uptime should be reasonable (less than 10 seconds for this test)
    assert!(
        info_response.uptime_secs < 10,
        "uptime too high: {}s",
        info_response.uptime_secs
    );

    // Hostname should be a non-empty string
    assert!(
        !info_response.host_name.is_empty(),
        "hostname should not be empty"
    );

    // Node count should be at least 1 (the master node itself)
    assert!(
        info_response.node_count >= 1,
        "node_count should be at least 1, got {}",
        info_response.node_count
    );
}
