mod common;

use common::{CALLER_INSTANCE_ID, setup_test_master_node};
use master_node::encoding::{NodeAddRequest, NodeStartRequest};
use std::path::PathBuf;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_node_stop_success() {
    // TODO finish
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_node_stop_not_found() {
    // TODO finish
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_node_stop_invalid_instance_id() {
    // TODO finish
}
