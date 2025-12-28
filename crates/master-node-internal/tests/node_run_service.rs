mod common;

use common::{CALLER_INSTANCE_ID, setup_test_master_node};
use master_node::encoding::{NodeAddRequest, NodeRunRequest};
use std::path::PathBuf;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_node_run_success() {
    let (client, server) = setup_test_master_node().await;

    // First, add a node with a launch_cmd
    let peppy_json5 = r#"{
        schema_version: 1,
        manifest: {
            name: "runnable_node",
            tag: "1.0.0",
            launch_cmd: ["true"]
        },
        interfaces: {}
    }"#;

    let from_dir = PathBuf::from("/tmp/test");
    let instance_id = "runnable_instance";

    let add_request = NodeAddRequest::new(peppy_json5, from_dir).with_instance_id(instance_id);
    let add_response = add_request
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            Some(&client.instance_id),
            Duration::from_secs(2),
        )
        .await
        .expect("add poll should succeed");

    assert!(add_response.success, "node add should succeed");

    // Verify the node is in the stack
    assert!(
        server.node_stack.contains("runnable_node", "1.0.0"),
        "node should be in the stack"
    );

    // Now run the node with a RuntimeConfig
    let runtime_config_json5 = r#"{
        messaging_host: "127.0.0.1",
        messaging_port: 7447,
        node_name: "runnable_node",
        bound_master_node: "test_master_node",
        deployment_instance: {
            instance_id: "runnable_instance"
        },
        codegen_peppy_config_md5: "d41d8cd98f00b204e9800998ecf8427e"
    }"#;

    let run_request = NodeRunRequest::new(runtime_config_json5);
    let run_response = run_request
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            Some(&client.instance_id),
            Duration::from_secs(2),
        )
        .await
        .expect("run poll should succeed");

    assert!(
        run_response.success,
        "node run should succeed, got error: {:?}",
        run_response.error_message
    );
    assert!(
        run_response.error_message.is_none(),
        "error_message should be None on success"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_node_run_not_found() {
    let (client, _server) = setup_test_master_node().await;

    // Try to run a node that doesn't exist in the stack
    let runtime_config_json5 = r#"{
        messaging_host: "127.0.0.1",
        messaging_port: 7447,
        node_name: "nonexistent_node",
        bound_master_node: "test_master_node",
        deployment_instance: {
            instance_id: "nonexistent_instance"
        },
        codegen_peppy_config_md5: "d41d8cd98f00b204e9800998ecf8427e"
    }"#;

    let run_request = NodeRunRequest::new(runtime_config_json5);
    let run_response = run_request
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            Some(&client.instance_id),
            Duration::from_secs(2),
        )
        .await
        .expect("run poll should succeed");

    assert!(
        !run_response.success,
        "node run should fail for non-existent node"
    );
    let error_message = run_response
        .error_message
        .expect("error_message should be present on failure");
    assert!(
        error_message.contains("not found"),
        "error should indicate node not found, got: {}",
        error_message
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_node_run_invalid_config() {
    let (client, _server) = setup_test_master_node().await;

    // Try to run with invalid JSON5
    let invalid_runtime_config = "invalid json5 {{{";

    let run_request = NodeRunRequest::new(invalid_runtime_config);
    let run_response = run_request
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            Some(&client.instance_id),
            Duration::from_secs(2),
        )
        .await
        .expect("run poll should succeed");

    assert!(
        !run_response.success,
        "node run should fail for invalid config"
    );
    let error_message = run_response
        .error_message
        .expect("error_message should be present on failure");
    assert!(
        error_message.contains("Failed to parse"),
        "error should indicate parsing failure, got: {}",
        error_message
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_node_run_no_launch_cmd() {
    let (client, server) = setup_test_master_node().await;

    // Add a node WITHOUT a launch_cmd
    let peppy_json5 = r#"{
        schema_version: 1,
        manifest: {
            name: "no_cmd_node",
            tag: "1.0.0"
        },
        interfaces: {}
    }"#;

    let from_dir = PathBuf::from("/tmp/test");
    let instance_id = "no_cmd_instance";

    let add_request = NodeAddRequest::new(peppy_json5, from_dir).with_instance_id(instance_id);
    let add_response = add_request
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            Some(&client.instance_id),
            Duration::from_secs(2),
        )
        .await
        .expect("add poll should succeed");

    assert!(add_response.success, "node add should succeed");

    // Verify the node is in the stack
    assert!(
        server.node_stack.contains("no_cmd_node", "1.0.0"),
        "node should be in the stack"
    );

    // Try to run the node - should fail because no launch_cmd
    let runtime_config_json5 = r#"{
        messaging_host: "127.0.0.1",
        messaging_port: 7447,
        node_name: "no_cmd_node",
        bound_master_node: "test_master_node",
        deployment_instance: {
            instance_id: "no_cmd_instance"
        },
        codegen_peppy_config_md5: "d41d8cd98f00b204e9800998ecf8427e"
    }"#;

    let run_request = NodeRunRequest::new(runtime_config_json5);
    let run_response = run_request
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            Some(&client.instance_id),
            Duration::from_secs(2),
        )
        .await
        .expect("run poll should succeed");

    assert!(
        !run_response.success,
        "node run should fail when no launch_cmd configured"
    );
    let error_message = run_response
        .error_message
        .expect("error_message should be present on failure");
    assert!(
        error_message.contains("launch_cmd") || error_message.contains("No launch_cmd"),
        "error should mention launch_cmd, got: {}",
        error_message
    );
}
