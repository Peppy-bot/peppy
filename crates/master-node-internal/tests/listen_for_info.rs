mod common;

use common::create_mock_messenger;
use master_node::encoding::{InfoRequest, InfoResponse};
use master_node::names;
use master_node::{MasterNode, MasterNodeArguments};
use peppylib::ServiceMessenger;
use peppylib::messaging::MessengerHandle;
use std::sync::Arc;
use std::time::Duration;

const CALLER_INSTANCE_ID: &str = "caller_instance";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_info_success() {
    let shared_messenger = create_mock_messenger().await;
    let caller_handle = MessengerHandle::from_shared(Arc::clone(&shared_messenger));

    let node_start_health_timeout = Duration::from_secs(5);
    let node_arguments = MasterNodeArguments {
        node_start_health_timeout,
    };
    let master_node = MasterNode::new(
        Arc::clone(&shared_messenger),
        Some("test_master_node"),
        node_arguments,
    );
    let master_node_name = master_node.node_name().to_string();

    // Start the master node in a separate task
    let master_node_task = tokio::spawn(async move { master_node.start().await });

    // Allow the MasterNode services to fully establish their listeners
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Send an info request to the master node
    let info_request = InfoRequest::new();
    let request_payload = info_request.encode().expect("encode should succeed");

    let response = ServiceMessenger::poll(
        &caller_handle,
        &master_node_name,
        CALLER_INSTANCE_ID,
        &master_node_name,
        names::INFO,
        Some(&master_node_name),
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
        info_response.master_node_name, master_node_name,
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
    master_node_task.abort();
}
