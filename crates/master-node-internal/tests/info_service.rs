mod common;

use common::{CALLER_INSTANCE_ID, setup_test_master_node};
use master_node::encoding::{InfoRequest, InfoResponse, InfoType};
use peppylib::messaging::ServiceMessenger;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_info_master_node_name_request() {
    let test_node = setup_test_master_node().await;

    let request = InfoRequest::new(InfoType::MasterNodeName);
    let request_payload = request.encode().expect("failed to encode info request");

    let response = ServiceMessenger::poll(
        &test_node.caller_handle,
        &test_node.master_node_name,
        CALLER_INSTANCE_ID,
        &test_node.master_node_name,
        "info",
        None,
        Some(&test_node.instance_id),
        request_payload,
        Duration::from_secs(2),
    )
    .await
    .expect("caller should receive response");

    let info_response =
        InfoResponse::decode(&response.payload().to_bytes()).expect("should decode info response");

    assert_eq!(info_response.info_type, InfoType::MasterNodeName);
    assert_eq!(info_response.value, test_node.master_node_name);
    assert_eq!(response.instance_id(), test_node.instance_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_info_master_node_instance_id_request() {
    let test_node = setup_test_master_node().await;

    let request = InfoRequest::new(InfoType::MasterNodeInstanceId);
    let request_payload = request.encode().expect("failed to encode info request");

    let response = ServiceMessenger::poll(
        &test_node.caller_handle,
        &test_node.master_node_name,
        CALLER_INSTANCE_ID,
        &test_node.master_node_name,
        "info",
        None,
        Some(&test_node.instance_id),
        request_payload,
        Duration::from_secs(2),
    )
    .await
    .expect("caller should receive response");

    let info_response =
        InfoResponse::decode(&response.payload().to_bytes()).expect("should decode info response");

    assert_eq!(info_response.info_type, InfoType::MasterNodeInstanceId);
    assert_eq!(info_response.value, test_node.instance_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_info_uptime_request() {
    let test_node = setup_test_master_node().await;

    let request = InfoRequest::new(InfoType::Uptime);
    let request_payload = request.encode().expect("failed to encode info request");

    let response = ServiceMessenger::poll(
        &test_node.caller_handle,
        &test_node.master_node_name,
        CALLER_INSTANCE_ID,
        &test_node.master_node_name,
        "info",
        None,
        Some(&test_node.instance_id),
        request_payload,
        Duration::from_secs(2),
    )
    .await
    .expect("caller should receive response");

    let info_response =
        InfoResponse::decode(&response.payload().to_bytes()).expect("should decode info response");

    assert_eq!(info_response.info_type, InfoType::Uptime);
    // Uptime should be a valid number (seconds since start)
    let uptime_secs: u64 = info_response
        .value
        .parse()
        .expect("uptime should be a valid number");
    // Uptime should be reasonable (less than 10 seconds for this test)
    assert!(uptime_secs < 10, "uptime too high: {uptime_secs}s");
}
