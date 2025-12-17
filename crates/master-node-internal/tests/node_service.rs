mod common;

use common::{CALLER_INSTANCE_ID, setup_test_master_node};
use config::node::NodeConfigParser;
use config::peppy_config::BuildSystem;
use master_node::encoding::{NodeAddRequest, NodeInitRequest, NodeListRequest, NodeSyncRequest};
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

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
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
        node_root_dir.join(config::consts::NODE_CONFIG_FILE),
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

    let peppygen_dir = node_root_dir.join(".peppy/libs/peppygen");
    assert!(
        peppygen_dir.join("Cargo.toml").exists(),
        "expected generated peppygen Cargo.toml at {}",
        peppygen_dir.display()
    );
    assert!(
        peppygen_dir.join("src/lib.rs").exists(),
        "expected generated peppygen src/lib.rs at {}",
        peppygen_dir.display()
    );

    assert!(
        peppygen_dir.join("src/exposed_topics.rs").exists(),
        "expected generated exposed_topics module at {}",
        peppygen_dir.display()
    );
    assert!(
        peppygen_dir
            .join("src/exposed_topics/sensor_data.rs")
            .exists(),
        "expected generated sensor_data topic module at {}",
        peppygen_dir.display()
    );

    assert!(
        peppygen_dir
            .join(config::consts::NODE_CONFIG_FINGERPRINT_FILE)
            .exists(),
        "expected node config fingerprint at {}",
        peppygen_dir.display()
    );
    assert!(
        !peppygen_dir.join(config::consts::NODE_CONFIG_FILE).exists(),
        "peppy.json5 should not be copied into the generated crate"
    );
}

// Long running test
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_node_init_rust_success() {
    let (client, _server) = setup_test_master_node().await;

    let temp_dir = Builder::new()
        .prefix("node_init")
        .tempdir()
        .expect("failed to create tempdir");
    let node_root_dir = temp_dir.path().to_path_buf();
    let node_name = "my_rust_node";

    let request =
        NodeInitRequest::new(&node_root_dir, node_name).with_build_system(BuildSystem::Cargo);

    let node_init_response = request
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

    assert!(
        node_init_response.success,
        "node_init should succeed, got error: {}",
        node_init_response.error_message
    );
    assert!(
        node_init_response.error_message.is_empty(),
        "expected empty error message, got: {}",
        node_init_response.error_message
    );

    // Verify the node directory was created
    let node_dir = node_root_dir.join(node_name);
    assert!(
        node_dir.exists(),
        "expected node directory to be created at {}",
        node_dir.display()
    );

    // Verify peppy.json5 was created
    assert!(
        node_dir.join(config::consts::NODE_CONFIG_FILE).exists(),
        "expected peppy.json5 to be created"
    );

    // Verify peppy.json5 can be parsed
    let peppy_config =
        NodeConfigParser::from_path(&node_dir.join(config::consts::NODE_CONFIG_FILE))
            .expect("peppy.json5 should be valid");
    assert_eq!(peppy_config.manifest.name.as_str(), node_name);

    // Verify Cargo.toml was created
    let cargo_toml_path = node_dir.join("Cargo.toml");
    assert!(
        cargo_toml_path.exists(),
        "expected Cargo.toml to be created at {}",
        cargo_toml_path.display()
    );

    // Verify Cargo.toml contains the node name
    let cargo_content =
        std::fs::read_to_string(&cargo_toml_path).expect("failed to read Cargo.toml");
    assert!(
        cargo_content.contains(&format!("name = \"{}\"", node_name)),
        "Cargo.toml should contain the node name"
    );

    // Verify Cargo.toml contains peppygen dependency
    assert!(
        cargo_content.contains(config::consts::PEPPYGEN_OUTPUT_PATH),
        "Cargo.toml should contain peppygen dependency path, got: {}",
        cargo_content
    );

    // Verify src/main.rs was created
    assert!(
        node_dir.join("src/main.rs").exists(),
        "expected src/main.rs to be created"
    );

    // Verify .gitignore was created
    assert!(
        node_dir.join(".gitignore").exists(),
        "expected .gitignore to be created"
    );

    // Verify peppygen was generated
    let peppygen_dir = node_dir.join(config::consts::PEPPYGEN_OUTPUT_PATH);
    assert!(
        peppygen_dir.join("Cargo.toml").exists(),
        "expected peppygen Cargo.toml at {}",
        peppygen_dir.display()
    );
    assert!(
        peppygen_dir.join("src/lib.rs").exists(),
        "expected peppygen src/lib.rs at {}",
        peppygen_dir.display()
    );

    // Compile the project and check that the compilation went fine
    let cargo_output = std::process::Command::new("cargo")
        .arg("build")
        .current_dir(&node_dir)
        .output()
        .expect("failed to invoke cargo build on generated node");
    assert!(
        cargo_output.status.success(),
        "cargo build failed for generated node with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        cargo_output.status.code(),
        String::from_utf8_lossy(&cargo_output.stdout),
        String::from_utf8_lossy(&cargo_output.stderr)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_node_init_python_success() {
    let (client, _server) = setup_test_master_node().await;

    let temp_dir = Builder::new()
        .prefix("node_init")
        .tempdir()
        .expect("failed to create tempdir");
    let node_root_dir = temp_dir.path().to_path_buf();
    let node_name = "my_python_node";

    let request =
        NodeInitRequest::new(&node_root_dir, node_name).with_build_system(BuildSystem::Python);

    let node_init_response = request
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

    assert!(
        node_init_response.success,
        "node_init should succeed, got error: {}",
        node_init_response.error_message
    );

    // Verify the node directory was created
    let node_dir = node_root_dir.join(node_name);
    assert!(
        node_dir.exists(),
        "expected node directory to be created at {}",
        node_dir.display()
    );

    // Verify peppy.json5 was created
    assert!(
        node_dir.join(config::consts::NODE_CONFIG_FILE).exists(),
        "expected peppy.json5 to be created"
    );

    // Verify pyproject.toml was created
    let pyproject_path = node_dir.join("pyproject.toml");
    assert!(
        pyproject_path.exists(),
        "expected pyproject.toml to be created at {}",
        pyproject_path.display()
    );

    // Verify pyproject.toml contains the node name
    let pyproject_content =
        std::fs::read_to_string(&pyproject_path).expect("failed to read pyproject.toml");
    assert!(
        pyproject_content.contains(&format!("name = \"{}\"", node_name)),
        "pyproject.toml should contain the node name"
    );

    // Verify main.py was created
    assert!(
        node_dir.join("main.py").exists(),
        "expected main.py to be created"
    );

    // Verify .gitignore was created
    assert!(
        node_dir.join(".gitignore").exists(),
        "expected .gitignore to be created"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_node_init_fails_if_directory_exists() {
    let (client, _server) = setup_test_master_node().await;

    let temp_dir = Builder::new()
        .prefix("node_init")
        .tempdir()
        .expect("failed to create tempdir");
    let node_root_dir = temp_dir.path().to_path_buf();
    let node_name = "existing_node";

    // Pre-create the node directory
    let node_dir = node_root_dir.join(node_name);
    std::fs::create_dir_all(&node_dir).expect("failed to create existing node directory");

    let request =
        NodeInitRequest::new(&node_root_dir, node_name).with_build_system(BuildSystem::Cargo);

    let node_init_response = request
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

    assert!(
        !node_init_response.success,
        "node_init should fail when directory exists"
    );
    assert!(
        node_init_response.error_message.contains("already exists"),
        "error message should indicate directory already exists, got: {}",
        node_init_response.error_message
    );
}
