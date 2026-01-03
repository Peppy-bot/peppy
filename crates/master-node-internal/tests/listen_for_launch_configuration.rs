mod common;

use common::{CALLER_INSTANCE_ID, start_master_node};
use config::consts::NODE_CONFIG_FILE;
use master_node::encoding::{LauncherRequest, NodeInitRequest};
use peppylib::messaging::MessengerHandle;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::tempdir;

async fn create_test_node(
    caller_handle: &MessengerHandle,
    master_node_name: &str,
    nodes_directory: &Path,
    node_subdir: &str,
    peppy_json5: &str,
) -> PathBuf {
    let node_dir = nodes_directory.join(node_subdir);
    let init_response = NodeInitRequest::new(nodes_directory, node_subdir)
        .poll(
            caller_handle,
            master_node_name,
            CALLER_INSTANCE_ID,
            master_node_name,
            Duration::from_secs(10),
        )
        .await
        .expect("node_init request should complete");
    assert!(
        init_response.success,
        "node_init should succeed, got error: {}",
        init_response.error_message
    );

    let node_config_path = node_dir.join(NODE_CONFIG_FILE);
    fs::write(&node_config_path, peppy_json5).expect("failed to write node config");
    node_config_path
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_succeed() {
    const TARGET_NODE_NAME: &str = "example_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const TARGET_INSTANCE_ID: &str = "example_instance";

    let started_master = start_master_node().await;
    let node_stack = started_master.node_stack.clone();

    let nodes_dir = tempdir().expect("failed to create temp nodes directory");
    let nodes_dir = nodes_dir.path();
    create_test_node(
        &started_master.caller_handle,
        &started_master.master_node_name,
        nodes_dir,
        TARGET_NODE_NAME,
        &format!(
            r#"{{
                schema_version: 1,
                manifest: {{
                    name: "{TARGET_NODE_NAME}",
                    tag: "{TARGET_NODE_TAG}",
                    launch_cmd: ["./mock_node_script.sh"]
                }}
            }}"#
        ),
    )
    .await;

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

    let response = LauncherRequest::new(launcher_json5, nodes_dir)
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
    assert_eq!(entity.instances().len(), 1);
    assert_eq!(
        entity.instances()[0].instance_id().as_str(),
        TARGET_INSTANCE_ID
    );

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_launch_config_invalid_json5_returns_error_and_does_not_mutate_stack()
 {
    todo!("Finish")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_launch_config_nodes_directory_must_be_a_directory() {
    todo!("Finish")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_config_missing_required_deployment_does_not_apply_partial_plan() {
    todo!("Finish")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_launch_config_dependency_errors_are_rejected() {
    todo!("Finish")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_launch_config_second_request_replaces_existing_stack() {
    todo!("Finish")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_runs_generate_before_launch() {
    todo!("Finish")
}
