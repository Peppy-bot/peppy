mod common;

use common::{CALLER_INSTANCE_ID, setup_test_master_node};
use master_node::encoding::NodeAddRequest;
use std::path::PathBuf;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_node_success() {
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
async fn add_node_invalid_config() {
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
async fn add_node_dependency_not_resolved() {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_same_node_different_tags_create_two_entities() {
    let (client, server) = setup_test_master_node().await;

    // Add sensor_node with tag 1.0.0
    let peppy_json5_v1 = r#"{
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
    let request_v1 = NodeAddRequest::new(peppy_json5_v1, from_dir.clone());
    let response_v1 = request_v1
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            Duration::from_secs(2),
        )
        .await
        .expect("poll should succeed");

    assert!(response_v1.success, "First node add should succeed");

    // Add sensor_node with tag 2.0.0 (different tag)
    let peppy_json5_v2 = r#"{
            schema_version: 1,
            manifest: {
                name: "sensor_node",
                tag: "2.0.0"
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

    let request_v2 = NodeAddRequest::new(peppy_json5_v2, from_dir);
    let response_v2 = request_v2
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            Duration::from_secs(2),
        )
        .await
        .expect("poll should succeed");

    assert!(response_v2.success, "Second node add should succeed");

    // Verify both entities exist
    assert!(
        server.node_stack.contains("sensor_node", "1.0.0"),
        "sensor_node:1.0.0 should be present in the node stack"
    );
    assert!(
        server.node_stack.contains("sensor_node", "2.0.0"),
        "sensor_node:2.0.0 should be present in the node stack"
    );

    // Verify they are separate entities (stack should have 3 nodes: root + 2 sensor nodes)
    assert_eq!(
        server.node_stack.len(),
        3,
        "Node stack should contain root node plus two sensor_node entities"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_same_node_same_tags_fails() {
    let (client, server) = setup_test_master_node().await;

    // Add sensor_node:1.0.0 with one interface
    let peppy_json5_first = r#"{
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
    let request_first = NodeAddRequest::new(peppy_json5_first, from_dir.clone());
    let response_first = request_first
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            Duration::from_secs(2),
        )
        .await
        .expect("poll should succeed");

    assert!(response_first.success, "First node add should succeed");

    // Try to add sensor_node:1.0.0 again with DIFFERENT interfaces
    let peppy_json5_second = r#"{
            schema_version: 1,
            manifest: {
                name: "sensor_node",
                tag: "1.0.0"
            },
            interfaces: {
                exposes: {
                    topics: [
                        {
                            name: "different_topic",
                            qos_profile: "standard",
                            message_format: {
                                value: "i32"
                            }
                        }
                    ]
                }
            }
        }"#;

    let request_second = NodeAddRequest::new(peppy_json5_second, from_dir);
    let response_second = request_second
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            Duration::from_secs(2),
        )
        .await
        .expect("poll should succeed");

    // Should fail because the same name:tag with different interfaces is not allowed
    assert!(
        !response_second.success,
        "Adding same node:tag with different interfaces should fail"
    );
    let error_message = response_second
        .error_message
        .expect("error_message should be present on failure");
    assert!(
        error_message.contains("sensor_node") || error_message.contains("mismatch"),
        "Error message should indicate a config mismatch, got: {}",
        error_message
    );

    // Verify only one entity exists
    assert!(
        server.node_stack.contains("sensor_node", "1.0.0"),
        "Original sensor_node:1.0.0 should still be present"
    );
    assert_eq!(
        server.node_stack.len(),
        2,
        "Node stack should contain only root node and one sensor_node entity"
    );
}
