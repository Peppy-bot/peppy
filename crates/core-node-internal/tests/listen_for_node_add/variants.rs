use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_variant_local_source() {
    const ROOT_NODE_NAME: &str = "robot_brain";
    const ROOT_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    // Create root node directory with variant inside it
    let parent_dir = tempfile::tempdir().expect("failed to create parent dir");
    let root_dir = parent_dir.path().join("root_node");
    let variant_dir = root_dir.join("mock_node");
    std::fs::create_dir_all(&root_dir).unwrap();
    std::fs::create_dir_all(&variant_dir).unwrap();

    // Root node config with a "mock" variant pointing to a subdirectory
    let root_config = r#"{
        schema_version: 1,
        manifest: {
            name: "robot_brain",
            tag: "0.1.0",
            variants: [
                { name: "mock", source: { local: "mock_node" } }
            ]
        },
        interfaces: {
            topics: {
                emits: [
                    { name: "joint_positions", qos_profile: "sensor_data", message_format: { x: "f64", y: "f64" } }
                ]
            }
        },
        execution: {
            language: "rust",
            run_cmd: ["sleep", "10"]
        }
    }"#;
    write_peppy_json5(&root_dir, root_config);

    // Variant config — only defines runtime (no manifest, no interfaces)
    let variant_config = r#"{
        schema_version: 1,
        execution: {
            language: "rust",
            run_cmd: ["sleep", "5"]
        }
    }"#;
    write_peppy_json5(&variant_dir, variant_config);

    let add_result = send_node_add_and_wait_with_variant(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        root_dir.as_path(),
        "mock",
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add with variant should succeed");

    assert!(
        add_result.success,
        "variant node_add should succeed, got error: {:?}",
        add_result.error_message
    );

    // Variant should be in the stack under the root node's name:tag
    assert!(node_stack.contains(ROOT_NODE_NAME, ROOT_NODE_TAG));

    let entity = node_stack
        .find(ROOT_NODE_NAME, ROOT_NODE_TAG)
        .expect("node should exist in stack");
    let entity_guard = entity.read();
    // The config in the stack should have root's interfaces but variant's runtime
    let config = entity_guard.config();
    assert!(
        config.interfaces.topics.is_some(),
        "interfaces should be inherited from root"
    );
    assert_eq!(
        config.execution.run_cmd.as_ref().unwrap(),
        &vec!["sleep".to_string(), "5".to_string()],
        "execution should come from the variant"
    );
    drop(entity_guard);
}
/// `node sync` must fingerprint the variant's own peppy.json5,
/// not the temporary merged config, so that `node add` verification passes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_variant_local_source_after_sync() {
    use core_node::encoding::NodeSyncRequest;

    const ROOT_NODE_NAME: &str = "synced_robot";
    const ROOT_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    // Create root node directory with variant inside it
    let parent_dir = tempfile::tempdir().expect("failed to create parent dir");
    let root_dir = parent_dir.path().join("root_node");
    let variant_dir = root_dir.join("mock_node");
    std::fs::create_dir_all(&root_dir).unwrap();
    std::fs::create_dir_all(&variant_dir).unwrap();

    // Root node config with a "mock" variant pointing to a subdirectory
    let root_config = r#"{
        schema_version: 1,
        manifest: {
            name: "synced_robot",
            tag: "0.1.0",
            variants: [
                { name: "mock", source: { local: "mock_node" } }
            ]
        },
        interfaces: {
            topics: {
                emits: [
                    { name: "joint_positions", qos_profile: "sensor_data", message_format: { x: "f64", y: "f64" } }
                ]
            }
        },
        execution: {
            language: "rust",
            run_cmd: ["sleep", "10"]
        }
    }"#;
    // Write configs WITHOUT pre-baked fingerprints — sync will generate them.
    let root_config_path = root_dir.join(NODE_CONFIG_FILE);
    std::fs::write(&root_config_path, root_config).expect("failed to write root config");

    // Variant config — only defines execution (no manifest, no interfaces)
    let variant_config = r#"{
        schema_version: 1,
        execution: {
            language: "rust",
            run_cmd: ["sleep", "5"]
        }
    }"#;
    let variant_config_path = variant_dir.join(NODE_CONFIG_FILE);
    std::fs::write(&variant_config_path, variant_config).expect("failed to write variant config");

    // Step 1: Run node sync — this generates peppygen + fingerprint for root and variant.
    let sync_response = NodeSyncRequest::new(&root_dir, TEST_GIT_HASH, vec![])
        .poll(
            &started_core_node.caller_handle,
            &started_core_node.core_node_name,
            CALLER_INSTANCE_ID,
            &started_core_node.core_node_name,
            Duration::from_secs(10),
        )
        .await
        .expect("node_sync request should complete");

    assert!(
        sync_response.success,
        "node_sync should succeed, got error: {}",
        sync_response.error_message
    );

    // Sanity: variant .peppy directory should exist after sync
    assert!(
        variant_dir.join(PEPPY_OUTPUT_DIR).exists(),
        "variant .peppy directory should exist after sync"
    );

    // Step 2: Run node add with the variant.
    let add_result = send_node_add_and_wait_with_variant(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        root_dir.as_path(),
        "mock",
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add with variant should succeed");

    assert!(
        add_result.success,
        "variant node_add after sync should succeed, got error: {:?}",
        add_result.error_message
    );

    // Verify the node is in the stack with the expected merged config
    assert!(node_stack.contains(ROOT_NODE_NAME, ROOT_NODE_TAG));

    let entity = node_stack
        .find(ROOT_NODE_NAME, ROOT_NODE_TAG)
        .expect("node should exist in stack");
    let entity_guard = entity.read();
    let config = entity_guard.config();
    assert!(
        config.interfaces.topics.is_some(),
        "interfaces should be inherited from root"
    );
    assert_eq!(
        config.execution.run_cmd.as_ref().unwrap(),
        &vec!["sleep".to_string(), "5".to_string()],
        "execution should come from the variant"
    );
    drop(entity_guard);

    // Verify that the fingerprint stored by sync matches the variant's peppy.json5 content
    let stored_fingerprint =
        config::fingerprint::read_codegen_fingerprint(&variant_config_path, PEPPYGEN_OUTPUT_PATH)
            .expect("variant fingerprint should be readable after sync");
    let expected_fingerprint =
        config::fingerprint::fingerprint_for_bytes(variant_config.as_bytes());
    assert_eq!(
        stored_fingerprint, expected_fingerprint,
        "stored fingerprint should match the variant's peppy.json5 content"
    );
}
/// Variant-only nodes (no execution at root, only in variants) must work with
/// `node sync` + `node add --variant`. Sync skips peppygen generation for the
/// root when it has no execution block, so only the variant directory gets a
/// `.peppy/git.hash`. The `node add` verification must use the resolved variant
/// path, not the root.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_variant_only_node_after_sync() {
    use core_node::encoding::NodeSyncRequest;

    const ROOT_NODE_NAME: &str = "variant_only_robot";
    const ROOT_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    // Create root node directory with variant inside it
    let parent_dir = tempfile::tempdir().expect("failed to create parent dir");
    let root_dir = parent_dir.path().join("root_node");
    let variant_dir = root_dir.join("mock_node");
    std::fs::create_dir_all(&root_dir).unwrap();
    std::fs::create_dir_all(&variant_dir).unwrap();

    // Root config has NO execution block — only manifest with variants + interfaces.
    // This is the variant-only pattern: the root defines the contract, variants
    // provide the implementation. A "default" variant is required when there is
    // no execution block at root.
    let root_config = r#"{
        schema_version: 1,
        manifest: {
            name: "variant_only_robot",
            tag: "0.1.0",
            variants: [
                { name: "default", source: { local: "mock_node" } },
                { name: "mock", source: { local: "mock_node" } }
            ]
        },
        interfaces: {
            topics: {
                emits: [
                    { name: "joint_positions", qos_profile: "sensor_data", message_format: { x: "f64", y: "f64" } }
                ]
            }
        }
    }"#;
    let root_config_path = root_dir.join(NODE_CONFIG_FILE);
    std::fs::write(&root_config_path, root_config).expect("failed to write root config");

    // Variant config defines execution (the implementation)
    let variant_config = r#"{
        schema_version: 1,
        execution: {
            language: "rust",
            run_cmd: ["sleep", "5"]
        }
    }"#;
    let variant_config_path = variant_dir.join(NODE_CONFIG_FILE);
    std::fs::write(&variant_config_path, variant_config).expect("failed to write variant config");

    // Step 1: Run node sync — generates peppygen only for the variant (not root,
    // since root has no execution block).
    let sync_response = NodeSyncRequest::new(&root_dir, TEST_GIT_HASH, vec![])
        .poll(
            &started_core_node.caller_handle,
            &started_core_node.core_node_name,
            CALLER_INSTANCE_ID,
            &started_core_node.core_node_name,
            Duration::from_secs(10),
        )
        .await
        .expect("node_sync request should complete");

    assert!(
        sync_response.success,
        "node_sync should succeed, got error: {}",
        sync_response.error_message
    );

    // Root should have .peppy/git.hash (sync always writes it alongside the
    // manifest) but no peppygen output (no execution block at root).
    let root_peppy_dir = root_dir.join(PEPPY_OUTPUT_DIR);
    assert!(
        root_peppy_dir.exists(),
        "root .peppy directory should exist after sync (git.hash lives here)"
    );
    assert!(
        root_peppy_dir.join("git.hash").exists(),
        "root .peppy/git.hash should exist after sync"
    );
    assert!(
        !root_dir.join(PEPPYGEN_OUTPUT_PATH).exists(),
        "root should NOT have peppygen output (no execution block)"
    );

    // Variant should have .peppy directory after sync.
    assert!(
        variant_dir.join(PEPPY_OUTPUT_DIR).exists(),
        "variant .peppy directory should exist after sync"
    );

    // Step 2: Run node add with the variant — must succeed despite no .peppy variant file at the repo root.
    let add_result = send_node_add_and_wait_with_variant(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        root_dir.as_path(),
        "mock",
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add with variant should succeed");

    assert!(
        add_result.success,
        "variant-only node_add after sync should succeed, got error: {:?}",
        add_result.error_message
    );

    // Verify the node is in the stack with the expected merged config
    assert!(node_stack.contains(ROOT_NODE_NAME, ROOT_NODE_TAG));

    let entity = node_stack
        .find(ROOT_NODE_NAME, ROOT_NODE_TAG)
        .expect("node should exist in stack");
    let entity_guard = entity.read();
    let config = entity_guard.config();
    assert!(
        config.interfaces.topics.is_some(),
        "interfaces should be inherited from root"
    );
    assert_eq!(
        config.execution.run_cmd.as_ref().unwrap(),
        &vec!["sleep".to_string(), "5".to_string()],
        "execution should come from the variant"
    );
}
/// After sync, modifying the variant's peppy.json5 must cause a fingerprint
/// mismatch on the next `node add`, blocking the stale variant from being added.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_variant_fingerprint_mismatch_after_sync() {
    use core_node::encoding::NodeSyncRequest;

    const ROOT_NODE_NAME: &str = "stale_variant_robot";
    const ROOT_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let parent_dir = tempfile::tempdir().expect("failed to create parent dir");
    let root_dir = parent_dir.path().join("root_node");
    let variant_dir = root_dir.join("mock_node");
    std::fs::create_dir_all(&root_dir).unwrap();
    std::fs::create_dir_all(&variant_dir).unwrap();

    let root_config = r#"{
        schema_version: 1,
        manifest: {
            name: "stale_variant_robot",
            tag: "0.1.0",
            variants: [
                { name: "mock", source: { local: "mock_node" } }
            ]
        },
        interfaces: {
            topics: {
                emits: [
                    { name: "joint_positions", qos_profile: "sensor_data", message_format: { x: "f64", y: "f64" } }
                ]
            }
        },
        execution: {
            language: "rust",
            run_cmd: ["sleep", "10"]
        }
    }"#;
    std::fs::write(root_dir.join(NODE_CONFIG_FILE), root_config)
        .expect("failed to write root config");

    let variant_config = r#"{
        schema_version: 1,
        execution: {
            language: "rust",
            run_cmd: ["sleep", "5"]
        }
    }"#;
    std::fs::write(variant_dir.join(NODE_CONFIG_FILE), variant_config)
        .expect("failed to write variant config");

    // Step 1: Sync — generates peppygen and fingerprint for both root and variant.
    let sync_response = NodeSyncRequest::new(&root_dir, TEST_GIT_HASH, vec![])
        .poll(
            &started_core_node.caller_handle,
            &started_core_node.core_node_name,
            CALLER_INSTANCE_ID,
            &started_core_node.core_node_name,
            Duration::from_secs(10),
        )
        .await
        .expect("node_sync request should complete");

    assert!(
        sync_response.success,
        "node_sync should succeed, got error: {}",
        sync_response.error_message
    );

    // Step 2: Modify the variant config after sync (simulates user editing without re-syncing).
    let modified_variant_config = r#"{
        schema_version: 1,
        execution: {
            language: "rust",
            run_cmd: ["sleep", "99"]
        }
    }"#;
    std::fs::write(variant_dir.join(NODE_CONFIG_FILE), modified_variant_config)
        .expect("failed to write modified variant config");

    // Step 3: node add should fail — fingerprint no longer matches.
    let add_result = send_node_add_and_wait_with_variant(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        root_dir.as_path(),
        "mock",
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should complete");

    assert!(
        !add_result.success,
        "node_add should fail when variant config was modified after sync"
    );
    assert!(
        add_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("Codegen fingerprint verification failed"))
            .unwrap_or(false),
        "error should indicate fingerprint verification failure, got: {:?}",
        add_result.error_message
    );

    // Node should not be in the stack
    assert!(
        !node_stack.contains(ROOT_NODE_NAME, ROOT_NODE_TAG),
        "node should not be added when variant fingerprint mismatches"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_with_fs_archive_variant_uses_archived_root() {
    const ROOT_NODE_NAME: &str = "archive_robot_brain";
    const ROOT_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let bundle_dir = tempfile::tempdir().expect("failed to create temp bundle dir");
    let archived_root_dir = bundle_dir.path().join("archived_root");
    let archived_variant_dir = archived_root_dir.join("mock_node");
    std::fs::create_dir_all(&archived_variant_dir).expect("failed to create archived variant dir");

    let root_config = r#"{
        schema_version: 1,
        manifest: {
            name: "archive_robot_brain",
            tag: "0.1.0",
            variants: [
                { name: "mock", source: { local: "./mock_node" } }
            ]
        },
        interfaces: {
            topics: {
                emits: [
                    { name: "joint_positions", qos_profile: "sensor_data", message_format: { x: "f64", y: "f64" } }
                ]
            }
        },
        execution: {
            language: "rust",
            run_cmd: ["sleep", "10"]
        }
    }"#;
    write_peppy_json5(&archived_root_dir, root_config);
    let peppy_dir = archived_root_dir.join(PEPPY_OUTPUT_DIR);
    std::fs::create_dir_all(&peppy_dir).expect("failed to create peppy output dir");
    std::fs::write(peppy_dir.join("git.hash"), TEST_GIT_HASH).expect("failed to write git hash");

    write_peppy_json5(
        &archived_variant_dir,
        r#"{
            schema_version: 1,
            execution: {
                language: "rust",
                run_cmd: ["sleep", "5"]
            }
        }"#,
    );

    let host_decoy_variant_dir = bundle_dir.path().join("mock_node");
    std::fs::create_dir_all(&host_decoy_variant_dir)
        .expect("failed to create host decoy variant dir");
    write_peppy_json5(
        &host_decoy_variant_dir,
        r#"{
            schema_version: 1,
            execution: {
                language: "rust",
                run_cmd: ["sleep", "99"]
            }
        }"#,
    );

    let bundle_path = bundle_dir.path().join("archive_robot_brain.tar.zst");
    create_tar_zst_from_dir(&archived_root_dir, &bundle_path, "root_node");

    let add_result = send_node_add_and_wait_with_variant(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        bundle_path.as_path(),
        "mock",
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add with archive variant should succeed");

    assert!(
        add_result.success,
        "archive variant node_add should succeed, got error: {:?}",
        add_result.error_message
    );
    assert!(node_stack.contains(ROOT_NODE_NAME, ROOT_NODE_TAG));

    let entity = node_stack
        .find(ROOT_NODE_NAME, ROOT_NODE_TAG)
        .expect("node should exist in stack");
    let entity_guard = entity.read();
    let config = entity_guard.config();
    assert!(
        config.interfaces.topics.is_some(),
        "interfaces should be inherited from root"
    );
    assert_eq!(
        config.execution.run_cmd.as_ref().unwrap(),
        &vec!["sleep".to_string(), "5".to_string()],
        "execution should come from the archived variant, not the host decoy"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_variant_not_found() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let parent_dir = tempfile::tempdir().expect("failed to create parent dir");
    let root_dir = parent_dir.path().join("root_node");
    let variant_dir = root_dir.join("real_variant");
    std::fs::create_dir_all(&root_dir).unwrap();
    std::fs::create_dir_all(&variant_dir).unwrap();

    let root_config = r#"{
        schema_version: 1,
        manifest: {
            name: "test_node",
            tag: "0.1.0",
            variants: [
                { name: "real", source: { local: "real_variant" } }
            ]
        },
        execution: {
            language: "rust",
            run_cmd: ["sleep", "10"]
        }
    }"#;
    write_peppy_json5(&root_dir, root_config);

    let variant_config = r#"{
        schema_version: 1,
        execution: {
            language: "rust",
            run_cmd: ["sleep", "5"]
        }
    }"#;
    write_peppy_json5(&variant_dir, variant_config);

    let add_result = send_node_add_and_wait_with_variant(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        root_dir.as_path(),
        "nonexistent",
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("request should complete");

    assert!(
        !add_result.success,
        "node_add should fail for nonexistent variant"
    );
    assert!(
        add_result
            .error_message
            .as_deref()
            .unwrap_or("")
            .contains("variant 'nonexistent' not found"),
        "error should mention the missing variant: {:?}",
        add_result.error_message
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_variant_interface_mismatch() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let parent_dir = tempfile::tempdir().expect("failed to create parent dir");
    let root_dir = parent_dir.path().join("root_node");
    let variant_dir = root_dir.join("bad_variant");
    std::fs::create_dir_all(&root_dir).unwrap();
    std::fs::create_dir_all(&variant_dir).unwrap();

    let root_config = r#"{
        schema_version: 1,
        manifest: {
            name: "test_node",
            tag: "0.1.0",
            variants: [
                { name: "bad", source: { local: "bad_variant" } }
            ]
        },
        interfaces: {
            topics: {
                emits: [
                    { name: "sensor_data", message_format: { temperature: "f64" } }
                ]
            }
        },
        execution: {
            language: "rust",
            run_cmd: ["sleep", "10"]
        }
    }"#;
    write_peppy_json5(&root_dir, root_config);

    // Variant defines DIFFERENT interfaces
    let variant_config = r#"{
        schema_version: 1,
        interfaces: {
            topics: {
                emits: [
                    { name: "different_topic", message_format: { speed: "f32" } }
                ]
            }
        },
        execution: {
            language: "rust",
            run_cmd: ["sleep", "5"]
        }
    }"#;
    write_peppy_json5(&variant_dir, variant_config);

    let add_result = send_node_add_and_wait_with_variant(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        root_dir.as_path(),
        "bad",
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("request should complete");

    assert!(
        !add_result.success,
        "node_add should fail for interface mismatch"
    );
    assert!(
        add_result
            .error_message
            .as_deref()
            .unwrap_or("")
            .contains("VariantInterfaceMismatch"),
        "error should mention interface mismatch: {:?}",
        add_result.error_message
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_variant_matching_interfaces_different_order() {
    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let parent_dir = tempfile::tempdir().expect("failed to create parent dir");
    let root_dir = parent_dir.path().join("root_node");
    let variant_dir = root_dir.join("good_variant");
    std::fs::create_dir_all(&root_dir).unwrap();
    std::fs::create_dir_all(&variant_dir).unwrap();

    let root_config = r#"{
        schema_version: 1,
        manifest: {
            name: "test_node",
            tag: "0.1.0",
            variants: [
                { name: "good", source: { local: "good_variant" } }
            ]
        },
        interfaces: {
            topics: {
                emits: [
                    { name: "topic_a", message_format: { x: "f64", y: "f64" } },
                    { name: "topic_b" }
                ]
            }
        },
        execution: {
            language: "rust",
            run_cmd: ["sleep", "10"]
        }
    }"#;
    write_peppy_json5(&root_dir, root_config);

    // Variant defines the SAME interfaces but in different order
    let variant_config = r#"{
        schema_version: 1,
        interfaces: {
            topics: {
                emits: [
                    { name: "topic_b" },
                    { name: "topic_a", message_format: { y: "f64", x: "f64" } }
                ]
            }
        },
        execution: {
            language: "rust",
            run_cmd: ["sleep", "5"]
        }
    }"#;
    write_peppy_json5(&variant_dir, variant_config);

    let add_result = send_node_add_and_wait_with_variant(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        root_dir.as_path(),
        "good",
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add with matching interfaces should succeed");

    assert!(
        add_result.success,
        "variant with matching interfaces (different order) should succeed, got error: {:?}",
        add_result.error_message
    );
    assert!(node_stack.contains("test_node", "0.1.0"));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_variant_no_interfaces() {
    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let parent_dir = tempfile::tempdir().expect("failed to create parent dir");
    let root_dir = parent_dir.path().join("root_node");
    let variant_dir = root_dir.join("minimal_variant");
    std::fs::create_dir_all(&root_dir).unwrap();
    std::fs::create_dir_all(&variant_dir).unwrap();

    let root_config = r#"{
        schema_version: 1,
        manifest: {
            name: "test_node",
            tag: "0.1.0",
            variants: [
                { name: "minimal", source: { local: "minimal_variant" } }
            ]
        },
        interfaces: {
            topics: {
                emits: [
                    { name: "data", message_format: { value: "f64" } }
                ]
            }
        },
        execution: {
            language: "rust",
            run_cmd: ["sleep", "10"]
        }
    }"#;
    write_peppy_json5(&root_dir, root_config);

    // Variant has NO interfaces (omitted entirely) — should be accepted
    let variant_config = r#"{
        schema_version: 1,
        execution: {
            language: "rust",
            run_cmd: ["sleep", "5"]
        }
    }"#;
    write_peppy_json5(&variant_dir, variant_config);

    let add_result = send_node_add_and_wait_with_variant(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        root_dir.as_path(),
        "minimal",
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add with variant without interfaces should succeed");

    assert!(
        add_result.success,
        "variant without interfaces should succeed, got error: {:?}",
        add_result.error_message
    );
    assert!(node_stack.contains("test_node", "0.1.0"));

    let entity = node_stack
        .find("test_node", "0.1.0")
        .expect("node should exist");
    assert!(
        entity.read().config().interfaces.topics.is_some(),
        "root interfaces should be used even when variant has none"
    );
}
/// Verifies that `NodeAddGoal` encode/decode roundtrips are lossless for every
/// `NodeSource` variant (`Fs`, `Git`, `Http`) used as either the primary source
/// or as a variant, as well as the case where no variant is set.
#[test]
fn listen_for_node_add_variant_encoding_roundtrip() {
    use core_node::encoding::NodeSource;

    // Name-based variant (Fs)
    let goal = NodeAddGoal::new("/some/path", "test-hash", 60).with_variant_name("mock");
    let encoded = goal.encode().expect("encoding should succeed");
    let decoded = NodeAddGoal::decode(&encoded).expect("decoding should succeed");
    assert!(
        matches!(&decoded.variant, Some(NodeSource::Fs(p)) if p.to_string_lossy() == "mock"),
        "expected Fs(\"mock\"), got {:?}",
        decoded.variant
    );
    assert_eq!(decoded.git_hash, "test-hash");
    assert_eq!(decoded.timeout_secs, 60);

    // Git-based variant
    let git_url = GitUrl::try_from("https://github.com/example/repo.git").unwrap();
    let goal_git =
        NodeAddGoal::new("/some/path", "test-hash", 60).with_variant_source(NodeSource::Git {
            repo_url: git_url.clone(),
            repo_path: "brain".to_string(),
            repo_ref: Some("main".to_string()),
        });
    let encoded = goal_git.encode().expect("encoding should succeed");
    let decoded = NodeAddGoal::decode(&encoded).expect("decoding should succeed");
    assert!(
        matches!(&decoded.variant, Some(NodeSource::Git { repo_path, repo_ref, .. }) if repo_path == "brain" && repo_ref.as_deref() == Some("main")),
        "expected Git variant, got {:?}",
        decoded.variant
    );

    // Http-based source
    let url = url::Url::parse("https://example.com/node.tar.zst").unwrap();
    let source_sha256 = "a".repeat(64);
    let goal_http_source =
        NodeAddGoal::new_http(url.clone(), Some(source_sha256.clone()), "test-hash", 60);
    let encoded = goal_http_source.encode().expect("encoding should succeed");
    let decoded = NodeAddGoal::decode(&encoded).expect("decoding should succeed");
    assert!(
        matches!(&decoded.source, NodeSource::Http { url: u, sha256 } if u.as_str() == "https://example.com/node.tar.zst" && sha256.as_deref() == Some(source_sha256.as_str())),
        "expected Http source with sha256, got {:?}",
        decoded.source
    );

    // Http-based variant
    let url = url::Url::parse("https://example.com/variant.tar.zst").unwrap();
    let variant_sha256 = "b".repeat(64);
    let goal_http =
        NodeAddGoal::new("/some/path", "test-hash", 60).with_variant_source(NodeSource::Http {
            url: url.clone(),
            sha256: Some(variant_sha256.clone()),
        });
    let encoded = goal_http.encode().expect("encoding should succeed");
    let decoded = NodeAddGoal::decode(&encoded).expect("decoding should succeed");
    assert!(
        matches!(&decoded.variant, Some(NodeSource::Http { url: u, sha256 }) if u.as_str() == "https://example.com/variant.tar.zst" && sha256.as_deref() == Some(variant_sha256.as_str())),
        "expected Http variant, got {:?}",
        decoded.variant
    );

    // Without variant
    let goal_no_variant = NodeAddGoal::new("/some/path", "test-hash", 60);
    let encoded = goal_no_variant.encode().expect("encoding should succeed");
    let decoded = NodeAddGoal::decode(&encoded).expect("decoding should succeed");
    assert_eq!(decoded.variant, None);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_variant_manifest_ignored_warning() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let parent_dir = tempfile::tempdir().expect("failed to create parent dir");
    let root_dir = parent_dir.path().join("root_node");
    let variant_dir = root_dir.join("variant_with_manifest");
    std::fs::create_dir_all(&root_dir).unwrap();
    std::fs::create_dir_all(&variant_dir).unwrap();

    let root_config = r#"{
        schema_version: 1,
        manifest: {
            name: "test_node",
            tag: "0.1.0",
            variants: [
                { name: "custom", source: { local: "variant_with_manifest" } }
            ]
        },
        execution: {
            language: "rust",
            run_cmd: ["sleep", "10"]
        }
    }"#;
    write_peppy_json5(&root_dir, root_config);

    // Variant defines a manifest — it should be ignored with a warning
    let variant_config = r#"{
        schema_version: 1,
        manifest: {
            name: "overridden_name",
            tag: "9.9.9",
        },
        execution: {
            language: "rust",
            run_cmd: ["sleep", "5"]
        }
    }"#;
    write_peppy_json5(&variant_dir, variant_config);

    let (feedback_tx, mut feedback_rx) = tokio::sync::mpsc::unbounded_channel::<NodeAddFeedback>();

    let add_result = send_node_add_and_wait_with_variant(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        root_dir.as_path(),
        "custom",
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        Some(feedback_tx),
    )
    .await
    .expect("node_add with variant should succeed");

    assert!(
        add_result.success,
        "variant node_add should succeed, got error: {:?}",
        add_result.error_message
    );

    // Collect feedback and verify the manifest-ignored warning was emitted
    let mut feedback = Vec::new();
    while let Ok(entry) = feedback_rx.try_recv() {
        feedback.push(entry);
    }

    let has_manifest_warning = feedback.iter().any(|f| {
        f.is_stderr()
            && f.line.contains("manifest")
            && f.line.contains("ignored")
            && f.line.contains("custom")
    });
    assert!(
        has_manifest_warning,
        "should emit a warning about variant manifest being ignored, got feedback: {:?}",
        feedback.iter().map(|f| &f.line).collect::<Vec<_>>()
    );

    // Verify the root manifest was used, not the variant's
    let entity = started_core_node
        .node_stack
        .find("test_node", "0.1.0")
        .expect("node should be in stack under root's name:tag");
    let entity_guard = entity.read();
    assert_eq!(entity_guard.config().manifest.name.as_str(), "test_node");
    assert_eq!(entity_guard.config().manifest.tag, "0.1.0");
}
/// When a root node defines a "default" variant and omits `runtime`, adding
/// the node without `--variant` should auto-resolve the default variant.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_default_variant_auto_resolved() {
    const ROOT_NODE_NAME: &str = "uvc_camera";
    const ROOT_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let parent_dir = tempfile::tempdir().expect("failed to create parent dir");
    let root_dir = parent_dir.path().join("uvc_camera");
    let default_variant_dir = root_dir.join("variants").join("default");
    let mujoco_variant_dir = root_dir.join("variants").join("mujoco");
    std::fs::create_dir_all(&root_dir).unwrap();
    std::fs::create_dir_all(&default_variant_dir).unwrap();
    std::fs::create_dir_all(&mujoco_variant_dir).unwrap();

    // Root config: has a "default" variant, NO runtime
    let root_config = r#"{
        schema_version: 1,
        manifest: {
            name: "uvc_camera",
            tag: "0.1.0",
            variants: [
                { name: "default", source: { local: "./variants/default" } },
                { name: "mujoco", source: { local: "./variants/mujoco" } },
            ]
        },
        interfaces: {
            topics: {
                emits: [
                    { name: "image", qos_profile: "sensor_data", message_format: { width: "u32", height: "u32" } }
                ]
            }
        }
    }"#;
    write_peppy_json5(&root_dir, root_config);

    // Default variant config
    let default_variant_config = r#"{
        schema_version: 1,
        execution: {
            language: "rust",
            run_cmd: ["sleep", "7"]
        }
    }"#;
    write_peppy_json5(&default_variant_dir, default_variant_config);

    // Mujoco variant config
    let mujoco_variant_config = r#"{
        schema_version: 1,
        execution: {
            language: "python",
            run_cmd: ["sleep", "3"]
        }
    }"#;
    write_peppy_json5(&mujoco_variant_dir, mujoco_variant_config);

    // Add WITHOUT specifying a variant — should auto-resolve "default"
    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        root_dir.as_path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add with default variant should succeed");

    assert!(
        add_result.success,
        "default variant node_add should succeed, got error: {:?}",
        add_result.error_message
    );

    assert!(node_stack.contains(ROOT_NODE_NAME, ROOT_NODE_TAG));

    let entity = node_stack
        .find(ROOT_NODE_NAME, ROOT_NODE_TAG)
        .expect("node should exist in stack");
    let entity_guard = entity.read();
    let config = entity_guard.config();
    assert!(
        config.interfaces.topics.is_some(),
        "interfaces should be inherited from root"
    );
    assert_eq!(
        config.execution.run_cmd.as_ref().unwrap(),
        &vec!["sleep".to_string(), "7".to_string()],
        "execution should come from the default variant"
    );
}
/// When a root node has a "default" variant but an explicit `--variant mujoco`
/// is requested, the explicit variant should be used instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_default_variant_explicit_other() {
    const ROOT_NODE_NAME: &str = "uvc_camera2";
    const ROOT_NODE_TAG: &str = "0.2.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let parent_dir = tempfile::tempdir().expect("failed to create parent dir");
    let root_dir = parent_dir.path().join("uvc_camera2");
    let default_variant_dir = parent_dir
        .path()
        .join("uvc_camera2")
        .join("variants")
        .join("default");
    let mujoco_variant_dir = parent_dir
        .path()
        .join("uvc_camera2")
        .join("variants")
        .join("mujoco");
    std::fs::create_dir_all(&root_dir).unwrap();
    std::fs::create_dir_all(&default_variant_dir).unwrap();
    std::fs::create_dir_all(&mujoco_variant_dir).unwrap();

    let root_config = r#"{
        schema_version: 1,
        manifest: {
            name: "uvc_camera2",
            tag: "0.2.0",
            variants: [
                { name: "default", source: { local: "./variants/default" } },
                { name: "mujoco", source: { local: "./variants/mujoco" } },
            ]
        },
        interfaces: {
            topics: {
                emits: [
                    { name: "image", qos_profile: "sensor_data", message_format: { width: "u32", height: "u32" } }
                ]
            }
        }
    }"#;
    write_peppy_json5(&root_dir, root_config);

    let default_variant_config = r#"{
        schema_version: 1,
        execution: {
            language: "rust",
            run_cmd: ["sleep", "7"]
        }
    }"#;
    write_peppy_json5(&default_variant_dir, default_variant_config);

    let mujoco_variant_config = r#"{
        schema_version: 1,
        execution: {
            language: "python",
            run_cmd: ["sleep", "3"]
        }
    }"#;
    write_peppy_json5(&mujoco_variant_dir, mujoco_variant_config);

    // Add with explicit --variant mujoco
    let add_result = send_node_add_and_wait_with_variant(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        root_dir.as_path(),
        "mujoco",
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add with explicit mujoco variant should succeed");

    assert!(
        add_result.success,
        "explicit variant node_add should succeed, got error: {:?}",
        add_result.error_message
    );

    let entity = node_stack
        .find(ROOT_NODE_NAME, ROOT_NODE_TAG)
        .expect("node should exist in stack");
    let entity_guard = entity.read();
    let config = entity_guard.config();
    assert_eq!(
        config.execution.run_cmd.as_ref().unwrap(),
        &vec!["sleep".to_string(), "3".to_string()],
        "execution should come from the mujoco variant, not the default"
    );
}
/// A root config that defines both an `execution` block AND a "default" variant
/// is invalid — execution must come from the default variant, not the root.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_execution_with_default_variant_fails() {
    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let parent_dir = tempfile::tempdir().expect("failed to create parent dir");
    let root_dir = parent_dir.path().join("uvc_camera");
    let default_variant_dir = root_dir.join("variants").join("default");
    let mujoco_variant_dir = root_dir.join("variants").join("mujoco");
    std::fs::create_dir_all(&root_dir).unwrap();
    std::fs::create_dir_all(&default_variant_dir).unwrap();
    std::fs::create_dir_all(&mujoco_variant_dir).unwrap();

    // Root config: has BOTH execution AND a "default" variant — this is invalid.
    let root_config = r#"{
        schema_version: 1,
        manifest: {
            name: "uvc_camera",
            tag: "0.1.0",
            variants: [
                { name: "default", source: { local: "./variants/default" } },
                { name: "mujoco", source: { local: "./variants/mujoco" } },
            ]
        },
        interfaces: {
            topics: {
                emits: [
                    { name: "image", qos_profile: "sensor_data", message_format: { width: "u32", height: "u32" } }
                ]
            }
        },
        execution: {
            language: "rust",
            run_cmd: ["sleep", "7"]
        }
    }"#;
    write_peppy_json5(&root_dir, root_config);

    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        root_dir.as_path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should complete");

    assert!(
        !add_result.success,
        "node_add should fail when both execution and a default variant are defined"
    );
    assert!(
        add_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("execution"))
            .unwrap_or(false),
        "error message should mention execution, got: {:?}",
        add_result.error_message
    );

    assert_eq!(node_stack.len(), 1, "only root should exist");
}
/// When a variant is fetched from a git repository, the cloned temp directory
/// does not contain `.peppy/git.hash`. The git hash verification must fall back
/// to the root source path (where `peppy node sync` wrote the hash file).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_git_variant_verifies_git_hash_at_root() {
    const ROOT_NODE_NAME: &str = "git_variant_hash_robot";
    const ROOT_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    // Create a local git repo containing the variant's execution-only config.
    let git_repo_temp = TempDir::new().expect("failed to create git repo temp dir");
    let git_repo_path = git_repo_temp.path().join("variant_repo.git");
    std::fs::create_dir_all(&git_repo_path).expect("create git repo dir");

    let repo = Repository::init(&git_repo_path).expect("init git repo");
    let signature = Signature::now("Peppy", "peppy@example.com").expect("create signature");

    let variant_config_rel = Path::new(NODE_CONFIG_FILE);
    std::fs::write(
        git_repo_path.join(variant_config_rel),
        r#"{
            schema_version: 1,
            execution: {
                language: "rust",
                run_cmd: ["sleep", "5"]
            }
        }"#,
    )
    .expect("write variant config to git repo");

    let mut index = repo.index().expect("open index");
    index
        .add_path(variant_config_rel)
        .expect("add variant config");
    index.write().expect("write index");

    let tree_id = index.write_tree().expect("write tree");
    let tree = repo.find_tree(tree_id).expect("find tree");
    let commit_id = repo
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            "variant v1",
            &tree,
            &[],
        )
        .expect("commit");
    let commit = repo.find_commit(commit_id).expect("find commit");
    repo.tag("v1.0", commit.as_object(), &signature, "v1.0", false)
        .expect("create v1.0 tag");

    // Build the root node directory.  The manifest declares a variant whose
    // deployment source is the local git repository we just created.
    let parent_dir = tempfile::tempdir().expect("create parent dir");
    let root_dir = parent_dir.path().join("root_node");
    std::fs::create_dir_all(&root_dir).unwrap();

    let root_config = r#"{
        schema_version: 1,
        manifest: {
            name: "ROOT_NODE_NAME",
            tag: "ROOT_NODE_TAG",
            variants: [
                { name: "git_variant", source: { repo: "REPO_PATH", path: ".", ref: "v1.0" } }
            ]
        },
        interfaces: {
            topics: {
                emits: [
                    { name: "joint_positions", qos_profile: "sensor_data", message_format: { x: "f64", y: "f64" } }
                ]
            }
        },
        execution: {
            language: "rust",
            run_cmd: ["sleep", "10"]
        }
    }"#
    .replace("ROOT_NODE_NAME", ROOT_NODE_NAME)
    .replace("ROOT_NODE_TAG", ROOT_NODE_TAG)
    .replace(
        "REPO_PATH",
        &git_repo_path.to_string_lossy().replace('\\', "/"),
    );
    write_peppy_json5(&root_dir, &root_config);

    // The test helper (send_node_add_and_wait_with_variant) auto-provisions
    // .peppy/git.hash at the root.  The git-cloned variant temp directory will
    // NOT have this file
    let add_result = send_node_add_and_wait_with_variant(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        root_dir.as_path(),
        "git_variant",
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should complete");

    assert!(
        add_result.success,
        "git variant node_add should succeed (git hash verified at root, not variant temp dir): {:?}",
        add_result.error_message
    );

    assert!(node_stack.contains(ROOT_NODE_NAME, ROOT_NODE_TAG));

    let entity = node_stack
        .find(ROOT_NODE_NAME, ROOT_NODE_TAG)
        .expect("node should exist in stack");
    let entity_guard = entity.read();
    let config = entity_guard.config();
    assert!(
        config.interfaces.topics.is_some(),
        "interfaces should be inherited from root"
    );
    assert_eq!(
        config.execution.run_cmd.as_ref().unwrap(),
        &vec!["sleep".to_string(), "5".to_string()],
        "execution should come from the git variant"
    );
}
/// `.peppy/git.hash` is always located at the root (alongside the manifest).
/// When a default variant is auto-resolved, the root's git hash must still be
/// verified.  A stale root hash must cause node_add to fail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_default_fs_variant_verifies_git_hash_at_root() {
    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    // Create root node directory with a "default" variant subdirectory.
    let parent_dir = tempfile::tempdir().expect("create parent dir");
    let root_dir = parent_dir.path().join("root_node");
    let variant_dir = root_dir.join("default_impl");
    std::fs::create_dir_all(&root_dir).unwrap();
    std::fs::create_dir_all(&variant_dir).unwrap();

    let root_config = r#"{
        schema_version: 1,
        manifest: {
            name: "default_variant_hash_robot",
            tag: "0.1.0",
            variants: [
                { name: "default", source: { local: "default_impl" } }
            ]
        },
        interfaces: {
            topics: {
                emits: [
                    { name: "joint_positions", qos_profile: "sensor_data", message_format: { x: "f64", y: "f64" } }
                ]
            }
        }
    }"#;
    write_peppy_json5(&root_dir, root_config);

    let variant_config = r#"{
        schema_version: 1,
        execution: {
            language: "rust",
            run_cmd: ["sleep", "5"]
        }
    }"#;
    write_peppy_json5(&variant_dir, variant_config);

    // Pre-provision .peppy/git.hash at root with a STALE value before the
    // test helper runs (it only writes when the file doesn't already exist).
    // This simulates the root being modified after sync without re-syncing.
    let root_peppy_dir = root_dir.join(PEPPY_OUTPUT_DIR);
    std::fs::create_dir_all(&root_peppy_dir).expect("create root .peppy dir");
    std::fs::write(root_peppy_dir.join("git.hash"), "stale-root-hash")
        .expect("write stale root git.hash");

    // No explicit variant — the "default" variant is auto-resolved by node_add.
    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        root_dir.as_path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add request should complete");

    assert!(
        !add_result.success,
        "default variant node_add should FAIL when root git.hash is stale"
    );
    assert!(
        add_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("git hash mismatch"))
            .unwrap_or(false),
        "error should mention git hash mismatch, got: {:?}",
        add_result.error_message
    );
    assert_eq!(
        node_stack.len(),
        1,
        "only the core node should exist (stale root rejected)"
    );
}
