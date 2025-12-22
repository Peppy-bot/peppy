mod common;

use common::setup_test_master_node;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_node_run_success() {
    let (_client, _server) = setup_test_master_node().await;

    todo!("Finish")
}
