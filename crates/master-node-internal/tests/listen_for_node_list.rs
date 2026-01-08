mod common;

use common::{CALLER_INSTANCE_ID, start_master_node};
use config::consts::NODE_CONFIG_FILE;
use master_node::encoding::{NodeAddRequest, NodeListRequest};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_list_returns_succeeds() {
    const TARGET_NODE_NAME: &str = "list_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const TARGET_INSTANCE_ID: &str = "list_instance";

    let started_master = start_master_node().await;
    let node_stack = started_master.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
                launch_cmd: ["sleep", "10"]
            }},
            parameters: {{}}
        }}"#
    );
    std::fs::write(source_dir.path().join(NODE_CONFIG_FILE), &peppy_json5)
        .expect("failed to write peppy.json5");

    let add_response = NodeAddRequest::new(source_dir.path())
        .with_instance_id(TARGET_INSTANCE_ID)
        .poll(
            &started_master.caller_handle,
            &started_master.master_node_name,
            CALLER_INSTANCE_ID,
            &started_master.master_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_add request should complete");

    assert!(
        add_response.success,
        "node_add should succeed, got error: {:?}",
        add_response.error_message
    );
    assert!(node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG));
    assert_eq!(node_stack.len(), 2, "root + added node");

    let response = NodeListRequest::new(false)
        .poll(
            &started_master.caller_handle,
            &started_master.master_node_name,
            CALLER_INSTANCE_ID,
            &started_master.master_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_list request should complete");

    assert!(
        response.dot_graph.is_none(),
        "dot_graph should be omitted when with_dot_graph=false, got: {:?}",
        response.dot_graph
    );

    let graph_json: serde_json::Value =
        serde_json::from_str(&response.graph_json).expect("graph_json should be valid JSON");
    let nodes = graph_json
        .get("nodes")
        .and_then(|nodes| nodes.as_array())
        .expect("graph_json should include a `nodes` array");

    let has_root = nodes.iter().any(|node| {
        node.get("name").and_then(|v| v.as_str()) == Some(&started_master.master_node_name)
            && node.get("tag").and_then(|v| v.as_str()) == Some("master-node")
    });
    assert!(
        has_root,
        "graph_json should include root node entry, got:\n{}",
        response.graph_json
    );

    let has_added_node = nodes.iter().any(|node| {
        node.get("name").and_then(|v| v.as_str()) == Some(TARGET_NODE_NAME)
            && node.get("tag").and_then(|v| v.as_str()) == Some(TARGET_NODE_TAG)
    });
    assert!(
        has_added_node,
        "graph_json should include added node entry, got:\n{}",
        response.graph_json
    );

    // Clean up copied directory
    if let Some(entity) = node_stack.find(TARGET_NODE_NAME, TARGET_NODE_TAG) {
        let _ = std::fs::remove_dir_all(entity.root_path());
    }

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_list_returns_dot_graph() {
    const TARGET_NODE_NAME: &str = "list_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const TARGET_INSTANCE_ID: &str = "list_instance";

    let started_master = start_master_node().await;
    let node_stack = started_master.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
                launch_cmd: ["sleep", "10"]
            }},
            parameters: {{}}
        }}"#
    );
    std::fs::write(source_dir.path().join(NODE_CONFIG_FILE), &peppy_json5)
        .expect("failed to write peppy.json5");

    let add_response = NodeAddRequest::new(source_dir.path())
        .with_instance_id(TARGET_INSTANCE_ID)
        .poll(
            &started_master.caller_handle,
            &started_master.master_node_name,
            CALLER_INSTANCE_ID,
            &started_master.master_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_add request should complete");

    assert!(
        add_response.success,
        "node_add should succeed, got error: {:?}",
        add_response.error_message
    );
    assert!(node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG));
    assert_eq!(node_stack.len(), 2, "root + added node");

    let response = NodeListRequest::new(true)
        .poll(
            &started_master.caller_handle,
            &started_master.master_node_name,
            CALLER_INSTANCE_ID,
            &started_master.master_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_list request should complete");

    let dot_graph = response
        .dot_graph
        .expect("dot_graph should be returned when with_dot_graph=true");

    assert!(
        dot_graph.contains("digraph"),
        "dot_graph should be DOT format, got:\n{}",
        dot_graph
    );
    assert!(
        dot_graph.contains(&format!("{}:master-node", started_master.master_node_name)),
        "dot_graph should include root node label, got:\n{}",
        dot_graph
    );
    assert!(
        dot_graph.contains(&format!("{TARGET_NODE_NAME}:{TARGET_NODE_TAG}")),
        "dot_graph should include added node label, got:\n{}",
        dot_graph
    );

    let label_count = dot_graph.matches("label=").count();
    assert_eq!(
        label_count, 2,
        "dot_graph should contain two node labels, got:\n{}",
        dot_graph
    );

    assert_eq!(
        dot_graph,
        node_stack.to_dot(),
        "dot_graph should match node_stack.to_dot()"
    );

    // Clean up copied directory
    if let Some(entity) = node_stack.find(TARGET_NODE_NAME, TARGET_NODE_TAG) {
        let _ = std::fs::remove_dir_all(entity.root_path());
    }

    started_master.task.abort();
}
