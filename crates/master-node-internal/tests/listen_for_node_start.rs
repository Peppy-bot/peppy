mod common;

use common::{start_master_node, start_master_node_with_timeout, CALLER_INSTANCE_ID};
use config::peppy_config::{DeploymentInstance, Name};
use config::runtime::RuntimeConfig;
use master_node::encoding::{NodeAddRequest, NodeStartRequest};
use peppylib::messaging::MessengerHandle;
use peppylib::services::health::listen_for_node_health;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_start_success() {
    const TARGET_NODE_NAME: &str = "runnable_node";
    const TARGET_INSTANCE_ID: &str = "runnable_instance";

    let started_master = start_master_node().await;

    // Create a node config with a launch_cmd that runs but doesn't respond to health checks on its own
    // We'll set up a separate health listener to respond on behalf of the "node"
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

    // Add the node to the master node's node stack
    let node_add_request =
        NodeAddRequest::new(&peppy_json5, "/tmp").with_instance_id(TARGET_INSTANCE_ID);
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
    assert_eq!(add_response.node_instance_id, TARGET_INSTANCE_ID);

    // Set up a health listener that will respond to health check requests
    // This simulates the node responding to health checks
    let health_handle = MessengerHandle::from_shared(Arc::clone(&started_master.shared_messenger));
    let health_task = listen_for_node_health(
        &health_handle,
        &started_master.master_node_name,
        TARGET_INSTANCE_ID,
        TARGET_NODE_NAME,
    )
    .await
    .expect("failed to start health service");

    // Allow the health service to fully establish its listener
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
        &started_master.master_node_name,
        "d41d8cd98f00b204e9800998ecf8427e", // dummy md5
    )
    .expect("runtime config should be valid");

    let runtime_config_json5 =
        serde_json5::to_string(&runtime_config).expect("runtime config should serialize");

    // Call node_start - this should succeed because the health listener will respond
    let node_start_request = NodeStartRequest::new(&runtime_config_json5);
    let start_response = node_start_request
        .poll(
            &started_master.caller_handle,
            &started_master.master_node_name,
            CALLER_INSTANCE_ID,
            &started_master.master_node_name,
            Duration::from_secs(10),
        )
        .await
        .expect("node_start request should complete");

    // The start should succeed because the health check was responded to
    assert!(
        start_response.success,
        "node_start should succeed, got error: {:?}",
        start_response.error_message
    );

    // Clean up
    health_task.abort();
    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_start_timeout() {
    const TARGET_NODE_NAME: &str = "runnable_node";
    const TARGET_INSTANCE_ID: &str = "runnable_instance";

    let started = start_master_node_with_timeout(Duration::from_millis(100)).await;

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

    // Add the node to the master node's node stack
    let node_add_request =
        NodeAddRequest::new(&peppy_json5, "/tmp").with_instance_id(TARGET_INSTANCE_ID);
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
    assert_eq!(add_response.node_instance_id, TARGET_INSTANCE_ID);

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
        "d41d8cd98f00b204e9800998ecf8427e", // dummy md5
    )
    .expect("runtime config should be valid");

    let runtime_config_json5 =
        serde_json5::to_string(&runtime_config).expect("runtime config should serialize");

    // Call node_start - this should timeout because the node won't respond to health checks
    let node_start_request = NodeStartRequest::new(&runtime_config_json5);
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
            .map(|msg| msg.contains("Health check failed"))
            .unwrap_or(false),
        "error message should indicate health check failure, got: {:?}",
        start_response.error_message
    );

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
        "d41d8cd98f00b204e9800998ecf8427e", // dummy md5
    )
    .expect("runtime config should be valid");

    let runtime_config_json5 =
        serde_json5::to_string(&runtime_config).expect("runtime config should serialize");

    // Call node_start - this should fail because the instance_id doesn't exist in the node stack
    let node_start_request = NodeStartRequest::new(&runtime_config_json5);
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

    // The start should fail because the node instance was not found
    assert!(
        !start_response.success,
        "node_start should fail because instance_id not found"
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
