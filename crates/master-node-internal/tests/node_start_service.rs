mod common;

use common::create_mock_messenger;
use master_node::{MasterNode, MasterNodeArguments};
use peppylib::messaging::MessengerHandle;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_start_timeout() {
    const TARGET_NODE_NAME: &str = "runnable_node";
    const TARGET_INSTANCE_ID: &str = "runnable_instance";

    let shared_messenger = create_mock_messenger().await;
    let caller_handle = MessengerHandle::from_shared(Arc::clone(&shared_messenger));

    let node_start_health_timeout = Duration::from_millis(100);
    let node_arguments = MasterNodeArguments {
        node_start_health_timeout,
    };
    let mut master_node = MasterNode::new(
        Arc::clone(&shared_messenger),
        Some("test_master_node"),
        node_arguments,
    );
    let master_node_name = master_node.node_name().to_string();

    // TODO finish: Call the master_node `start()` function that in turn will listen to `listen_for_node_add` and `listen_for_node_start` then test that `listen_for_node_start` times out because the node passed to `listen_for_node_add` does not call `health` in time
}
