mod common;

use common::{CALLER_INSTANCE_ID, start_master_node};
use master_node::encoding::{NodeAddRequest, NodeListRequest};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_list_returns_dot_graph() {
    const TARGET_NODE_NAME: &str = "list_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const TARGET_INSTANCE_ID: &str = "list_instance";

    let started_master = start_master_node().await;
    let node_stack = started_master.node_stack.clone();

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

    let add_response = NodeAddRequest::new(&peppy_json5, "/tmp")
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

    let response = NodeListRequest::new()
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
        response.dot_graph.contains("digraph"),
        "dot_graph should be DOT format, got:\n{}",
        response.dot_graph
    );
    assert!(
        response
            .dot_graph
            .contains(&format!("{}:internal", started_master.master_node_name)),
        "dot_graph should include root node label, got:\n{}",
        response.dot_graph
    );
    assert!(
        response
            .dot_graph
            .contains(&format!("{TARGET_NODE_NAME}:{TARGET_NODE_TAG}")),
        "dot_graph should include added node label, got:\n{}",
        response.dot_graph
    );

    let label_count = response.dot_graph.matches("label=").count();
    assert_eq!(
        label_count, 2,
        "dot_graph should contain two node labels, got:\n{}",
        response.dot_graph
    );

    assert_eq!(
        response.dot_graph,
        node_stack.to_dot(),
        "dot_graph should match node_stack.to_dot()"
    );

    started_master.task.abort();
}
