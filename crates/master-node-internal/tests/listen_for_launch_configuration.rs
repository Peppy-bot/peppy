mod common;

use common::{CALLER_INSTANCE_ID, create_test_node, start_master_node};
use master_node::encoding::LauncherRequest;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_succeed() {
    const TARGET_NODE_NAME: &str = "example_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const TARGET_INSTANCE_ID: &str = "example_instance";

    let started_master = start_master_node().await;
    let node_stack = started_master.node_stack.clone();

    let nodes_dir = create_test_node().join(TARGET_NODE_TAG);

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

    let response = LauncherRequest::new(launcher_json5, &nodes_dir)
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
