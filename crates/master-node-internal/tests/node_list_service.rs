mod common;

use common::{CALLER_INSTANCE_ID, setup_test_master_node};
use config::node::NodeConfigParser;
use master_node::encoding::NodeListRequest;
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
