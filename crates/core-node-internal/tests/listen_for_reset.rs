mod common;

use common::{
    CALLER_INSTANCE_ID, build_staged_node, send_node_add_and_wait, spawn_real_running_instance,
    start_core_node_with_mock_messenger, write_peppy_json5,
};
use config::node::Name;
use core_node_api::encoding::NodeResetRequest;
use peppylib::core_node::transport::poll_node_reset;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_reset_clears_node_stack() {
    const TARGET_NODE_A_NAME: &str = "resettable_node_a";
    const TARGET_NODE_A_TAG: &str = "0.1.0";
    const TARGET_NODE_A_INSTANCE_ID: &str = "resettable_instance_a";

    const TARGET_NODE_B_NAME: &str = "resettable_node_b";
    const TARGET_NODE_B_TAG: &str = "0.2.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();
    let root_instance_id_before = node_stack
        .root()
        .read()
        .instances()
        .first()
        .expect("root should have exactly one instance")
        .instance_id()
        .as_str()
        .to_owned();

    let source_dir_a = tempfile::tempdir().expect("failed to create temp source dir");
    let source_dir_b = tempfile::tempdir().expect("failed to create temp source dir");

    let peppy_json5_a = r#"{
            peppy_schema: "nodes_v1",
            manifest: {
                name: "{TARGET_NODE_A_NAME}",
                tag: "{TARGET_NODE_A_TAG}",
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_A_NAME}", TARGET_NODE_A_NAME)
    .replace("{TARGET_NODE_A_TAG}", TARGET_NODE_A_TAG);
    write_peppy_json5(source_dir_a.path(), &peppy_json5_a);

    let add_response_a = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir_a.path(),
        Duration::from_secs(5),
        Duration::from_secs(5),
        None,
    )
    .await
    .expect("node_add should complete");

    assert!(
        add_response_a.success,
        "node_add should succeed, got error: {:?}",
        add_response_a.error_message
    );

    let peppy_json5_b = r#"{
            peppy_schema: "nodes_v1",
            manifest: {
                name: "{TARGET_NODE_B_NAME}",
                tag: "{TARGET_NODE_B_TAG}",
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_B_NAME}", TARGET_NODE_B_NAME)
    .replace("{TARGET_NODE_B_TAG}", TARGET_NODE_B_TAG);
    write_peppy_json5(source_dir_b.path(), &peppy_json5_b);

    let add_response_b = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir_b.path(),
        Duration::from_secs(5),
        Duration::from_secs(5),
        None,
    )
    .await
    .expect("node_add should complete");

    assert!(
        add_response_b.success,
        "node_add should succeed, got error: {:?}",
        add_response_b.error_message
    );

    assert!(node_stack.contains(TARGET_NODE_A_NAME, TARGET_NODE_A_TAG));
    assert!(node_stack.contains(TARGET_NODE_B_NAME, TARGET_NODE_B_TAG));
    assert_eq!(node_stack.len(), 3, "root + two added nodes");
    build_staged_node(&started_core_node, TARGET_NODE_A_NAME, TARGET_NODE_A_TAG).await;
    build_staged_node(&started_core_node, TARGET_NODE_B_NAME, TARGET_NODE_B_TAG).await;

    let instance_id_a = Name::new(TARGET_NODE_A_INSTANCE_ID).expect("valid instance id");
    let _running_a = spawn_real_running_instance(
        &started_core_node,
        TARGET_NODE_A_NAME,
        TARGET_NODE_A_TAG,
        &instance_id_a,
    )
    .await;
    let entity_a = node_stack
        .find(TARGET_NODE_A_NAME, TARGET_NODE_A_TAG)
        .expect("node A should exist in stack");
    assert_eq!(
        entity_a.read().instances().len(),
        1,
        "node A should have one instance"
    );

    let reset_response = poll_node_reset(
        &NodeResetRequest::new(),
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        CALLER_INSTANCE_ID,
        &started_core_node.core_node_name,
        Duration::from_secs(5),
    )
    .await
    .expect("node_reset request should complete");

    assert!(
        reset_response.success,
        "node_reset should succeed, got error: {:?}",
        reset_response.error_message
    );
    assert!(
        reset_response.error_message.is_none(),
        "success response should not include error_message, got: {:?}",
        reset_response.error_message
    );

    assert_eq!(node_stack.len(), 1, "only root should remain");
    assert!(
        !node_stack.contains(TARGET_NODE_A_NAME, TARGET_NODE_A_TAG),
        "node A should be removed from node stack"
    );
    assert!(
        !node_stack.contains(TARGET_NODE_B_NAME, TARGET_NODE_B_TAG),
        "node B should be removed from node stack"
    );

    let root_after = node_stack.root();
    let root_guard = root_after.read();
    assert_eq!(
        root_guard.config().manifest.name.as_str(),
        started_core_node.core_node_name,
        "root node name should be preserved"
    );
    assert_eq!(
        root_guard.config().manifest.tag,
        started_core_node.core_node_tag,
        "root node tag should be preserved"
    );
    let root_instance_id_after = root_guard
        .instances()
        .first()
        .expect("root should have exactly one instance")
        .instance_id()
        .as_str()
        .to_owned();
    drop(root_guard);
    assert_eq!(
        root_instance_id_after, root_instance_id_before,
        "root instance id should be preserved"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_reset_is_idempotent() {
    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();
    assert_eq!(node_stack.len(), 1, "only root should exist initially");

    let root_instance_id_before = node_stack
        .root()
        .read()
        .instances()
        .first()
        .expect("root should have exactly one instance")
        .instance_id()
        .as_str()
        .to_owned();

    let response = poll_node_reset(
        &NodeResetRequest::new(),
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        CALLER_INSTANCE_ID,
        &started_core_node.core_node_name,
        Duration::from_secs(5),
    )
    .await
    .expect("node_reset request should complete");

    assert!(response.success, "node_reset should succeed");
    assert_eq!(node_stack.len(), 1, "only root should remain after reset");

    let root_instance_id_after = node_stack
        .root()
        .read()
        .instances()
        .first()
        .expect("root should have exactly one instance")
        .instance_id()
        .as_str()
        .to_owned();
    assert_eq!(
        root_instance_id_after, root_instance_id_before,
        "root instance id should be preserved"
    );
}
