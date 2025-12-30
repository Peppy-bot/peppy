mod common;

use common::{CALLER_INSTANCE_ID, start_master_node};
use master_node::encoding::NodeAddRequest;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_success() {
    const TARGET_NODE_NAME: &str = "runnable_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const TARGET_INSTANCE_ID: &str = "runnable_instance";

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

    assert!(node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG));
    assert_eq!(node_stack.len(), 2, "root + added node");

    let entity = node_stack
        .find(TARGET_NODE_NAME, TARGET_NODE_TAG)
        .expect("node should exist in stack");
    assert_eq!(entity.instances().len(), 1);
    assert_eq!(
        entity.instances()[0].instance_id().as_str(),
        TARGET_INSTANCE_ID
    );

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_invalid_config_fails() {
    let started_master = start_master_node().await;
    let node_stack = started_master.node_stack.clone();

    let peppy_json5 = r#"{ manifest: [unclosed"#;

    let node_add_request = NodeAddRequest::new(peppy_json5, "/tmp").with_instance_id("bad_node");
    let add_response = node_add_request
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
        !add_response.success,
        "node_add should fail for invalid json5"
    );
    assert_eq!(add_response.node_instance_id, "");
    assert!(
        add_response
            .error_message
            .as_ref()
            .map(|msg| msg.contains("Failed to parse node config"))
            .unwrap_or(false),
        "error message should indicate parse failure, got: {:?}",
        add_response.error_message
    );

    assert_eq!(node_stack.len(), 1, "only root should exist");

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_no_launch_cmd_fails() {
    let started_master = start_master_node().await;
    let node_stack = started_master.node_stack.clone();

    let peppy_json5 = r#"{
        schema_version: 1,
        manifest: {
            name: "no_launch_cmd_node",
            tag: "0.1.0",
        },
        parameters: {}
    }"#;

    let node_add_request = NodeAddRequest::new(peppy_json5, "/tmp");
    let add_response = node_add_request
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
        !add_response.success,
        "node_add should fail when launch_cmd is missing"
    );
    assert_eq!(add_response.node_instance_id, "");
    assert!(
        add_response
            .error_message
            .as_ref()
            .map(|msg| msg.contains("launch_cmd"))
            .unwrap_or(false),
        "error message should mention launch_cmd, got: {:?}",
        add_response.error_message
    );

    assert_eq!(node_stack.len(), 1, "only root should exist");

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_dependency_not_resolved() {
    let started_master = start_master_node().await;
    let node_stack = started_master.node_stack.clone();

    // Try to add a consumer node that depends on a non-existent provider
    let peppy_json5 = r#"{
        schema_version: 1,
        manifest: {
            name: "consumer_node",
            tag: "1.0.0",
            launch_cmd: ["sleep", "10"],
        },
        interfaces: {
            subscribes_to: {
                topics: [
                    {
                        id: "sensor_input",
                        node: "non_existent_node",
                        name: "sensor_data",
                        tag: "1.0.0"
                    }
                ]
            }
        }
    }"#;

    let node_add_request = NodeAddRequest::new(peppy_json5, "/tmp").with_instance_id("consumer_1");
    let add_response = node_add_request
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
        !add_response.success,
        "node_add should fail when dependencies are missing"
    );
    assert_eq!(add_response.node_instance_id, "");
    assert!(
        add_response
            .error_message
            .as_ref()
            .map(|msg| msg.contains("Failed to add node"))
            .unwrap_or(false),
        "error message should indicate add failure, got: {:?}",
        add_response.error_message
    );
    assert!(
        add_response
            .error_message
            .as_ref()
            .map(|msg| msg.contains("does not exist in the stack"))
            .unwrap_or(false),
        "error message should indicate missing dependency, got: {:?}",
        add_response.error_message
    );

    assert_eq!(node_stack.len(), 1, "only root should exist");

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_same_node_same_tags_fails() {
    const NODE_NAME: &str = "mismatch_node";
    const NODE_TAG: &str = "1.0.0";

    let started_master = start_master_node().await;
    let node_stack = started_master.node_stack.clone();

    // First add: no interfaces
    let peppy_json5_v1 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{NODE_NAME}",
                tag: "{NODE_TAG}",
                launch_cmd: ["sleep", "10"]
            }},
            parameters: {{}}
        }}"#
    );

    let add_v1 = NodeAddRequest::new(&peppy_json5_v1, "/tmp")
        .with_instance_id("mismatch_instance_1")
        .poll(
            &started_master.caller_handle,
            &started_master.master_node_name,
            CALLER_INSTANCE_ID,
            &started_master.master_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_add v1 should complete");

    assert!(
        add_v1.success,
        "node_add v1 should succeed, got error: {:?}",
        add_v1.error_message
    );

    assert_eq!(node_stack.len(), 2, "root + v1");
    let entity = node_stack
        .find(NODE_NAME, NODE_TAG)
        .expect("node should exist after v1");
    assert_eq!(entity.instances().len(), 1);

    // Second add: same name+tag but different interfaces -> should be rejected.
    let peppy_json5_v2 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{NODE_NAME}",
                tag: "{NODE_TAG}",
                launch_cmd: ["sleep", "10"]
            }},
            interfaces: {{
                exposes: {{
                    topics: [{{ name: "/example" }}]
                }}
            }}
        }}"#
    );

    let add_v2 = NodeAddRequest::new(&peppy_json5_v2, "/tmp")
        .with_instance_id("mismatch_instance_2")
        .poll(
            &started_master.caller_handle,
            &started_master.master_node_name,
            CALLER_INSTANCE_ID,
            &started_master.master_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_add v2 should complete");

    assert!(
        !add_v2.success,
        "node_add should fail when interfaces mismatch for same name+tag"
    );
    assert!(
        add_v2
            .error_message
            .as_ref()
            .map(|msg| msg.contains("Config mismatch"))
            .unwrap_or(false),
        "error message should indicate config mismatch, got: {:?}",
        add_v2.error_message
    );

    assert_eq!(node_stack.len(), 2, "stack should be unchanged");
    let entity = node_stack
        .find(NODE_NAME, NODE_TAG)
        .expect("node should still exist after v2 failure");
    assert_eq!(entity.instances().len(), 1, "should not add a new instance");

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_same_node_different_tags_create_two_entities() {
    const NODE_NAME: &str = "versioned_node";

    let started_master = start_master_node().await;
    let node_stack = started_master.node_stack.clone();

    let peppy_json5_v1 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{NODE_NAME}",
                tag: "1.0.0",
                launch_cmd: ["sleep", "10"]
            }}
        }}"#
    );

    let add_v1 = NodeAddRequest::new(&peppy_json5_v1, "/tmp")
        .with_instance_id("versioned_instance_1")
        .poll(
            &started_master.caller_handle,
            &started_master.master_node_name,
            CALLER_INSTANCE_ID,
            &started_master.master_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_add v1 should complete");

    assert!(
        add_v1.success,
        "node_add v1 should succeed, got error: {:?}",
        add_v1.error_message
    );

    let peppy_json5_v2 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{NODE_NAME}",
                tag: "2.0.0",
                launch_cmd: ["sleep", "10"]
            }}
        }}"#
    );

    let add_v2 = NodeAddRequest::new(&peppy_json5_v2, "/tmp")
        .with_instance_id("versioned_instance_2")
        .poll(
            &started_master.caller_handle,
            &started_master.master_node_name,
            CALLER_INSTANCE_ID,
            &started_master.master_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_add v2 should complete");

    assert!(
        add_v2.success,
        "node_add v2 should succeed, got error: {:?}",
        add_v2.error_message
    );

    assert_eq!(node_stack.len(), 3, "root + two versions");
    assert!(node_stack.contains(NODE_NAME, "1.0.0"));
    assert!(node_stack.contains(NODE_NAME, "2.0.0"));

    started_master.task.abort();
}
