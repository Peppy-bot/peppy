mod common;

use common::start_core_node_with_mock_messenger;
use config::consts::DEFAULT_MESSAGING_PORT;
use core_node_api::encoding::InfoRequest;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_info_success() {
    let started = start_core_node_with_mock_messenger().await;

    let info_response = common::poll_core_node(&started, &InfoRequest::new()).await;

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
