mod common;

use common::{CALLER_INSTANCE_ID, setup_test_master_node};
use master_node::encoding::NodeAddRequest;
use std::path::PathBuf;
use std::time::Duration;

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
    assert!(node_add_response.error_message.is_none());
    assert_eq!(
        node_add_response.node_instance_id, custom_instance_id,
        "node_instance_id should match the custom instance_id provided in the request"
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
    let error_message = node_add_response
        .error_message
        .expect("error_message should be present on failure");
    assert!(
        error_message.contains("Failed to parse"),
        "Error message should indicate parsing failure, got: {}",
        error_message
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
    let error_message = node_add_response
        .error_message
        .expect("error_message should be present on failure");
    assert!(
        error_message.contains("non_existent_node"),
        "Error message should mention the missing dependency, got: {}",
        error_message
    );

    // Verify the node was NOT added to the node stack
    assert!(
        !server.node_stack.contains("consumer_node", "1.0.0"),
        "consumer_node:1.0.0 should NOT be present in the node stack"
    );
}
