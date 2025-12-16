mod common;

use common::{CALLER_INSTANCE_ID, setup_test_master_node};
use config::node::NodeConfigParser;
use config::peppy_config::BuildSystem;
use master_node::encoding::{NodeAddRequest, NodeListRequest, NodeSyncRequest};
use std::path::PathBuf;
use std::time::Duration;
use tempfile::Builder;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_node_list_returns_dot_graph() {
    let (client, server) = setup_test_master_node().await;

    // Add a provider node that exposes a topic
    let provider_config = NodeConfigParser::from_content(
        r#"{
            schema_version: 1,
            manifest: {
                name: "sensor_node",
                tag: "1.0.0"
            },
            interfaces: {
                exposes: {
                    topics: [
                        {
                            name: "sensor_data",
                            qos_profile: "sensor_data",
                            message_format: {
                                value: "f32"
                            }
                        }
                    ]
                }
            }
        }"#,
    )
    .expect("provider config should parse");

    server
        .node_stack
        .push_config(&provider_config, None, false)
        .expect("provider should be added to node stack");

    // Add a consumer node that depends on the provider
    let consumer_config = NodeConfigParser::from_content(
        r#"{
            schema_version: 1,
            manifest: {
                name: "consumer_node",
                tag: "1.0.0"
            },
            interfaces: {
                subscribes_to: {
                    topics: [
                        {
                            id: "sensor_input",
                            node: "sensor_node",
                            name: "sensor_data",
                            tag: "1.0.0"
                        }
                    ]
                }
            }
        }"#,
    )
    .expect("consumer config should parse");

    server
        .node_stack
        .push_config(&consumer_config, None, false)
        .expect("consumer should be added to node stack");

    // Request the node list via the service
    let request = NodeListRequest::new();
    let node_list_response = request
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            Some(&client.instance_id),
            Duration::from_secs(2),
        )
        .await
        .expect("poll should succeed");

    let dot_graph = &node_list_response.dot_graph;

    // Verify the DOT graph structure
    assert!(
        dot_graph.contains("digraph"),
        "DOT graph should be a directed graph, got: {}",
        dot_graph
    );

    // Find node indices by their labels in the DOT graph.
    // Format: `N [ label="name:tag\n(X instance(s))" ]` for nodes
    let find_node_index = |node_label: &str| -> Option<&str> {
        dot_graph.lines().find_map(|line| {
            if line.contains(&format!("label=\"{}\\n", node_label)) {
                line.trim().split_whitespace().next()
            } else {
                None
            }
        })
    };

    let master_idx = find_node_index("test_master_node:internal")
        .expect("master node should be in the DOT graph");
    let sensor_idx =
        find_node_index("sensor_node:1.0.0").expect("sensor_node should be in the DOT graph");
    let consumer_idx =
        find_node_index("consumer_node:1.0.0").expect("consumer_node should be in the DOT graph");

    // Verify all three nodes have distinct indices
    assert_ne!(master_idx, sensor_idx);
    assert_ne!(master_idx, consumer_idx);
    assert_ne!(sensor_idx, consumer_idx);

    // Verify the dependency edge: consumer -> sensor (consumer depends on sensor)
    let expected_edge = format!("{} -> {}", consumer_idx, sensor_idx);
    assert!(
        dot_graph.contains(&expected_edge),
        "DOT graph should contain edge from consumer to sensor ({}), got: {}",
        expected_edge,
        dot_graph
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_node_add_success() {
    let (client, server) = setup_test_master_node().await;

    // Add a provider node that exposes a topic
    let peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "sensor_node",
                tag: "1.0.0"
            },
            interfaces: {
                exposes: {
                    topics: [
                        {
                            name: "sensor_data",
                            qos_profile: "sensor_data",
                            message_format: {
                                value: "f32"
                            }
                        }
                    ]
                }
            }
        }"#;

    let from_dir = PathBuf::from("/tmp/test");
    let custom_instance_id = "my_custom_sensor_instance";

    let request = NodeAddRequest::new(peppy_json5, from_dir).with_instance_id(custom_instance_id);
    let node_add_response = request
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            Some(&client.instance_id),
            Duration::from_secs(2),
        )
        .await
        .expect("poll should succeed");

    assert!(node_add_response.success);
    assert!(node_add_response.error_message.is_empty());
    assert_eq!(
        node_add_response.node_id, custom_instance_id,
        "node_id should match the custom instance_id provided in the request"
    );

    // Verify the node was added to the node stack
    assert!(
        server.node_stack.contains("sensor_node", "1.0.0"),
        "sensor_node:1.0.0 should be present in the node stack after node_add"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_node_add_invalid_config() {
    let (client, _server) = setup_test_master_node().await;

    let peppy_json5 = "invalid json5 {{{";
    let from_dir = PathBuf::from("/tmp/test");

    let request = NodeAddRequest::new(peppy_json5, from_dir);
    let node_add_response = request
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            Some(&client.instance_id),
            Duration::from_secs(2),
        )
        .await
        .expect("poll should succeed");

    assert!(!node_add_response.success);
    assert!(
        node_add_response.error_message.contains("Failed to parse"),
        "Error message should indicate parsing failure, got: {}",
        node_add_response.error_message
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_node_add_dependency_not_resolved() {
    let (client, server) = setup_test_master_node().await;

    // Try to add a consumer node that depends on a non-existent provider
    let peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "consumer_node",
                tag: "1.0.0"
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

    let from_dir = PathBuf::from("/tmp/test");

    let request = NodeAddRequest::new(peppy_json5, from_dir);
    let node_add_response = request
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            Some(&client.instance_id),
            Duration::from_secs(2),
        )
        .await
        .expect("poll should succeed");

    assert!(!node_add_response.success);
    assert!(
        node_add_response
            .error_message
            .contains("non_existent_node"),
        "Error message should mention the missing dependency, got: {}",
        node_add_response.error_message
    );

    // Verify the node was NOT added to the node stack
    assert!(
        !server.node_stack.contains("consumer_node", "1.0.0"),
        "consumer_node:1.0.0 should NOT be present in the node stack"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_node_sync_success() {
    let (client, _server) = setup_test_master_node().await;

    let temp_dir = Builder::new()
        .prefix("node_sync")
        .tempdir()
        .expect("failed to create tempdir");
    let node_root_dir = temp_dir.path().to_path_buf();

    let peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "sensor_node",
                tag: "1.0.0"
            },
            interfaces: {
                exposes: {
                    topics: [
                        {
                            name: "sensor_data",
                            qos_profile: "sensor_data",
                            message_format: {
                                value: "f32"
                            }
                        }
                    ]
                }
            }
        }"#;

    std::fs::write(
        node_root_dir.join(config::consts::PEPPY_NODE_CONFIG_FILE),
        peppy_json5,
    )
    .expect("failed to write node config");

    let request = NodeSyncRequest::new(&node_root_dir).with_build_system(BuildSystem::Rust);

    let node_sync_response = request
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            Some(&client.instance_id),
            Duration::from_secs(2),
        )
        .await
        .expect("poll should succeed");

    assert!(node_sync_response.success);
    assert!(
        node_sync_response.error_message.is_empty(),
        "expected empty error message, got: {}",
        node_sync_response.error_message
    );

    let output_dir = node_root_dir.join(".peppy/libs/peppygen");
    assert!(
        output_dir.join("Cargo.toml").exists(),
        "expected generated peppygen Cargo.toml at {}",
        output_dir.display()
    );
    assert!(
        output_dir.join("src/lib.rs").exists(),
        "expected generated peppygen src/lib.rs at {}",
        output_dir.display()
    );

    assert!(
        output_dir.join("src/exposed_topics.rs").exists(),
        "expected generated exposed_topics module at {}",
        output_dir.display()
    );
    assert!(
        output_dir
            .join("src/exposed_topics/sensor_data.rs")
            .exists(),
        "expected generated sensor_data topic module at {}",
        output_dir.display()
    );

    assert!(
        output_dir.join(".peppygen/node_config.sha256").exists(),
        "expected node config fingerprint at {}",
        output_dir.display()
    );
    assert!(
        !output_dir
            .join(config::consts::PEPPY_NODE_CONFIG_FILE)
            .exists(),
        "peppy.json5 should not be copied into the generated crate"
    );
}
