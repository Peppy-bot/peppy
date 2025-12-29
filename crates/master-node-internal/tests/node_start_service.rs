mod common;

use common::{CALLER_INSTANCE_ID, setup_test_master_node, setup_test_master_node_with_timeout};
use master_node::encoding::{NodeAddRequest, NodeStartRequest};
use peppylib::messaging::MessengerHandle;
use peppylib::services::health::listen_for_node_health;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_start_success() {
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
    let node_name = "runnable_node";
    let bound_master_node = "test_master_node";

    let add_request = NodeAddRequest::new(peppy_json5, from_dir).with_instance_id(instance_id);
    let add_response = add_request
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
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

    // Set up a mock health service listener to simulate the spawned node being ready.
    // In real usage, the spawned process would expose this service, but in tests with
    // launch_cmd: ["true"], the process exits immediately. This mock simulates the node
    // responding to health checks.
    let health_handle = MessengerHandle::from_shared(Arc::clone(&server.shared_messenger));
    let _health_task =
        listen_for_node_health(&health_handle, bound_master_node, instance_id, node_name)
            .await
            .expect("failed to start mock health service");

    // Allow the health service to fully establish its listeners
    tokio::time::sleep(Duration::from_millis(50)).await;

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

    let run_request = NodeStartRequest::new(runtime_config_json5);
    let run_response = run_request
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
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
async fn node_start_not_found() {
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

    let run_request = NodeStartRequest::new(runtime_config_json5);
    let run_response = run_request
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
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
async fn node_start_invalid_config() {
    let (client, _server) = setup_test_master_node().await;

    // Try to run with invalid JSON5
    let invalid_runtime_config = "invalid json5 {{{";

    let run_request = NodeStartRequest::new(invalid_runtime_config);
    let run_response = run_request
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
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

// TODO: Probably a good idea to reject the node if there is no `launch_cmd` instead of accepting it into the Node stack and then preventing it from running
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_start_no_launch_cmd() {
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

    let run_request = NodeStartRequest::new(runtime_config_json5);
    let run_response = run_request
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_start_health_check_timeout() {
    // Use a short health check timeout for testing (1 second instead of 15)
    let (client, server) = setup_test_master_node_with_timeout(Duration::from_secs(1)).await;

    // Add a node with a launch_cmd
    let peppy_json5 = r#"{
        schema_version: 1,
        manifest: {
            name: "unhealthy_node",
            tag: "1.0.0",
            launch_cmd: ["sleep", "10"]
        },
        interfaces: {}
    }"#;

    let from_dir = PathBuf::from("/tmp/test");
    let instance_id = "unhealthy_instance";

    let add_request = NodeAddRequest::new(peppy_json5, from_dir).with_instance_id(instance_id);
    let add_response = add_request
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            Duration::from_secs(2),
        )
        .await
        .expect("add poll should succeed");

    assert!(add_response.success, "node add should succeed");

    // Verify the node is in the stack
    assert!(
        server.node_stack.contains("unhealthy_node", "1.0.0"),
        "node should be in the stack"
    );

    // NOTE: We intentionally do NOT set up a mock health service listener.
    // This simulates a node that starts but never becomes healthy (doesn't expose health service).

    // Try to start the node - should fail because health check times out
    let runtime_config_json5 = r#"{
        messaging_host: "127.0.0.1",
        messaging_port: 7447,
        node_name: "unhealthy_node",
        bound_master_node: "test_master_node",
        deployment_instance: {
            instance_id: "unhealthy_instance"
        },
        codegen_peppy_config_md5: "d41d8cd98f00b204e9800998ecf8427e"
    }"#;

    let run_request = NodeStartRequest::new(runtime_config_json5);
    let run_response = run_request
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            Duration::from_secs(5), // Give enough time for health check to timeout
        )
        .await
        .expect("run poll should succeed");

    assert!(
        !run_response.success,
        "node run should fail when health check times out"
    );
    let error_message = run_response
        .error_message
        .expect("error_message should be present on failure");
    assert!(
        error_message.contains("Health check failed") || error_message.contains("health"),
        "error should mention health check failure, got: {}",
        error_message
    );
}
