mod common;

use common::{CALLER_INSTANCE_ID, setup_test_master_node};
use config::node::NodeConfigParser;
use master_node::encoding::{
    NodeAddRequest, NodeAddResponse, NodeListRequest, NodeListResponse, NodeSyncRequest,
    NodeSyncResponse,
};
use peppylib::messaging::ServiceMessenger;
use std::path::PathBuf;
use std::time::Duration;

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
    let request_payload = request
        .encode()
        .expect("failed to encode node_list request");

    let response = ServiceMessenger::poll(
        &client.caller_handle,
        &client.master_node_name,
        CALLER_INSTANCE_ID,
        &client.master_node_name,
        "node_list",
        None,
        Some(&client.instance_id),
        request_payload,
        Duration::from_secs(2),
    )
    .await
    .expect("caller should receive response");

    let node_list_response = NodeListResponse::decode(&response.payload().to_bytes())
        .expect("should decode node_list response");

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
    let (client, _server) = setup_test_master_node().await;

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

    let request = NodeAddRequest::new(peppy_json5, from_dir);
    let request_payload = request.encode().expect("failed to encode node_add request");

    let response = ServiceMessenger::poll(
        &client.caller_handle,
        &client.master_node_name,
        CALLER_INSTANCE_ID,
        &client.master_node_name,
        "node_add",
        None,
        Some(&client.instance_id),
        request_payload,
        Duration::from_secs(2),
    )
    .await
    .expect("caller should receive response");

    let node_add_response = NodeAddResponse::decode(&response.payload().to_bytes())
        .expect("should decode node_add response");

    assert!(node_add_response.success);
    assert!(node_add_response.error_message.is_empty());
    todo!("Verify the node was added successfully");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "validation not implemented yet - test will pass once node_add validates config"]
async fn test_node_add_invalid_config() {
    let (client, _server) = setup_test_master_node().await;

    let peppy_json5 = "invalid json5 {{{";
    let from_dir = PathBuf::from("/tmp/test");

    let request = NodeAddRequest::new(peppy_json5, from_dir);
    let request_payload = request.encode().expect("failed to encode node_add request");

    let response = ServiceMessenger::poll(
        &client.caller_handle,
        &client.master_node_name,
        CALLER_INSTANCE_ID,
        &client.master_node_name,
        "node_add",
        None,
        Some(&client.instance_id),
        request_payload,
        Duration::from_secs(2),
    )
    .await
    .expect("caller should receive response");

    let node_add_response = NodeAddResponse::decode(&response.payload().to_bytes())
        .expect("should decode node_add response");

    assert!(!node_add_response.success);
    assert!(!node_add_response.error_message.is_empty());
    todo!("Verify the node addition failed with an error");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_node_sync_success() {
    let (client, _server) = setup_test_master_node().await;

    let request = NodeSyncRequest::new();
    let request_payload = request
        .encode()
        .expect("failed to encode node_sync request");

    let response = ServiceMessenger::poll(
        &client.caller_handle,
        &client.master_node_name,
        CALLER_INSTANCE_ID,
        &client.master_node_name,
        "node_sync",
        None,
        Some(&client.instance_id),
        request_payload,
        Duration::from_secs(2),
    )
    .await
    .expect("caller should receive response");

    let node_sync_response = NodeSyncResponse::decode(&response.payload().to_bytes())
        .expect("should decode node_sync response");

    assert!(node_sync_response.success);
    assert!(node_sync_response.error_message.is_empty());
    todo!("Verify the sync was successful");
}
