mod common;

use common::{CALLER_INSTANCE_ID, start_core_node_with_mock_messenger};
use config::consts::{NODE_CONFIG_FILE, PEPPY_OUTPUT_DIR, PEPPYGEN_OUTPUT_PATH};
use core_node::transport::NodeSyncRequestPollExt;
use core_node_api::encoding::NodeSyncRequest;
use std::fs;
use std::path::Path;
use std::time::Duration;
use tempfile::tempdir;

fn write_node_config(node_dir: &Path, peppy_json5: &str) {
    let config_path = node_dir.join(NODE_CONFIG_FILE);
    fs::write(&config_path, peppy_json5).expect("failed to write peppy.json5");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_sync_success() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let node_dir = tempdir().expect("failed to create temp node directory");
    write_node_config(
        node_dir.path(),
        r#"{
            schema_version: 1,
            manifest: {
                name: "example_node",
                tag: "0.1.0",
            },
            interfaces: {
                topics: {
                    emits: [],
                    consumes: [],
                },
                services: {
                    exposes: [],
                },
                actions: {
                    exposes: [],
                },
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            }
        }"#,
    );

    let expected_git_hash = "deadbeef";
    let response = NodeSyncRequest::new(node_dir.path(), expected_git_hash, vec![])
        .poll(
            &started_core_node.caller_handle,
            &started_core_node.core_node_name,
            CALLER_INSTANCE_ID,
            &started_core_node.core_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_sync request should complete");

    assert!(
        response.success,
        "node_sync should succeed, got error: {}",
        response.error_message
    );

    let peppygen_dir = node_dir.path().join(PEPPYGEN_OUTPUT_PATH);
    assert!(
        peppygen_dir.exists(),
        "peppygen directory should exist at {}",
        peppygen_dir.display()
    );

    let git_hash_path = node_dir.path().join(PEPPY_OUTPUT_DIR).join("git.hash");
    assert!(
        git_hash_path.exists(),
        "git.hash should exist at {}",
        git_hash_path.display()
    );
    let stored_git_hash = fs::read_to_string(&git_hash_path).expect("failed to read git.hash file");
    assert_eq!(
        stored_git_hash.trim(),
        expected_git_hash,
        "git.hash should contain the sync request git_hash"
    );

    let config_path = node_dir.path().join(NODE_CONFIG_FILE);
    assert!(
        config::fingerprint::read_codegen_fingerprint(&config_path, PEPPYGEN_OUTPUT_PATH).is_ok(),
        "fingerprint file should exist in peppygen directory"
    );

    let cargo_toml_path = node_dir.path().join("Cargo.toml");
    assert!(
        cargo_toml_path.exists(),
        "node Cargo.toml should exist at {}",
        cargo_toml_path.display()
    );
    let cargo_toml = fs::read_to_string(&cargo_toml_path).expect("failed to read node Cargo.toml");
    assert!(
        cargo_toml.contains("peppygen"),
        "Cargo.toml should contain peppygen dependency, got:\n{}",
        cargo_toml
    );
    assert!(
        cargo_toml.contains(PEPPYGEN_OUTPUT_PATH),
        "Cargo.toml should reference generated peppygen path, got:\n{}",
        cargo_toml
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_sync_missing_node_root_dir_fails() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let response = NodeSyncRequest::new("", common::TEST_GIT_HASH, vec![])
        .poll(
            &started_core_node.caller_handle,
            &started_core_node.core_node_name,
            CALLER_INSTANCE_ID,
            &started_core_node.core_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_sync request should complete");

    assert!(!response.success, "node_sync should fail");
    assert!(
        response.error_message.contains("Missing `node_root_dir`"),
        "error should mention missing node_root_dir, got: {}",
        response.error_message
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_sync_node_root_dir_does_not_exist_fails() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let tmp = tempdir().expect("failed to create temp directory");
    let missing_dir = tmp.path().join("does_not_exist");
    assert!(
        !missing_dir.exists(),
        "missing_dir should not exist at {}",
        missing_dir.display()
    );

    let response = NodeSyncRequest::new(missing_dir, common::TEST_GIT_HASH, vec![])
        .poll(
            &started_core_node.caller_handle,
            &started_core_node.core_node_name,
            CALLER_INSTANCE_ID,
            &started_core_node.core_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_sync request should complete");

    assert!(!response.success, "node_sync should fail");
    assert!(
        response
            .error_message
            .contains("`node_root_dir` does not exist"),
        "error should mention missing directory, got: {}",
        response.error_message
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_sync_node_root_dir_is_not_a_directory_fails() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let tmp = tempdir().expect("failed to create temp directory");
    let file_path = tmp.path().join("not_a_directory");
    fs::write(&file_path, "not a dir").expect("failed to write temp file");

    let response = NodeSyncRequest::new(file_path, common::TEST_GIT_HASH, vec![])
        .poll(
            &started_core_node.caller_handle,
            &started_core_node.core_node_name,
            CALLER_INSTANCE_ID,
            &started_core_node.core_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_sync request should complete");

    assert!(!response.success, "node_sync should fail");
    assert!(
        response
            .error_message
            .contains("`node_root_dir` is not a directory"),
        "error should mention not a directory, got: {}",
        response.error_message
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_sync_missing_peppy_json5_fails() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let node_dir = tempdir().expect("failed to create temp node directory");
    let peppygen_dir = node_dir.path().join(PEPPYGEN_OUTPUT_PATH);
    let cargo_toml_path = node_dir.path().join("Cargo.toml");

    let response = NodeSyncRequest::new(node_dir.path(), common::TEST_GIT_HASH, vec![])
        .poll(
            &started_core_node.caller_handle,
            &started_core_node.core_node_name,
            CALLER_INSTANCE_ID,
            &started_core_node.core_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_sync request should complete");

    assert!(!response.success, "node_sync should fail");
    assert!(
        response
            .error_message
            .contains("Node config file does not exist"),
        "error should mention missing node config, got: {}",
        response.error_message
    );

    assert!(
        !peppygen_dir.exists(),
        "peppygen directory should not exist at {}",
        peppygen_dir.display()
    );
    assert!(
        !cargo_toml_path.exists(),
        "node Cargo.toml should not be created at {}",
        cargo_toml_path.display()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_sync_invalid_peppy_json5_fails() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let node_dir = tempdir().expect("failed to create temp node directory");
    write_node_config(node_dir.path(), r#"{ manifest: [unclosed"#);

    let peppygen_dir = node_dir.path().join(PEPPYGEN_OUTPUT_PATH);

    let response = NodeSyncRequest::new(node_dir.path(), common::TEST_GIT_HASH, vec![])
        .poll(
            &started_core_node.caller_handle,
            &started_core_node.core_node_name,
            CALLER_INSTANCE_ID,
            &started_core_node.core_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_sync request should complete");

    assert!(!response.success, "node_sync should fail");
    assert!(
        response
            .error_message
            .contains("Failed to parse node config"),
        "error should mention parse failure, got: {}",
        response.error_message
    );
    assert!(
        !peppygen_dir.exists(),
        "peppygen directory should not exist at {}",
        peppygen_dir.display()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_sync_resolves_dependency_via_local_peers() {
    // Verifies that `local_peers` lets the daemon resolve dependencies that
    // exist on disk but have NOT been registered in the persistent node stack
    // (which is what `peppy node sync -a` relies on).
    let started_core_node = start_core_node_with_mock_messenger().await;

    // Camera node — emits a `video_stream` topic.
    let camera_dir = tempdir().expect("failed to create camera tempdir");
    write_node_config(
        camera_dir.path(),
        r#"
        {
            schema_version: 1,
            manifest: {
                name: "uvc_camera",
                tag: "0.1.0",
                labels: ["camera"],
            },
            interfaces: {
                topics: {
                    emits: [
                      {
                        name: "video_stream",
                        qos_profile: "sensor_data",
                        message_format: {
                            header: {
                              $type: "object",
                              stamp: "time",
                              frame_id: "u32",
                            },
                            encoding: "string",
                            width: "u32",
                            height: "u32",
                            frame: {
                              $type: "array",
                              $items: "u8"
                            },
                        },
                      }
                    ],
                    consumes: [],
                },
                services: { exposes: [] },
                actions: { exposes: [] },
            },
            execution: {
                language: "rust",
                build_cmd: ["true"],
                run_cmd: ["sleep", "10"],
            },
        }
        "#,
    );

    // Brain node — depends on the camera. The camera is NOT added to the
    // node stack; it is supplied only via `local_peers`.
    let brain_dir = tempdir().expect("failed to create brain tempdir");
    write_node_config(
        brain_dir.path(),
        r#"
        {
            schema_version: 1,
            manifest: {
                name: "my_robot_brain",
                tag: "0.1.0",
                labels: ["brain"],
                depends_on: {
                    nodes: [
                        { name: "uvc_camera", tag: "0.1.0", local_id: "uvc_camera" }
                    ]
                },
            },
            interfaces: {
                topics: {
                    emits: [],
                    consumes: [
                        {
                          local_node_id: "uvc_camera",
                          name: "video_stream",
                        }
                    ],
                },
                services: { exposes: [] },
                actions: { exposes: [] },
            },
            execution: {
                language: "rust",
                build_cmd: ["true"],
                run_cmd: ["sleep", "10"],
            },
        }
        "#,
    );

    let response = NodeSyncRequest::new(
        brain_dir.path(),
        common::TEST_GIT_HASH,
        vec![camera_dir.path().to_path_buf()],
    )
    .poll(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        CALLER_INSTANCE_ID,
        &started_core_node.core_node_name,
        Duration::from_secs(5),
    )
    .await
    .expect("node_sync request should complete");

    assert!(
        response.success,
        "brain node_sync should succeed with local peer resolution, got error: {}",
        response.error_message
    );

    // Confirm peppygen was generated for the consumed topic — that file only
    // exists when the resolver successfully fetched the camera's emitted topic
    // metadata via `local_peers`.
    let consumed_topic_path = brain_dir
        .path()
        .join(PEPPYGEN_OUTPUT_PATH)
        .join("src")
        .join("consumed_topics")
        .join("uvc_camera_video_stream.rs");
    assert!(
        consumed_topic_path.exists(),
        "expected generated peppygen file at {}",
        consumed_topic_path.display()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_sync_missing_dependency_fails() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let node_dir = tempdir().expect("failed to create temp node directory");
    // The node subscribes to `video_stream` from `uvc_camera:0.1.0`, but this node doesn't exist in the node stack
    // so the generation fails since it can't sync the Rust interfaces
    write_node_config(
        node_dir.path(),
        r#"
        {
            schema_version: 1,
            manifest: {
                name: "my_robot_brain",
                tag: "0.1.0",
                labels: ["brain"],
                depends_on: {
                    nodes: [
                        { name: "uvc_camera", tag: "0.1.0", local_id: "uvc_camera" }
                    ]
                },
            },
            interfaces: {
                topics: {
                    emits: [],
                    consumes: [
                        {
                            local_node_id: "uvc_camera",
                            name: "video_stream",
                        },
                        {
                            local_node_id: "uvc_camera",
                            name: "video_stream_rear",
                        },
                    ],
                },
                services: {
                    exposes: [],
                },
                actions: {
                    exposes: [],
                },
            },
            execution: {
                language: "rust",
                build_cmd: ["cargo", "build", "--release"],
                run_cmd: ["./target/release/my_robot_brain"],
            },
        }
        "#,
    );

    let peppygen_dir = node_dir.path().join(PEPPYGEN_OUTPUT_PATH);

    let response = NodeSyncRequest::new(node_dir.path(), common::TEST_GIT_HASH, vec![])
        .poll(
            &started_core_node.caller_handle,
            &started_core_node.core_node_name,
            CALLER_INSTANCE_ID,
            &started_core_node.core_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_sync request should complete");

    assert!(!response.success, "node_sync should fail");
    assert!(
        response.error_message.contains(
            "my_robot_brain:0.1.0 depends on `uvc_camera:0.1.0`, but it does not exist in the stack"
        ),
        "error should mention missing dependency, got: {}",
        response.error_message
    );

    assert!(
        !peppygen_dir.exists(),
        "peppygen directory should not exist at {}",
        peppygen_dir.display()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_sync_multiple_missing_dependencies_fails() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let node_dir = tempdir().expect("failed to create temp node directory");
    // The node subscribes to topics from multiple non-existent nodes, including
    // duplicate subscriptions to the same node (uvc_camera appears twice)
    write_node_config(
        node_dir.path(),
        r#"
        {
            schema_version: 1,
            manifest: {
                name: "my_robot_brain",
                tag: "0.1.0",
                labels: ["brain"],
                depends_on: {
                    nodes: [
                        { name: "uvc_camera", tag: "0.1.0", local_id: "uvc_camera" },
                        { name: "uvc_camera", tag: "0.1.0", local_id: "uvc_camera_2" },
                        { name: "lidar_sensor", tag: "1.0.0", local_id: "lidar_sensor" },
                        { name: "gps_module", tag: "2.0.0", local_id: "gps_module" },
                    ]
                },
            },
            interfaces: {},
            execution: {
                language: "rust",
                build_cmd: ["cargo", "build", "--release"],
                run_cmd: ["./target/release/my_robot_brain"],
            },
        }
        "#,
    );

    let peppygen_dir = node_dir.path().join(PEPPYGEN_OUTPUT_PATH);

    let response = NodeSyncRequest::new(node_dir.path(), common::TEST_GIT_HASH, vec![])
        .poll(
            &started_core_node.caller_handle,
            &started_core_node.core_node_name,
            CALLER_INSTANCE_ID,
            &started_core_node.core_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_sync request should complete");

    assert!(!response.success, "node_sync should fail");
    // The error message should contain all three unique missing dependencies
    assert!(
        response.error_message.contains("uvc_camera:0.1.0"),
        "error should mention uvc_camera dependency, got: {}",
        response.error_message
    );
    assert!(
        response.error_message.contains("lidar_sensor:1.0.0"),
        "error should mention lidar_sensor dependency, got: {}",
        response.error_message
    );
    assert!(
        response.error_message.contains("gps_module:2.0.0"),
        "error should mention gps_module dependency, got: {}",
        response.error_message
    );
    assert!(
        response.error_message.contains("they do not exist"),
        "error should use plural form for multiple missing dependencies, got: {}",
        response.error_message
    );
    // Verify deduplication: uvc_camera should only appear once despite two subscriptions
    assert_eq!(
        response.error_message.matches("uvc_camera:0.1.0").count(),
        1,
        "uvc_camera:0.1.0 should appear exactly once (deduplicated), got: {}",
        response.error_message
    );

    assert!(
        !peppygen_dir.exists(),
        "peppygen directory should not exist at {}",
        peppygen_dir.display()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_sync_generates_rust_interfaces() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let uvc_camera_node_dir = tempdir().expect("failed to create temp node directory");
    write_node_config(
        uvc_camera_node_dir.path(),
        r#"
        {
            schema_version: 1,
            manifest: {
                name: "uvc_camera",
                tag: "0.1.0",
                labels: ["camera"],
            },
            interfaces: {
                topics: {
                    emits: [
                      {
                        name: "video_stream",
                        qos_profile: "sensor_data",
                        message_format: {
                            header: {
                              $type: "object",
                              stamp: "time",
                              frame_id: "u32",
                            },
                            encoding: "string",
                            width: "u32",
                            height: "u32",
                            frame: {
                              $type: "array",
                              $items: "u8"
                            },
                        },
                      }
                    ],
                    consumes: [],
                },
                services: {
                    exposes: [],
                },
                actions: {
                    exposes: [],
                },
            },
            execution: {
                language: "rust",
                build_cmd: ["true"],
                run_cmd: ["sleep", "10"],
            },
        }
        "#,
    );

    // Generate peppygen for the uvc_camera node first
    let uvc_camera_response =
        NodeSyncRequest::new(uvc_camera_node_dir.path(), common::TEST_GIT_HASH, vec![])
            .poll(
                &started_core_node.caller_handle,
                &started_core_node.core_node_name,
                CALLER_INSTANCE_ID,
                &started_core_node.core_node_name,
                Duration::from_secs(5),
            )
            .await
            .expect("node_sync request should complete");

    assert!(
        uvc_camera_response.success,
        "uvc_camera node_sync should succeed, got error: {}",
        uvc_camera_response.error_message
    );

    // Add uvc_camera to the node stack so the brain node can depend on it
    common::write_peppy_json5(
        uvc_camera_node_dir.path(),
        &fs::read_to_string(uvc_camera_node_dir.path().join(NODE_CONFIG_FILE))
            .expect("failed to read uvc_camera config"),
    );
    let add_result = common::send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        uvc_camera_node_dir.path(),
        Duration::from_secs(5),
        Duration::from_secs(10),
        None,
    )
    .await
    .expect("node_add should succeed");

    assert!(
        add_result.success,
        "uvc_camera node_add should succeed, got error: {}",
        add_result.error_message.unwrap_or_default()
    );

    // The second node depends on the first, and it should work since the first node is now in the node stack
    let brain_node_dir = tempdir().expect("failed to create temp node directory");
    write_node_config(
        brain_node_dir.path(),
        r#"
        {
            schema_version: 1,
            manifest: {
                name: "my_robot_brain",
                tag: "0.1.0",
                labels: ["brain"],
                depends_on: {
                    nodes: [
                        { name: "uvc_camera", tag: "0.1.0", local_id: "uvc_camera" }
                    ]
                },
            },
            interfaces: {
                topics: {
                    emits: [],
                    consumes: [
                        {
                          local_node_id: "uvc_camera",
                          name: "video_stream",
                        }
                    ],
                },
                services: {
                    exposes: [],
                },
                actions: {
                    exposes: [],
                },
            },
            execution: {
                language: "rust",
                build_cmd: ["true"],
                run_cmd: ["sleep", "10"],
            },
        }
        "#,
    );

    // Generate the brain node - this should succeed now that uvc_camera is in the stack
    let brain_response = NodeSyncRequest::new(brain_node_dir.path(), common::TEST_GIT_HASH, vec![])
        .poll(
            &started_core_node.caller_handle,
            &started_core_node.core_node_name,
            CALLER_INSTANCE_ID,
            &started_core_node.core_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_sync request should complete");

    assert!(
        brain_response.success,
        "my_robot_brain node_sync should succeed, got error: {}",
        brain_response.error_message
    );

    // Verify that peppygen was generated for the brain node
    let brain_peppygen_dir = brain_node_dir.path().join(PEPPYGEN_OUTPUT_PATH);
    assert!(
        brain_peppygen_dir.exists(),
        "peppygen directory should exist at {}",
        brain_peppygen_dir.display()
    );

    // Verify that Cargo.toml was created with peppygen dependency
    let brain_cargo_toml_path = brain_node_dir.path().join("Cargo.toml");
    assert!(
        brain_cargo_toml_path.exists(),
        "Cargo.toml should exist at {}",
        brain_cargo_toml_path.display()
    );

    let brain_cargo_toml =
        fs::read_to_string(&brain_cargo_toml_path).expect("failed to read Cargo.toml");
    assert!(
        brain_cargo_toml.contains("peppygen"),
        "Cargo.toml should contain peppygen dependency, got:\n{}",
        brain_cargo_toml
    );

    // Verify that the generated Rust code includes the expected modules
    let brain_lib_rs_path = brain_peppygen_dir.join("src").join("lib.rs");
    assert!(
        brain_lib_rs_path.exists(),
        "peppygen lib.rs should exist at {}",
        brain_lib_rs_path.display()
    );

    let brain_lib_rs = fs::read_to_string(&brain_lib_rs_path).expect("failed to read lib.rs");
    // The generated code should include standard peppygen modules
    assert!(
        brain_lib_rs.contains("pub mod consumed_topics"),
        "lib.rs should contain consumed_topics module, got:\n{}",
        brain_lib_rs
    );
    assert!(
        brain_lib_rs.contains("NodeBuilder"),
        "lib.rs should re-export NodeBuilder, got:\n{}",
        brain_lib_rs
    );

    let consumed_topic_path = brain_peppygen_dir
        .join("src")
        .join("consumed_topics")
        .join("uvc_camera_video_stream.rs");
    assert!(
        consumed_topic_path.exists(),
        "peppygen uvc_camera_video_stream.rs should exist at {}",
        consumed_topic_path.display()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_sync_generates_rust_consumed_service_interfaces() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let uvc_camera_node_dir = tempdir().expect("failed to create temp node directory");
    write_node_config(
        uvc_camera_node_dir.path(),
        r#"
        {
            schema_version: 1,
            manifest: {
                name: "uvc_camera",
                tag: "0.1.0",
                labels: ["camera"],
            },
            interfaces: {
                topics: {
                    emits: [],
                },
                services: {
                    exposes: [
                      {
                        name: "enable_camera",
                        request_message_format: {
                          enable: "bool",
                        },
                      }
                    ],
                },
                actions: {
                    exposes: [],
                },
            },
            execution: {
                language: "rust",
                build_cmd: ["true"],
                run_cmd: ["sleep", "10"],
            },
        }
        "#,
    );

    let uvc_camera_response =
        NodeSyncRequest::new(uvc_camera_node_dir.path(), common::TEST_GIT_HASH, vec![])
            .poll(
                &started_core_node.caller_handle,
                &started_core_node.core_node_name,
                CALLER_INSTANCE_ID,
                &started_core_node.core_node_name,
                Duration::from_secs(5),
            )
            .await
            .expect("node_sync request should complete");

    assert!(
        uvc_camera_response.success,
        "uvc_camera node_sync should succeed, got error: {}",
        uvc_camera_response.error_message
    );

    common::write_peppy_json5(
        uvc_camera_node_dir.path(),
        &fs::read_to_string(uvc_camera_node_dir.path().join(NODE_CONFIG_FILE))
            .expect("failed to read uvc_camera config"),
    );
    let add_result = common::send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        uvc_camera_node_dir.path(),
        Duration::from_secs(5),
        Duration::from_secs(10),
        None,
    )
    .await
    .expect("node_add should succeed");

    assert!(
        add_result.success,
        "uvc_camera node_add should succeed, got error: {}",
        add_result.error_message.unwrap_or_default()
    );

    let brain_node_dir = tempdir().expect("failed to create temp node directory");
    write_node_config(
        brain_node_dir.path(),
        r#"
        {
            schema_version: 1,
            manifest: {
                name: "my_robot_brain",
                tag: "0.1.0",
                labels: ["brain"],
                depends_on: {
                    nodes: [
                        { name: "uvc_camera", tag: "0.1.0", local_id: "uvc_camera" }
                    ]
                },
            },
            interfaces: {
                topics: {
                    emits: [],
                },
                services: {
                    exposes: [],
                    consumes: [
                        {
                          local_node_id: "uvc_camera",
                          name: "enable_camera",
                        }
                    ],
                },
                actions: {
                    exposes: [],
                },
            },
            execution: {
                language: "rust",
                build_cmd: ["true"],
                run_cmd: ["sleep", "10"],
            },
        }
        "#,
    );

    let brain_response = NodeSyncRequest::new(brain_node_dir.path(), common::TEST_GIT_HASH, vec![])
        .poll(
            &started_core_node.caller_handle,
            &started_core_node.core_node_name,
            CALLER_INSTANCE_ID,
            &started_core_node.core_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_sync request should complete");

    assert!(
        brain_response.success,
        "my_robot_brain node_sync should succeed, got error: {}",
        brain_response.error_message
    );

    let brain_peppygen_dir = brain_node_dir.path().join(PEPPYGEN_OUTPUT_PATH);
    assert!(
        brain_peppygen_dir.exists(),
        "peppygen directory should exist at {}",
        brain_peppygen_dir.display()
    );

    let consumed_service_path = brain_peppygen_dir
        .join("src")
        .join("consumed_services")
        .join("uvc_camera_enable_camera.rs");
    assert!(
        consumed_service_path.exists(),
        "peppygen uvc_camera_enable_camera.rs should exist at {}",
        consumed_service_path.display()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_sync_generates_rust_consumed_topic_interfaces() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let uvc_camera_node_dir = tempdir().expect("failed to create temp node directory");
    write_node_config(
        uvc_camera_node_dir.path(),
        r#"
        {
            schema_version: 1,
            manifest: {
                name: "uvc_camera",
                tag: "0.1.0",
                labels: ["camera"],
            },
            interfaces: {
                topics: {
                    emits: [
                      {
                        name: "video_stream",
                        qos_profile: "sensor_data",
                        message_format: {
                          width: "u32",
                          height: "u32",
                        },
                      }
                    ],
                },
                services: {
                    exposes: [],
                },
                actions: {
                    exposes: [],
                },
            },
            execution: {
                language: "rust",
                build_cmd: ["true"],
                run_cmd: ["sleep", "10"],
            },
        }
        "#,
    );

    let uvc_camera_response =
        NodeSyncRequest::new(uvc_camera_node_dir.path(), common::TEST_GIT_HASH, vec![])
            .poll(
                &started_core_node.caller_handle,
                &started_core_node.core_node_name,
                CALLER_INSTANCE_ID,
                &started_core_node.core_node_name,
                Duration::from_secs(5),
            )
            .await
            .expect("node_sync request should complete");

    assert!(
        uvc_camera_response.success,
        "uvc_camera node_sync should succeed, got error: {}",
        uvc_camera_response.error_message
    );

    common::write_peppy_json5(
        uvc_camera_node_dir.path(),
        &fs::read_to_string(uvc_camera_node_dir.path().join(NODE_CONFIG_FILE))
            .expect("failed to read uvc_camera config"),
    );
    let add_result = common::send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        uvc_camera_node_dir.path(),
        Duration::from_secs(5),
        Duration::from_secs(10),
        None,
    )
    .await
    .expect("node_add should succeed");

    assert!(
        add_result.success,
        "uvc_camera node_add should succeed, got error: {}",
        add_result.error_message.unwrap_or_default()
    );

    let brain_node_dir = tempdir().expect("failed to create temp node directory");
    write_node_config(
        brain_node_dir.path(),
        r#"
        {
            schema_version: 1,
            manifest: {
                name: "my_robot_brain",
                tag: "0.1.0",
                labels: ["brain"],
                depends_on: {
                    nodes: [
                        { name: "uvc_camera", tag: "0.1.0", local_id: "uvc_camera" }
                    ]
                },
            },
            interfaces: {
                topics: {
                    emits: [],
                    consumes: [
                        {
                          local_node_id: "uvc_camera",
                          name: "video_stream",
                        }
                    ],
                },
                services: {
                    exposes: [],
                },
                actions: {
                    exposes: [],
                },
            },
            execution: {
                language: "rust",
                build_cmd: ["true"],
                run_cmd: ["sleep", "10"],
            },
        }
        "#,
    );

    let brain_response = NodeSyncRequest::new(brain_node_dir.path(), common::TEST_GIT_HASH, vec![])
        .poll(
            &started_core_node.caller_handle,
            &started_core_node.core_node_name,
            CALLER_INSTANCE_ID,
            &started_core_node.core_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_sync request should complete");

    assert!(
        brain_response.success,
        "my_robot_brain node_sync should succeed, got error: {}",
        brain_response.error_message
    );

    let brain_peppygen_dir = brain_node_dir.path().join(PEPPYGEN_OUTPUT_PATH);
    assert!(
        brain_peppygen_dir.exists(),
        "peppygen directory should exist at {}",
        brain_peppygen_dir.display()
    );

    let consumed_topic_path = brain_peppygen_dir
        .join("src")
        .join("consumed_topics")
        .join("uvc_camera_video_stream.rs");
    assert!(
        consumed_topic_path.exists(),
        "peppygen uvc_camera_video_stream.rs should exist at {}",
        consumed_topic_path.display()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_sync_generates_rust_consumed_action_interfaces() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let action_server_node_dir = tempdir().expect("failed to create temp node directory");
    write_node_config(
        action_server_node_dir.path(),
        r#"
        {
            schema_version: 1,
            manifest: {
                name: "brain",
                tag: "0.1.0",
                labels: ["brain"],
            },
            interfaces: {
                topics: {
                    emits: [],
                },
                services: {
                    exposes: [],
                },
                actions: {
                    exposes: [
                      {
                        name: "move_arm",
                        goal_service: {
                          request_message_format: { value: "u32" },
                          response_message_format: { accepted: "bool" },
                        },
                        feedback_topic: {
                          qos_profile: "sensor_data",
                          message_format: { progress: "u8" },
                        },
                        result_service: {
                          response_message_format: { success: "bool" },
                        },
                      }
                    ],
                },
            },
            execution: {
                language: "rust",
                build_cmd: ["true"],
                run_cmd: ["sleep", "10"],
            },
        }
        "#,
    );

    let action_server_response =
        NodeSyncRequest::new(action_server_node_dir.path(), common::TEST_GIT_HASH, vec![])
            .poll(
                &started_core_node.caller_handle,
                &started_core_node.core_node_name,
                CALLER_INSTANCE_ID,
                &started_core_node.core_node_name,
                Duration::from_secs(5),
            )
            .await
            .expect("node_sync request should complete");

    assert!(
        action_server_response.success,
        "brain node_sync should succeed, got error: {}",
        action_server_response.error_message
    );

    common::write_peppy_json5(
        action_server_node_dir.path(),
        &fs::read_to_string(action_server_node_dir.path().join(NODE_CONFIG_FILE))
            .expect("failed to read brain config"),
    );
    let add_result = common::send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        action_server_node_dir.path(),
        Duration::from_secs(5),
        Duration::from_secs(10),
        None,
    )
    .await
    .expect("node_add should succeed");

    assert!(
        add_result.success,
        "brain node_add should succeed, got error: {}",
        add_result.error_message.unwrap_or_default()
    );

    let controller_node_dir = tempdir().expect("failed to create temp node directory");
    write_node_config(
        controller_node_dir.path(),
        r#"
        {
            schema_version: 1,
            manifest: {
                name: "controller",
                tag: "0.1.0",
                labels: ["controller"],
                depends_on: {
                    nodes: [
                        { name: "brain", tag: "0.1.0", local_id: "brain" }
                    ]
                },
            },
            interfaces: {
                topics: {
                    emits: [],
                },
                services: {
                    exposes: [],
                },
                actions: {
                    exposes: [],
                    consumes: [
                        {
                          local_node_id: "brain",
                          name: "move_arm",
                        }
                    ],
                },
            },
            execution: {
                language: "rust",
                build_cmd: ["true"],
                run_cmd: ["sleep", "10"],
            },
        }
        "#,
    );

    let controller_response =
        NodeSyncRequest::new(controller_node_dir.path(), common::TEST_GIT_HASH, vec![])
            .poll(
                &started_core_node.caller_handle,
                &started_core_node.core_node_name,
                CALLER_INSTANCE_ID,
                &started_core_node.core_node_name,
                Duration::from_secs(5),
            )
            .await
            .expect("node_sync request should complete");

    assert!(
        controller_response.success,
        "controller node_sync should succeed, got error: {}",
        controller_response.error_message
    );

    let controller_peppygen_dir = controller_node_dir.path().join(PEPPYGEN_OUTPUT_PATH);
    assert!(
        controller_peppygen_dir.exists(),
        "peppygen directory should exist at {}",
        controller_peppygen_dir.display()
    );

    let consumed_action_path = controller_peppygen_dir
        .join("src")
        .join("consumed_actions")
        .join("brain_move_arm.rs");
    assert!(
        consumed_action_path.exists(),
        "peppygen brain_move_arm.rs should exist at {}",
        consumed_action_path.display()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_sync_generates_rust_parameters() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let node_dir = tempdir().expect("failed to create temp node directory");
    write_node_config(
        node_dir.path(),
        r#"
        {
            schema_version: 1,
            manifest: {
                name: "uvc_camera",
                tag: "0.1.0",
                labels: ["camera"],
            },
            execution: {
                language: "rust",
                parameters: {
                  device: {
                    physical: "string",
                    sim: "string",
                    priority: "string"
                  },
                  video: {
                    frame_rate: "u16",
                    resolution: {
                      width: "u16",
                      height: "u16",
                    },
                    encoding: "string",
                  },
                },
                build_cmd: ["true"],
                run_cmd: ["sleep", "10"],
            },
        }
        "#,
    );

    // Generate peppygen for the uvc_camera node first
    let response = NodeSyncRequest::new(node_dir.path(), common::TEST_GIT_HASH, vec![])
        .poll(
            &started_core_node.caller_handle,
            &started_core_node.core_node_name,
            CALLER_INSTANCE_ID,
            &started_core_node.core_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_sync request should complete");

    assert!(
        response.success,
        "uvc_camera node_sync should succeed, got error: {}",
        response.error_message
    );

    // Add uvc_camera to the node stack so the brain node can depend on it
    common::write_peppy_json5(
        node_dir.path(),
        &fs::read_to_string(node_dir.path().join(NODE_CONFIG_FILE))
            .expect("failed to read uvc_camera config"),
    );
    let add_result = common::send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        node_dir.path(),
        Duration::from_secs(5),
        Duration::from_secs(10),
        None,
    )
    .await
    .expect("node_add should succeed");

    assert!(
        add_result.success,
        "uvc_camera node_add should succeed, got error: {}",
        add_result.error_message.unwrap_or_default()
    );

    // Verify that the generated Rust code includes the parameters modules
    let peppygen_dir = node_dir.path().join(PEPPYGEN_OUTPUT_PATH);
    let parameters_rs_path = peppygen_dir.join("src").join("parameters.rs");
    assert!(
        parameters_rs_path.exists(),
        "uvc_camera peppygen parameters.rs should exist at {}",
        parameters_rs_path.display()
    );

    let parameters_rs_content =
        fs::read_to_string(&parameters_rs_path).expect("failed to read parameters.rs");
    assert!(
        parameters_rs_content.contains("mod video"),
        "parameters.rs should contain `mod video`, got:\n{}",
        parameters_rs_content
    );

    assert!(
        parameters_rs_content.contains("Resolution"),
        "parameters.rs should contain `Resolution`, got:\n{}",
        parameters_rs_content
    );

    assert!(
        parameters_rs_content.contains("Video"),
        "parameters.rs should contain `Video`, got:\n{}",
        parameters_rs_content
    );
    assert!(
        parameters_rs_content.contains("frame_rate"),
        "parameters.rs should contain `frame_rate`, got:\n{}",
        parameters_rs_content
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_sync_deletes_previous_peppy_folder() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let node_dir = tempdir().expect("failed to create temp node directory");
    write_node_config(
        node_dir.path(),
        r#"{
            schema_version: 1,
            manifest: {
                name: "example_node",
                tag: "0.1.0",
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            }
        }"#,
    );

    // First generation - creates the .peppy folder
    let response = NodeSyncRequest::new(node_dir.path(), common::TEST_GIT_HASH, vec![])
        .poll(
            &started_core_node.caller_handle,
            &started_core_node.core_node_name,
            CALLER_INSTANCE_ID,
            &started_core_node.core_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_sync request should complete");

    assert!(
        response.success,
        "first node_sync should succeed, got error: {}",
        response.error_message
    );

    let peppy_dir = node_dir.path().join(PEPPY_OUTPUT_DIR);
    assert!(
        peppy_dir.exists(),
        ".peppy directory should exist at {}",
        peppy_dir.display()
    );

    // Add a stale file to simulate leftover content from a previous generation
    let stale_file = peppy_dir.join("stale_file.txt");
    fs::write(&stale_file, "stale content").expect("failed to write stale file");
    assert!(
        stale_file.exists(),
        "stale file should exist at {}",
        stale_file.display()
    );

    // Second generation - should delete the .peppy folder and recreate it
    let response = NodeSyncRequest::new(node_dir.path(), common::TEST_GIT_HASH, vec![])
        .poll(
            &started_core_node.caller_handle,
            &started_core_node.core_node_name,
            CALLER_INSTANCE_ID,
            &started_core_node.core_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_sync request should complete");

    assert!(
        response.success,
        "second node_sync should succeed, got error: {}",
        response.error_message
    );

    // The .peppy folder should still exist (recreated by generation)
    assert!(
        peppy_dir.exists(),
        ".peppy directory should exist after regeneration at {}",
        peppy_dir.display()
    );

    // But the stale file should be gone (folder was deleted before regeneration)
    assert!(
        !stale_file.exists(),
        "stale file should NOT exist after regeneration at {}",
        stale_file.display()
    );

    // Verify the generated content still exists
    let peppygen_dir = node_dir.path().join(PEPPYGEN_OUTPUT_PATH);
    assert!(
        peppygen_dir.exists(),
        "peppygen directory should exist at {}",
        peppygen_dir.display()
    );
}

fn write_variant_config(variant_dir: &Path, peppy_json5: &str) {
    fs::create_dir_all(variant_dir).expect("failed to create variant directory");
    let config_path = variant_dir.join(NODE_CONFIG_FILE);
    fs::write(&config_path, peppy_json5).expect("failed to write variant peppy.json5");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_sync_with_variant_succeeds() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let node_dir = tempdir().expect("failed to create temp node directory");

    // Create a variant subdirectory with a Rust VariantConfig
    let variant_dir = node_dir.path().join("rust_variant");
    write_variant_config(
        &variant_dir,
        r#"{
            schema_version: 1,
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"],
            },
        }"#,
    );

    // Root config declares the variant
    write_node_config(
        node_dir.path(),
        r#"{
            schema_version: 1,
            manifest: {
                name: "example_node",
                tag: "0.1.0",
                variants: [
                    { name: "rust_variant", source: { local: "./rust_variant" } },
                ],
            },
            interfaces: {
                topics: {
                    emits: [
                        {
                            name: "hello_world",
                            qos_profile: "sensor_data",
                            message_format: {
                                message: "string",
                            },
                        },
                    ],
                },
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"],
            },
        }"#,
    );

    let expected_git_hash = "deadbeef";
    let response = NodeSyncRequest::new(node_dir.path(), expected_git_hash, vec![])
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
        response.success,
        "node_sync should succeed, got error: {}",
        response.error_message
    );

    // Verify root .peppy was generated
    let root_peppygen_dir = node_dir.path().join(PEPPYGEN_OUTPUT_PATH);
    assert!(
        root_peppygen_dir.exists(),
        "root peppygen directory should exist at {}",
        root_peppygen_dir.display()
    );

    // Verify variant .peppy was generated
    let variant_peppy_dir = variant_dir.join(PEPPY_OUTPUT_DIR);
    assert!(
        variant_peppy_dir.exists(),
        "variant .peppy directory should exist at {}",
        variant_peppy_dir.display()
    );

    // Verify git.hash in root .peppy
    let node_dir_hash_path = node_dir.path().join(PEPPY_OUTPUT_DIR).join("git.hash");
    assert!(
        node_dir_hash_path.exists(),
        "root git.hash should exist at {}",
        node_dir_hash_path.display()
    );
    let stored_git_hash =
        fs::read_to_string(&node_dir_hash_path).expect("failed to read root git.hash");
    assert_eq!(
        stored_git_hash.trim(),
        expected_git_hash,
        "root git.hash should contain the sync request git_hash"
    );

    // Verify git.hash in variant .peppy
    let variant_git_hash_path = variant_peppy_dir.join("git.hash");
    assert!(
        variant_git_hash_path.exists(),
        "variant git.hash should exist at {}",
        variant_git_hash_path.display()
    );
    let stored_git_hash =
        fs::read_to_string(&variant_git_hash_path).expect("failed to read variant git.hash");
    assert_eq!(
        stored_git_hash.trim(),
        expected_git_hash,
        "variant git.hash should contain the sync request git_hash"
    );

    // Verify root peppygen was generated
    let root_peppygen_dir = node_dir.path().join(PEPPYGEN_OUTPUT_PATH);
    assert!(
        root_peppygen_dir.exists(),
        "root peppygen directory should exist at {}",
        root_peppygen_dir.display()
    );

    // Verify variant peppygen was generated
    let variant_peppygen_dir = variant_dir.join(PEPPYGEN_OUTPUT_PATH);
    assert!(
        variant_peppygen_dir.exists(),
        "variant peppygen directory should exist at {}",
        variant_peppygen_dir.display()
    );

    // Verify the original variant peppy.json5 was NOT overwritten with the merged config
    let variant_config_content = fs::read_to_string(variant_dir.join(NODE_CONFIG_FILE))
        .expect("variant peppy.json5 should still exist");
    assert!(
        !variant_config_content.contains("example_node"),
        "variant peppy.json5 should not contain the root manifest name — \
         it should remain the original VariantConfig"
    );
    assert!(
        variant_config_content.contains("run_cmd"),
        "variant peppy.json5 should still contain the original execution config"
    );

    // Verify the stored fingerprint for the root peppy.json5.
    // Same sandbox principle: the root fingerprint covers only the raw root
    // peppy.json5, not the merged config.
    let root_fingerprint_path = root_peppygen_dir.join("peppy.json5.sha256");
    let stored_root_fingerprint = fs::read_to_string(&root_fingerprint_path)
        .expect("root fingerprint file should exist")
        .trim()
        .to_string();

    let root_own_bytes = fs::read(node_dir.path().join(NODE_CONFIG_FILE))
        .expect("root peppy.json5 should be readable");
    let expected_root_fingerprint = config::fingerprint::fingerprint_for_bytes(&root_own_bytes);
    assert_eq!(
        stored_root_fingerprint, expected_root_fingerprint,
        "stored fingerprint should match the root's own peppy.json5, not the merged config"
    );

    // Verify the stored fingerprint matches the variant's own peppy.json5.
    // Each variant lives in its own sandbox for fingerprinting — it is only
    // aware of its own peppy.json5, not the merged config.
    let variant_fingerprint_path = variant_peppygen_dir.join("peppy.json5.sha256");
    let stored_fingerprint = fs::read_to_string(&variant_fingerprint_path)
        .expect("variant fingerprint file should exist")
        .trim()
        .to_string();

    let variant_own_bytes = fs::read(variant_dir.join(NODE_CONFIG_FILE))
        .expect("variant peppy.json5 should be readable");
    let expected_fingerprint = config::fingerprint::fingerprint_for_bytes(&variant_own_bytes);
    assert_eq!(
        stored_fingerprint, expected_fingerprint,
        "stored fingerprint should match the variant's own peppy.json5"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_sync_variant_missing_directory_fails() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let node_dir = tempdir().expect("failed to create temp node directory");

    // Root config declares a variant whose directory does not exist
    write_node_config(
        node_dir.path(),
        r#"{
            schema_version: 1,
            manifest: {
                name: "example_node",
                tag: "0.1.0",
                variants: [
                    { name: "missing_variant", source: { local: "./missing_variant" } },
                ],
            },
            interfaces: {},
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"],
            },
        }"#,
    );

    let response = NodeSyncRequest::new(node_dir.path(), common::TEST_GIT_HASH, vec![])
        .poll(
            &started_core_node.caller_handle,
            &started_core_node.core_node_name,
            CALLER_INSTANCE_ID,
            &started_core_node.core_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_sync request should complete");

    assert!(!response.success, "node_sync should fail");
    assert!(
        response.error_message.contains("missing_variant"),
        "error should mention the variant name, got: {}",
        response.error_message
    );
    assert!(
        response.error_message.contains("does not exist")
            || response.error_message.contains("No such file"),
        "error should mention missing variant directory, got: {}",
        response.error_message
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_sync_variant_invalid_config_fails() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let node_dir = tempdir().expect("failed to create temp node directory");

    // Create variant directory with invalid config
    let variant_dir = node_dir.path().join("bad_variant");
    write_variant_config(&variant_dir, r#"{ invalid: [unclosed"#);

    // Root config declares the variant
    write_node_config(
        node_dir.path(),
        r#"{
            schema_version: 1,
            manifest: {
                name: "example_node",
                tag: "0.1.0",
                variants: [
                    { name: "bad_variant", source: { local: "./bad_variant" } },
                ],
            },
            interfaces: {},
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"],
            },
        }"#,
    );

    let response = NodeSyncRequest::new(node_dir.path(), common::TEST_GIT_HASH, vec![])
        .poll(
            &started_core_node.caller_handle,
            &started_core_node.core_node_name,
            CALLER_INSTANCE_ID,
            &started_core_node.core_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_sync request should complete");

    assert!(!response.success, "node_sync should fail");
    assert!(
        response.error_message.contains("bad_variant"),
        "error should mention the variant name, got: {}",
        response.error_message
    );
    assert!(
        response.error_message.contains("Failed to parse variant"),
        "error should mention parse failure, got: {}",
        response.error_message
    );
}

/// When a root node has a "default" variant and no execution, sync should
/// skip root codegen (no .peppy at root) but still generate the variant's .peppy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_sync_default_variant_skips_root_codegen() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let node_dir = tempdir().expect("failed to create temp node directory");

    // Default variant with its own execution
    let default_variant_dir = node_dir.path().join("variants").join("default");
    write_variant_config(
        &default_variant_dir,
        r#"{
            schema_version: 1,
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"],
            },
        }"#,
    );

    // Root config: default variant, NO execution
    write_node_config(
        node_dir.path(),
        r#"{
            schema_version: 1,
            manifest: {
                name: "default_variant_node",
                tag: "0.1.0",
                variants: [
                    { name: "default", source: { local: "./variants/default" } },
                ],
            },
            interfaces: {
                topics: {
                    emits: [
                        {
                            name: "hello_world",
                            qos_profile: "sensor_data",
                            message_format: {
                                message: "string",
                            },
                        },
                    ],
                },
            },
        }"#,
    );

    let expected_git_hash = "abc12345";
    let response = NodeSyncRequest::new(node_dir.path(), expected_git_hash, vec![])
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
        response.success,
        "node_sync with default variant should succeed, got error: {}",
        response.error_message
    );

    // Root peppygen should NOT be generated (no execution at root level)
    let root_peppygen_dir = node_dir.path().join(PEPPYGEN_OUTPUT_PATH);
    assert!(
        !root_peppygen_dir.exists(),
        "root peppygen directory should NOT exist for a default-variant node"
    );

    // Root .peppy should exist (git.hash is always written alongside the
    // manifest) but should NOT contain peppygen output (no execution at root).
    let root_peppy_dir = node_dir.path().join(".peppy");
    assert!(
        root_peppy_dir.exists(),
        "root .peppy directory should exist (git.hash lives alongside the manifest)"
    );
    assert!(
        root_peppy_dir.join("git.hash").exists(),
        "root .peppy/git.hash should exist after sync"
    );

    // Variant .peppy should be generated
    let variant_peppy_dir = default_variant_dir.join(PEPPY_OUTPUT_DIR);
    assert!(
        variant_peppy_dir.exists(),
        "variant .peppy directory should exist at {}",
        variant_peppy_dir.display()
    );

    // Verify variant peppygen was generated
    let variant_peppygen_dir = default_variant_dir.join(PEPPYGEN_OUTPUT_PATH);
    assert!(
        variant_peppygen_dir.exists(),
        "variant peppygen directory should exist at {}",
        variant_peppygen_dir.display()
    );

    // Verify git.hash in variant .peppy
    let variant_git_hash_path = variant_peppy_dir.join("git.hash");
    let stored_git_hash =
        fs::read_to_string(&variant_git_hash_path).expect("failed to read variant git.hash");
    assert_eq!(
        stored_git_hash.trim(),
        expected_git_hash,
        "variant git.hash should contain the sync request git_hash"
    );

    // Verify the original variant peppy.json5 was NOT overwritten with the merged config
    let variant_config_content = fs::read_to_string(default_variant_dir.join(NODE_CONFIG_FILE))
        .expect("variant peppy.json5 should still exist");
    assert!(
        !variant_config_content.contains("default_variant_node"),
        "variant peppy.json5 should not contain the root manifest name — \
         it should remain the original VariantConfig"
    );
    assert!(
        variant_config_content.contains("run_cmd"),
        "variant peppy.json5 should still contain the original execution config"
    );
}

/// Verifies that stale root `.peppy` output directories are cleaned up when a node's
/// execution moves from the root level into a variant. The first sync creates a root
/// `.peppy` directory (execution defined at root), then the config is rewritten to remove
/// root execution and add a `"default"` variant with execution instead. The second sync
/// should delete the now-stale root `.peppy` directory and create one under the variant.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_sync_default_variant_cleans_stale_root_peppy_dir() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let node_dir = tempdir().expect("failed to create temp node directory");

    // First sync: node WITH execution — creates a root .peppy directory
    write_node_config(
        node_dir.path(),
        r#"{
            schema_version: 1,
            manifest: {
                name: "example_node",
                tag: "0.1.0",
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"],
            },
        }"#,
    );

    let response = NodeSyncRequest::new(node_dir.path(), common::TEST_GIT_HASH, vec![])
        .poll(
            &started_core_node.caller_handle,
            &started_core_node.core_node_name,
            CALLER_INSTANCE_ID,
            &started_core_node.core_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("first node_sync request should complete");

    assert!(
        response.success,
        "first node_sync (with execution) should succeed, got error: {}",
        response.error_message
    );

    let root_peppy_dir = node_dir.path().join(PEPPY_OUTPUT_DIR);
    assert!(
        root_peppy_dir.exists(),
        "root .peppy directory should exist after first sync"
    );

    // Rewrite config: remove execution from root, add a default variant
    let default_variant_dir = node_dir.path().join("variants").join("default");
    write_variant_config(
        &default_variant_dir,
        r#"{
            schema_version: 1,
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"],
            },
        }"#,
    );

    write_node_config(
        node_dir.path(),
        r#"{
            schema_version: 1,
            manifest: {
                name: "example_node",
                tag: "0.1.0",
                variants: [
                    { name: "default", source: { local: "./variants/default" } },
                ],
            },
        }"#,
    );

    // Second sync: language is None at root — should clean up stale root .peppy
    let response = NodeSyncRequest::new(node_dir.path(), common::TEST_GIT_HASH, vec![])
        .poll(
            &started_core_node.caller_handle,
            &started_core_node.core_node_name,
            CALLER_INSTANCE_ID,
            &started_core_node.core_node_name,
            Duration::from_secs(10),
        )
        .await
        .expect("second node_sync request should complete");

    assert!(
        response.success,
        "second node_sync (without execution) should succeed, got error: {}",
        response.error_message
    );

    // Root .peppy should still exist (git.hash is always written alongside
    // the manifest) but stale peppygen output should have been cleaned up.
    assert!(
        root_peppy_dir.exists(),
        "root .peppy directory should exist (git.hash lives alongside the manifest)"
    );
    assert!(
        root_peppy_dir.join("git.hash").exists(),
        "root .peppy/git.hash should exist after re-sync"
    );
    assert!(
        !node_dir.path().join(PEPPYGEN_OUTPUT_PATH).exists(),
        "stale root peppygen output should have been cleaned up after re-sync"
    );

    // Variant .peppy should be generated
    let variant_peppy_dir = default_variant_dir.join(PEPPY_OUTPUT_DIR);
    assert!(
        variant_peppy_dir.exists(),
        "variant .peppy directory should exist at {}",
        variant_peppy_dir.display()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_sync_undeclared_local_node_id_fails() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let node_dir = tempdir().expect("failed to create temp node directory");
    // The node consumes a topic from local_node_id "nonexistent", but this
    // local_node_id is not declared in depends_on.nodes, so sync should fail.
    write_node_config(
        node_dir.path(),
        r#"
        {
            schema_version: 1,
            manifest: {
                name: "my_robot_brain",
                tag: "0.1.0",
                depends_on: {
                    nodes: []
                },
            },
            interfaces: {
                topics: {
                    emits: [],
                    consumes: [
                        {
                            local_node_id: "nonexistent",
                            name: "video_stream",
                        }
                    ],
                },
                services: {
                    exposes: [],
                },
                actions: {
                    exposes: [],
                },
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"],
            },
        }
        "#,
    );

    let peppygen_dir = node_dir.path().join(PEPPYGEN_OUTPUT_PATH);

    let response = NodeSyncRequest::new(node_dir.path(), common::TEST_GIT_HASH, vec![])
        .poll(
            &started_core_node.caller_handle,
            &started_core_node.core_node_name,
            CALLER_INSTANCE_ID,
            &started_core_node.core_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_sync request should complete");

    assert!(!response.success, "node_sync should fail");
    assert!(
        response.error_message.contains("undeclared local_node_id"),
        "error should mention undeclared local_node_id, got: {}",
        response.error_message
    );

    assert!(
        !peppygen_dir.exists(),
        "peppygen directory should not exist at {}",
        peppygen_dir.display()
    );
}
