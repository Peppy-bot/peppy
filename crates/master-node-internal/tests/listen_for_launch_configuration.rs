mod common;

use common::{CALLER_INSTANCE_ID, start_master_node_with_zenoh_messenger};
use config::consts::{NODE_CONFIG_FILE, PEPPYGEN_OUTPUT_PATH};
use config::runtime::LauncherRuntimeConfig;
use master_node::encoding::LauncherRequest;
use peppylib::messaging::MessengerHandle;
use std::fs;
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_succeed() {
    const TARGET_NODE_NAME: &str = "example_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const TARGET_INSTANCE_ID: &str = "example_instance";

    let started_master = start_master_node_with_zenoh_messenger().await;
    let node_stack = started_master.node_stack.clone();

    let nodes_dir = common::create_test_node();

    let launcher_json5 = format!(
        r#"{{
            deployments: [
                {{
                    name: "{TARGET_NODE_NAME}",
                    tag: "{TARGET_NODE_TAG}",
                    instances: [{{ instance_id: "{TARGET_INSTANCE_ID}" }}]
                }}
            ]
        }}"#
    );

    let (messaging_host, messaging_port) = started_master
        .caller_handle
        .messaging_endpoint()
        .await
        .expect("zenoh endpoint should be available for launcher test");
    let launcher_runtime_config = LauncherRuntimeConfig::new(messaging_host, messaging_port);
    let launcher_runtime_config_json =
        serde_json::to_string(&launcher_runtime_config).expect("serialize runtime config");

    let response = LauncherRequest::new(launcher_json5, nodes_dir, launcher_runtime_config_json)
        .poll(
            &started_master.caller_handle,
            &started_master.master_node_name,
            CALLER_INSTANCE_ID,
            &started_master.master_node_name,
            None,
            Duration::from_secs(60),
        )
        .await
        .expect("launcher request should complete");

    assert!(
        response.success,
        "launcher request should succeed, got error: {}",
        response.error_message
    );

    assert!(node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG));
    assert_eq!(node_stack.len(), 2, "root + deployed node");

    let entity = node_stack
        .find(TARGET_NODE_NAME, TARGET_NODE_TAG)
        .expect("deployed node should exist in stack");

    // One instance as specified in the launcher config
    assert_eq!(entity.instances().len(), 1);
    assert_eq!(
        entity.instances()[0].instance_id().as_str(),
        TARGET_INSTANCE_ID
    );

    // Total instances across the stack: master node instance + deployed node instance
    let total_instances: usize = node_stack
        .snapshot()
        .iter()
        .map(|e| e.instances().len())
        .sum();
    assert_eq!(total_instances, 2);

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_two_instances_succeed() {
    const TARGET_NODE_NAME: &str = "example_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const TARGET_INSTANCE_ID: &str = "example_instance1";
    const TARGET_INSTANCE_ID2: &str = "example_instance2";

    let started_master = start_master_node_with_zenoh_messenger().await;
    let node_stack = started_master.node_stack.clone();

    let nodes_dir = common::create_test_node();

    let launcher_json5 = format!(
        r#"{{
            deployments: [
                {{
                    name: "{TARGET_NODE_NAME}",
                    tag: "{TARGET_NODE_TAG}",
                    instances: [
                      {{ instance_id: "{TARGET_INSTANCE_ID}" }},
                       {{ instance_id: "{TARGET_INSTANCE_ID2}" }}
                    ]
                }}
            ]
        }}"#
    );

    let (messaging_host, messaging_port) = started_master
        .caller_handle
        .messaging_endpoint()
        .await
        .expect("zenoh endpoint should be available for launcher test");
    let launcher_runtime_config = LauncherRuntimeConfig::new(messaging_host, messaging_port);
    let launcher_runtime_config_json =
        serde_json::to_string(&launcher_runtime_config).expect("serialize runtime config");

    let response = LauncherRequest::new(launcher_json5, nodes_dir, launcher_runtime_config_json)
        .poll(
            &started_master.caller_handle,
            &started_master.master_node_name,
            CALLER_INSTANCE_ID,
            &started_master.master_node_name,
            None,
            Duration::from_secs(60),
        )
        .await
        .expect("launcher request should complete");

    assert!(
        response.success,
        "launcher request should succeed, got error: {}",
        response.error_message
    );

    assert!(node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG));
    assert_eq!(node_stack.len(), 2, "root + deployed node");

    let entity = node_stack
        .find(TARGET_NODE_NAME, TARGET_NODE_TAG)
        .expect("deployed node should exist in stack");

    // Two instances as specified in the launcher config
    assert_eq!(entity.instances().len(), 2);
    let instance_ids: Vec<&str> = entity
        .instances()
        .iter()
        .map(|instance| instance.instance_id().as_str())
        .collect();
    assert!(instance_ids.contains(&TARGET_INSTANCE_ID));
    assert!(instance_ids.contains(&TARGET_INSTANCE_ID2));

    // Total instances across the stack: master node instance + deployed node instances
    let total_instances: usize = node_stack
        .snapshot()
        .iter()
        .map(|e| e.instances().len())
        .sum();
    assert_eq!(total_instances, 3);

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_launch_config_invalid_json5_returns_error_and_does_not_mutate_stack()
 {
    let started_master = start_master_node_with_zenoh_messenger().await;
    let node_stack = started_master.node_stack.clone();

    let nodes_dir = common::create_test_node();
    let launcher_json5 = r#"{ deployments: [ { "#;

    let (messaging_host, messaging_port) = started_master
        .caller_handle
        .messaging_endpoint()
        .await
        .expect("zenoh endpoint should be available for launcher test");
    let launcher_runtime_config = LauncherRuntimeConfig::new(messaging_host, messaging_port);
    let launcher_runtime_config_json =
        serde_json::to_string(&launcher_runtime_config).expect("serialize runtime config");

    let response = LauncherRequest::new(launcher_json5, nodes_dir, launcher_runtime_config_json)
        .poll(
            &started_master.caller_handle,
            &started_master.master_node_name,
            CALLER_INSTANCE_ID,
            &started_master.master_node_name,
            None,
            Duration::from_secs(60),
        )
        .await
        .expect("launcher request should complete");

    assert!(
        !response.success,
        "launcher request should fail for invalid json5, got error: {}",
        response.error_message
    );
    assert!(
        response
            .error_message
            .contains("invalid peppy_launcher_json5"),
        "error message should indicate parse failure, got: {}",
        response.error_message
    );
    assert_eq!(node_stack.len(), 1, "only root should exist");

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_launch_config_nodes_directory_must_be_a_directory() {
    let started_master = start_master_node_with_zenoh_messenger().await;
    let node_stack = started_master.node_stack.clone();

    let tmp = tempdir().expect("failed to create temp directory");
    let file_path = tmp.path().join("not_a_directory");
    fs::write(&file_path, "not a dir").expect("failed to write temp file");

    let launcher_json5 = r#"{
        deployments: [
            {
                name: "example_node",
                tag: "0.1.0",
                instances: []
            }
        ]
    }"#;

    let (messaging_host, messaging_port) = started_master
        .caller_handle
        .messaging_endpoint()
        .await
        .expect("zenoh endpoint should be available for launcher test");
    let launcher_runtime_config = LauncherRuntimeConfig::new(messaging_host, messaging_port);
    let launcher_runtime_config_json =
        serde_json::to_string(&launcher_runtime_config).expect("serialize runtime config");

    let response = LauncherRequest::new(launcher_json5, file_path, launcher_runtime_config_json)
        .poll(
            &started_master.caller_handle,
            &started_master.master_node_name,
            CALLER_INSTANCE_ID,
            &started_master.master_node_name,
            None,
            Duration::from_secs(60),
        )
        .await
        .expect("launcher request should complete");

    assert!(
        !response.success,
        "launcher request should fail when nodes_directory is not a directory, got error: {}",
        response.error_message
    );
    assert!(
        response
            .error_message
            .contains("nodes_directory is not a directory"),
        "error message should mention not a directory, got: {}",
        response.error_message
    );
    assert_eq!(node_stack.len(), 1, "only root should exist");

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_config_missing_required_deployment_does_not_apply_partial_plan() {
    const TARGET_NODE_NAME: &str = "example_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const TARGET_INSTANCE_ID: &str = "example_instance";
    const MISSING_NODE_NAME: &str = "missing_node";
    const MISSING_NODE_TAG: &str = "0.1.0";
    const MISSING_INSTANCE_ID: &str = "missing_instance";

    let started_master = start_master_node_with_zenoh_messenger().await;
    let node_stack = started_master.node_stack.clone();

    let nodes_dir = common::create_test_node();

    let launcher_json5 = format!(
        r#"{{
            deployments: [
                {{
                    name: "{TARGET_NODE_NAME}",
                    tag: "{TARGET_NODE_TAG}",
                    instances: [{{ instance_id: "{TARGET_INSTANCE_ID}" }}]
                }},
                {{
                    name: "{MISSING_NODE_NAME}",
                    tag: "{MISSING_NODE_TAG}",
                    instances: [{{ instance_id: "{MISSING_INSTANCE_ID}" }}]
                }}
            ]
        }}"#
    );

    let (messaging_host, messaging_port) = started_master
        .caller_handle
        .messaging_endpoint()
        .await
        .expect("zenoh endpoint should be available for launcher test");
    let launcher_runtime_config = LauncherRuntimeConfig::new(messaging_host, messaging_port);
    let launcher_runtime_config_json =
        serde_json::to_string(&launcher_runtime_config).expect("serialize runtime config");

    let response = LauncherRequest::new(launcher_json5, nodes_dir, launcher_runtime_config_json)
        .poll(
            &started_master.caller_handle,
            &started_master.master_node_name,
            CALLER_INSTANCE_ID,
            &started_master.master_node_name,
            None,
            Duration::from_secs(60),
        )
        .await
        .expect("launcher request should complete");

    assert!(
        !response.success,
        "launcher request should fail when a required deployment is missing, got error: {}",
        response.error_message
    );
    assert!(
        response.error_message.contains(&format!(
            "deployment {MISSING_NODE_NAME}:{MISSING_NODE_TAG} failed"
        )),
        "error message should mention missing deployment, got: {}",
        response.error_message
    );
    assert_eq!(node_stack.len(), 1, "only root should exist");
    assert!(
        !node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG),
        "resolved deployment should not be applied when plan validation fails"
    );

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_launch_config_dependency_errors_are_rejected() {
    const DEPENDANT_NODE_NAME: &str = "consumer_node";
    const DEPENDANT_NODE_TAG: &str = "0.1.0";
    const DEPENDANT_INSTANCE_ID: &str = "consumer_instance";
    const MISSING_NODE_NAME: &str = "provider_node";
    const MISSING_NODE_TAG: &str = "0.1.0";

    let started_master = start_master_node_with_zenoh_messenger().await;
    let node_stack = started_master.node_stack.clone();

    let nodes_dir = tempdir().expect("failed to create temp directory");
    let node_root = nodes_dir.path().join(DEPENDANT_NODE_NAME);
    fs::create_dir_all(&node_root).expect("failed to create node directory");
    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{DEPENDANT_NODE_NAME}",
                tag: "{DEPENDANT_NODE_TAG}",
                launch_cmd: ["sleep", "10"]
            }},
            interfaces: {{
                subscribes_to: {{
                    topics: [
                        {{
                            id: "sensor_input",
                            node: "{MISSING_NODE_NAME}",
                            name: "sensor_data",
                            tag: "{MISSING_NODE_TAG}"
                        }}
                    ]
                }}
            }}
        }}"#
    );
    fs::write(node_root.join(NODE_CONFIG_FILE), peppy_json5)
        .expect("failed to write dependent node config");

    let launcher_json5 = format!(
        r#"{{
            deployments: [
                {{
                    name: "{DEPENDANT_NODE_NAME}",
                    tag: "{DEPENDANT_NODE_TAG}",
                    instances: [{{ instance_id: "{DEPENDANT_INSTANCE_ID}" }}]
                }}
            ]
        }}"#
    );

    let (messaging_host, messaging_port) = started_master
        .caller_handle
        .messaging_endpoint()
        .await
        .expect("zenoh endpoint should be available for launcher test");
    let launcher_runtime_config = LauncherRuntimeConfig::new(messaging_host, messaging_port);
    let launcher_runtime_config_json =
        serde_json::to_string(&launcher_runtime_config).expect("serialize runtime config");

    let response = LauncherRequest::new(
        launcher_json5,
        nodes_dir.path(),
        launcher_runtime_config_json,
    )
    .poll(
        &started_master.caller_handle,
        &started_master.master_node_name,
        CALLER_INSTANCE_ID,
        &started_master.master_node_name,
        None,
        Duration::from_secs(60),
    )
    .await
    .expect("launcher request should complete");

    assert!(
        !response.success,
        "launcher request should fail when dependencies are missing, got error: {}",
        response.error_message
    );
    assert!(
        response
            .error_message
            .contains("does not exist in the stack"),
        "error message should mention missing dependency, got: {}",
        response.error_message
    );
    assert_eq!(node_stack.len(), 1, "only root should exist");
    assert!(
        !node_stack.contains(DEPENDANT_NODE_NAME, DEPENDANT_NODE_TAG),
        "failed launch plan should not be applied to the stack"
    );

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_launch_config_second_request_replaces_existing_stack() {
    const TARGET_NODE_NAME: &str = "example_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const TARGET_INSTANCE_ID: &str = "example_instance1";
    const TARGET_INSTANCE_ID2: &str = "example_instance2";

    let started_master = start_master_node_with_zenoh_messenger().await;
    let node_stack = started_master.node_stack.clone();

    let nodes_dir = common::create_test_node();

    let (messaging_host, messaging_port) = started_master
        .caller_handle
        .messaging_endpoint()
        .await
        .expect("zenoh endpoint should be available for launcher test");
    let launcher_runtime_config = LauncherRuntimeConfig::new(messaging_host, messaging_port);
    let launcher_runtime_config_json =
        serde_json::to_string(&launcher_runtime_config).expect("serialize runtime config");

    let launcher_json5 = format!(
        r#"{{
            deployments: [
                {{
                    name: "{TARGET_NODE_NAME}",
                    tag: "{TARGET_NODE_TAG}",
                    instances: [{{ instance_id: "{TARGET_INSTANCE_ID}" }}]
                }}
            ]
        }}"#
    );

    let response = LauncherRequest::new(
        launcher_json5,
        nodes_dir.clone(),
        launcher_runtime_config_json.clone(),
    )
    .poll(
        &started_master.caller_handle,
        &started_master.master_node_name,
        CALLER_INSTANCE_ID,
        &started_master.master_node_name,
        None,
        Duration::from_secs(60),
    )
    .await
    .expect("launcher request should complete");

    assert!(
        response.success,
        "first launcher request should succeed, got error: {}",
        response.error_message
    );
    let entity = node_stack
        .find(TARGET_NODE_NAME, TARGET_NODE_TAG)
        .expect("deployed node should exist in stack");
    assert_eq!(entity.instances().len(), 1);
    assert_eq!(
        entity.instances()[0].instance_id().as_str(),
        TARGET_INSTANCE_ID
    );

    let launcher_json5 = format!(
        r#"{{
            deployments: [
                {{
                    name: "{TARGET_NODE_NAME}",
                    tag: "{TARGET_NODE_TAG}",
                    instances: [{{ instance_id: "{TARGET_INSTANCE_ID2}" }}]
                }}
            ]
        }}"#
    );

    let response = LauncherRequest::new(launcher_json5, nodes_dir, launcher_runtime_config_json)
        .poll(
            &started_master.caller_handle,
            &started_master.master_node_name,
            CALLER_INSTANCE_ID,
            &started_master.master_node_name,
            None,
            Duration::from_secs(60),
        )
        .await
        .expect("launcher request should complete");

    assert!(
        response.success,
        "second launcher request should succeed, got error: {}",
        response.error_message
    );
    assert!(node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG));
    assert_eq!(node_stack.len(), 2, "root + deployed node");

    let entity = node_stack
        .find(TARGET_NODE_NAME, TARGET_NODE_TAG)
        .expect("deployed node should exist in stack");
    assert_eq!(entity.instances().len(), 1);

    // Check that the name of the instance is TARGET_INSTANCE_ID2
    assert_eq!(
        entity.instances()[0].instance_id().as_str(),
        TARGET_INSTANCE_ID2
    );

    // Total instances across the stack: master node instance + deployed node instance
    let total_instances: usize = node_stack
        .snapshot()
        .iter()
        .map(|e| e.instances().len())
        .sum();
    assert_eq!(total_instances, 2);

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_runs_generate_on_node_before_start() {
    const TARGET_NODE_NAME: &str = "generate_test_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const TARGET_INSTANCE_ID: &str = "generate_test_instance";

    let started_master = start_master_node_with_zenoh_messenger().await;
    let node_stack = started_master.node_stack.clone();

    // Create a node directory with peppy.json5 but WITHOUT running generate
    let nodes_dir = tempdir().expect("failed to create temp directory");
    let node_root = nodes_dir.path().join(TARGET_NODE_NAME);
    fs::create_dir_all(&node_root).expect("failed to create node directory");

    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
                launch_cmd: ["sleep", "1"]
            }}
        }}"#
    );
    fs::write(node_root.join(NODE_CONFIG_FILE), peppy_json5).expect("failed to write node config");

    // Verify peppygen directory does NOT exist before launch
    let peppygen_dir = node_root.join(PEPPYGEN_OUTPUT_PATH);
    assert!(
        !peppygen_dir.exists(),
        "peppygen directory should NOT exist before launch at {}",
        peppygen_dir.display()
    );

    let (messaging_host, messaging_port) = started_master
        .caller_handle
        .messaging_endpoint()
        .await
        .expect("zenoh endpoint should be available for launcher test");
    let launcher_runtime_config = LauncherRuntimeConfig::new(messaging_host, messaging_port);
    let launcher_runtime_config_json =
        serde_json::to_string(&launcher_runtime_config).expect("serialize runtime config");

    let launcher_json5 = format!(
        r#"{{
            deployments: [
                {{
                    name: "{TARGET_NODE_NAME}",
                    tag: "{TARGET_NODE_TAG}",
                    instances: [{{ instance_id: "{TARGET_INSTANCE_ID}" }}]
                }}
            ]
        }}"#
    );

    let response = LauncherRequest::new(
        launcher_json5,
        nodes_dir.path(),
        launcher_runtime_config_json,
    )
    .poll(
        &started_master.caller_handle,
        &started_master.master_node_name,
        CALLER_INSTANCE_ID,
        &started_master.master_node_name,
        None,
        Duration::from_secs(60),
    )
    .await
    .expect("launcher request should complete");

    assert!(
        response.success,
        "launcher request should succeed, got error: {}",
        response.error_message
    );

    // Verify peppygen directory now exists after launch (proving generate was run)
    assert!(
        peppygen_dir.exists(),
        "peppygen directory should exist after launch at {}",
        peppygen_dir.display()
    );

    // Verify the node was deployed
    assert!(node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG));
    let entity = node_stack
        .find(TARGET_NODE_NAME, TARGET_NODE_TAG)
        .expect("deployed node should exist in stack");
    assert_eq!(entity.instances().len(), 1);
    assert_eq!(
        entity.instances()[0].instance_id().as_str(),
        TARGET_INSTANCE_ID
    );

    started_master.task.abort();
}
