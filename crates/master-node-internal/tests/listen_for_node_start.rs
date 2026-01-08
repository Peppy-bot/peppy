mod common;

use common::{
    CALLER_INSTANCE_ID, create_test_node_with_name, start_master_node,
    start_master_node_with_health_timeout, start_master_node_with_zenoh_messenger,
};
use config::consts::NODE_CONFIG_FILE;
use config::node::Name as NodeName;
use config::peppy_config::{DeploymentInstance, Name};
use config::runtime::RuntimeConfig;
use master_node::encoding::{NodeAddRequest, NodeStartRequest};
use peppylib::messaging::MessengerHandle;
use peppylib::services::ready::listen_for_node_ready;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

/// Creates a temp directory with a peppy.json5 file
fn create_node_config_dir(peppy_json5: &str) -> TempDir {
    let temp_dir = TempDir::new().expect("failed to create temp directory");
    let config_path = temp_dir.path().join(NODE_CONFIG_FILE);
    std::fs::write(&config_path, peppy_json5).expect("failed to write peppy.json5");
    temp_dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_start_success() {
    // These must match the values used in create_test_node()
    const TARGET_NODE_NAME: &str = "runnable_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const TARGET_INSTANCE_ID: &str = "runnable_instance";

    let started_master = start_master_node_with_zenoh_messenger().await;

    // Use a pre-built test node to avoid compilation delays during the test
    let node_dir = create_test_node_with_name(TARGET_NODE_NAME, TARGET_NODE_TAG);

    // Add the node to the master node's node stack
    let node_add_request = NodeAddRequest::new(&node_dir).with_instance_id(TARGET_INSTANCE_ID);
    let add_response = node_add_request
        .poll(
            &started_master.caller_handle,
            &started_master.master_node_name,
            CALLER_INSTANCE_ID,
            &started_master.master_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_add request should succeed");

    assert!(
        add_response.success,
        "node_add should succeed, got error: {:?}",
        add_response.error_message
    );
    // Leave aside for debugging
    let _snapshot_node_path = add_response.snapshot_path;

    // Get the actual messaging endpoint from the Zenoh session
    let (messaging_host, messaging_port) = started_master
        .caller_handle
        .messaging_endpoint()
        .await
        .expect("zenoh endpoint should be available");

    // Create a runtime config for the node_start request
    let runtime_config = RuntimeConfig::new(
        messaging_host.as_str(),
        messaging_port,
        DeploymentInstance {
            instance_id: Name::new(TARGET_INSTANCE_ID).unwrap(),
            arguments: Default::default(),
        },
        TARGET_NODE_NAME,
        &started_master.master_node_name,
    )
    .expect("runtime config should be valid");

    let runtime_config_json5 =
        serde_json5::to_string(&runtime_config).expect("runtime config should serialize");

    let node_start_request =
        NodeStartRequest::new(&runtime_config_json5, TARGET_NODE_NAME, TARGET_NODE_TAG);
    let start_response = node_start_request
        .poll(
            &started_master.caller_handle,
            &started_master.master_node_name,
            CALLER_INSTANCE_ID,
            &started_master.master_node_name,
            Duration::from_secs(60),
        )
        .await
        .expect("node_start request should complete");

    // The start should succeed because the health check was responded to
    assert!(
        start_response.success,
        "node_start should succeed, got error: {:?}",
        start_response.error_message
    );

    // Verify that the instance was added to the node stack
    let instance_id = NodeName::new(TARGET_INSTANCE_ID).expect("valid instance id");
    let found_instance = started_master.node_stack.find_by_instance_id(&instance_id);
    assert!(
        found_instance.is_some(),
        "instance should be registered in the node stack after successful start"
    );

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_start_timeout() {
    const TARGET_NODE_NAME: &str = "runnable_node";
    const TARGET_INSTANCE_ID: &str = "runnable_instance";

    // Use a short health timeout so the test doesn't take too long
    let started = start_master_node_with_health_timeout(Duration::from_secs(2)).await;

    // Create a node config with a launch_cmd that won't respond to health checks
    // Using "sleep 10" as a simple command that runs but doesn't respond
    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{}",
                tag: "0.1.0",
                launch_cmd: ["sleep", "10"]
            }},
            parameters: {{}}
        }}"#,
        TARGET_NODE_NAME
    );

    // Create temp directory with peppy.json5
    let temp_dir = create_node_config_dir(&peppy_json5);

    // Add the node to the master node's node stack
    let node_add_request =
        NodeAddRequest::new(temp_dir.path()).with_instance_id(TARGET_INSTANCE_ID);
    let add_response = node_add_request
        .poll(
            &started.caller_handle,
            &started.master_node_name,
            CALLER_INSTANCE_ID,
            &started.master_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_add request should succeed");

    assert!(
        add_response.success,
        "node_add should succeed, got error: {:?}",
        add_response.error_message
    );

    // Set up a ready listener so node_start can proceed to the health-check phase.
    // We intentionally do NOT set up a health listener to force the health check to time out.
    let ready_handle = MessengerHandle::from_shared(Arc::clone(&started.shared_messenger));
    let ready_task = listen_for_node_ready(
        &ready_handle,
        &started.master_node_name,
        TARGET_INSTANCE_ID,
        TARGET_NODE_NAME,
    )
    .await
    .expect("failed to start ready service");

    // Allow the ready service to fully establish its listener
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Create a runtime config for the node_start request
    let runtime_config = RuntimeConfig::new(
        "127.0.0.1",
        7448,
        DeploymentInstance {
            instance_id: Name::new(TARGET_INSTANCE_ID).unwrap(),
            arguments: Default::default(),
        },
        TARGET_NODE_NAME,
        &started.master_node_name,
    )
    .expect("runtime config should be valid");

    let runtime_config_json5 =
        serde_json5::to_string(&runtime_config).expect("runtime config should serialize");

    // Call node_start - this should timeout because the node won't respond to health checks
    let node_start_request =
        NodeStartRequest::new(&runtime_config_json5, TARGET_NODE_NAME, "0.1.0");
    let start_response = node_start_request
        .poll(
            &started.caller_handle,
            &started.master_node_name,
            CALLER_INSTANCE_ID,
            &started.master_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_start request should complete");

    // The start should fail because the health check timed out
    assert!(
        !start_response.success,
        "node_start should fail due to health check timeout"
    );
    assert!(
        start_response
            .error_message
            .as_ref()
            .map(|msg| msg.contains("health check timed out"))
            .unwrap_or(false),
        "error message should indicate health check failure, got: {:?}",
        start_response.error_message
    );

    // Verify that the instance was NOT added to the node stack since start failed
    let instance_id = NodeName::new(TARGET_INSTANCE_ID).expect("valid instance id");
    let found_instance = started.node_stack.find_by_instance_id(&instance_id);
    assert!(
        found_instance.is_none(),
        "instance should NOT be registered in the node stack after failed start"
    );

    // Clean up
    ready_task.abort();

    // Abort the master node task
    started.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_start_not_found() {
    const TARGET_NODE_NAME: &str = "nonexistent_node";
    const TARGET_INSTANCE_ID: &str = "nonexistent_instance";

    let started = start_master_node().await;

    // Note: We intentionally do NOT add any node to the node stack
    // This simulates trying to start a node that doesn't exist

    // Create a runtime config for a node that was never added
    let runtime_config = RuntimeConfig::new(
        "127.0.0.1",
        7448,
        DeploymentInstance {
            instance_id: Name::new(TARGET_INSTANCE_ID).unwrap(),
            arguments: Default::default(),
        },
        TARGET_NODE_NAME,
        &started.master_node_name,
    )
    .expect("runtime config should be valid");

    let runtime_config_json5 =
        serde_json5::to_string(&runtime_config).expect("runtime config should serialize");

    // Call node_start - this should fail because the node doesn't exist in the node stack
    let node_start_request =
        NodeStartRequest::new(&runtime_config_json5, TARGET_NODE_NAME, "0.1.0");
    let start_response = node_start_request
        .poll(
            &started.caller_handle,
            &started.master_node_name,
            CALLER_INSTANCE_ID,
            &started.master_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_start request should complete");

    // The start should fail because the node was not found
    assert!(
        !start_response.success,
        "node_start should fail because node not found"
    );
    assert!(
        start_response
            .error_message
            .as_ref()
            .map(|msg| msg.contains("not found in node stack"))
            .unwrap_or(false),
        "error message should indicate node not found, got: {:?}",
        start_response.error_message
    );

    // Abort the master node task
    started.task.abort();
}
