mod common;

use common::{
    CALLER_INSTANCE_ID, send_node_add_and_wait, start_core_node_with_mock_messenger,
    write_peppy_json5,
};
use core_node_api::encoding::StackListRequest;
use peppylib::core_node::transport::poll;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_list_returns_succeeds() {
    const TARGET_NODE_NAME: &str = "list_node";
    const TARGET_NODE_TAG: &str = "v1";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    let peppy_json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_response = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir.path(),
        Duration::from_secs(5),
        Duration::from_secs(5),
        None,
    )
    .await
    .expect("node_add should complete");

    assert!(
        add_response.success,
        "node_add should succeed, got error: {:?}",
        add_response.error_message
    );
    assert!(node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG));
    assert_eq!(node_stack.len(), 2, "root + added node");

    let response = poll(
        &StackListRequest::new(),
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        CALLER_INSTANCE_ID,
        &started_core_node.core_node_name,
        Duration::from_secs(5),
    )
    .await
    .expect("node_list request should complete");

    let expected_host_name = hostname::get()
        .ok()
        .and_then(|host| host.into_string().ok())
        .unwrap_or_else(|| "unknown".to_string());
    assert_eq!(response.host_name, expected_host_name);

    let graph_json: serde_json::Value =
        serde_json::from_str(&response.graph_json).expect("graph_json should be valid JSON");
    let nodes = graph_json
        .get("nodes")
        .and_then(|nodes| nodes.as_array())
        .expect("graph_json should include a `nodes` array");
    assert!(
        nodes.iter().all(|node| {
            node.get("core_node").and_then(|v| v.as_str())
                == Some(started_core_node.core_node_name.as_str())
        }),
        "every serialized node should carry its owning core-node name, got:\n{}",
        response.graph_json
    );

    let has_root = nodes.iter().any(|node| {
        node.get("name").and_then(|v| v.as_str()) == Some(&started_core_node.core_node_name)
            && node.get("tag").and_then(|v| v.as_str())
                == Some(started_core_node.core_node_tag.as_str())
            && node.get("stage").and_then(|v| v.as_str()) == Some("Root")
    });
    assert!(
        has_root,
        "graph_json should include root node entry with stage 'Root', got:
{}",
        response.graph_json
    );

    let has_added_node = nodes.iter().any(|node| {
        node.get("name").and_then(|v| v.as_str()) == Some(TARGET_NODE_NAME)
            && node.get("tag").and_then(|v| v.as_str()) == Some(TARGET_NODE_TAG)
            && node.get("stage").and_then(|v| v.as_str()) == Some("Added")
    });
    assert!(
        has_added_node,
        "graph_json should include added node entry with stage 'Added', got:
{}",
        response.graph_json
    );
}
