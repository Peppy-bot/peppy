#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_success() {}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_invalid_config_fails() {}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_no_launch_cmd_fails() {}
