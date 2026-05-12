mod common;

use common::{
    CALLER_INSTANCE_ID, StartedCoreNode, TestPackagesCache, start_core_node_with_mock_messenger,
};
use config::consts::{NODE_CONFIG_FILE, PEPPY_OUTPUT_DIR, PEPPYGEN_OUTPUT_PATH};
use core_node_api::encoding::{NodeSyncRequest, RepoSourceKind};
use peppylib::core_node::transport::poll_node_sync;
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
            peppy_schema: "node_v1",
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
    let response = poll_node_sync(
        &NodeSyncRequest::new(node_dir.path(), expected_git_hash, false),
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

    let response = poll_node_sync(
        &NodeSyncRequest::new("", common::TEST_GIT_HASH, false),
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

    let response = poll_node_sync(
        &NodeSyncRequest::new(missing_dir, common::TEST_GIT_HASH, false),
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

    let response = poll_node_sync(
        &NodeSyncRequest::new(file_path, common::TEST_GIT_HASH, false),
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

    let response = poll_node_sync(
        &NodeSyncRequest::new(node_dir.path(), common::TEST_GIT_HASH, false),
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

    let response = poll_node_sync(
        &NodeSyncRequest::new(node_dir.path(), common::TEST_GIT_HASH, false),
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
async fn listen_for_node_sync_missing_dependency_fails() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let node_dir = tempdir().expect("failed to create temp node directory");
    // The node subscribes to `video_stream` from `uvc_camera:0.1.0`, but this node doesn't exist in the node stack
    // so the generation fails since it can't sync the Rust interfaces
    write_node_config(
        node_dir.path(),
        r#"
        {
            peppy_schema: "node_v1",
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

    let response = poll_node_sync(
        &NodeSyncRequest::new(node_dir.path(), common::TEST_GIT_HASH, false),
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
            peppy_schema: "node_v1",
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
            execution: {
                language: "rust",
                build_cmd: ["cargo", "build", "--release"],
                run_cmd: ["./target/release/my_robot_brain"],
            },
        }
        "#,
    );

    let peppygen_dir = node_dir.path().join(PEPPYGEN_OUTPUT_PATH);

    let response = poll_node_sync(
        &NodeSyncRequest::new(node_dir.path(), common::TEST_GIT_HASH, false),
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
            peppy_schema: "node_v1",
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
    let uvc_camera_response = poll_node_sync(
        &NodeSyncRequest::new(uvc_camera_node_dir.path(), common::TEST_GIT_HASH, false),
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
            peppy_schema: "node_v1",
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
    let brain_response = poll_node_sync(
        &NodeSyncRequest::new(brain_node_dir.path(), common::TEST_GIT_HASH, false),
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
            peppy_schema: "node_v1",
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

    let uvc_camera_response = poll_node_sync(
        &NodeSyncRequest::new(uvc_camera_node_dir.path(), common::TEST_GIT_HASH, false),
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
            peppy_schema: "node_v1",
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

    let brain_response = poll_node_sync(
        &NodeSyncRequest::new(brain_node_dir.path(), common::TEST_GIT_HASH, false),
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
            peppy_schema: "node_v1",
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

    let uvc_camera_response = poll_node_sync(
        &NodeSyncRequest::new(uvc_camera_node_dir.path(), common::TEST_GIT_HASH, false),
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
            peppy_schema: "node_v1",
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

    let brain_response = poll_node_sync(
        &NodeSyncRequest::new(brain_node_dir.path(), common::TEST_GIT_HASH, false),
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
            peppy_schema: "node_v1",
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

    let action_server_response = poll_node_sync(
        &NodeSyncRequest::new(action_server_node_dir.path(), common::TEST_GIT_HASH, false),
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
            peppy_schema: "node_v1",
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

    let controller_response = poll_node_sync(
        &NodeSyncRequest::new(controller_node_dir.path(), common::TEST_GIT_HASH, false),
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
            peppy_schema: "node_v1",
            manifest: {
                name: "uvc_camera",
                tag: "0.1.0",
                labels: ["camera"],
            },
            execution: {
                language: "rust",
                parameters: {
                  device_path: "string",
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
    let response = poll_node_sync(
        &NodeSyncRequest::new(node_dir.path(), common::TEST_GIT_HASH, false),
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
            peppy_schema: "node_v1",
            manifest: {
                name: "example_node",
                tag: "0.1.0",
            },
            interfaces: {},
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            }
        }"#,
    );

    // First generation - creates the .peppy folder
    let response = poll_node_sync(
        &NodeSyncRequest::new(node_dir.path(), common::TEST_GIT_HASH, false),
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
    let response = poll_node_sync(
        &NodeSyncRequest::new(node_dir.path(), common::TEST_GIT_HASH, false),
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
            peppy_schema: "node_v1",
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

    let response = poll_node_sync(
        &NodeSyncRequest::new(node_dir.path(), common::TEST_GIT_HASH, false),
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

// -----------------------------------------------------------------------
// `include_repositories=true` — repository fallback resolution.
// -----------------------------------------------------------------------

/// Camera config emitting a `video_stream` topic. Used as a stable dep
/// across the repository tests below.
fn camera_config() -> &'static str {
    r#"
    {
        peppy_schema: "node_v1",
        manifest: {
            name: "uvc_camera",
            tag: "0.1.0",
        },
        interfaces: {
            topics: {
                emits: [
                  {
                    name: "video_stream",
                    qos_profile: "sensor_data",
                    message_format: { encoding: "string", width: "u32" },
                  }
                ],
                consumes: [],
            },
            services: { exposes: [] },
            actions: { exposes: [] },
        },
        execution: {
            language: "rust",
            run_cmd: ["sleep", "10"],
        },
    }
    "#
}

/// Brain that consumes `video_stream` from `uvc_camera:0.1.0` — used to
/// drive the resolution path in tests where the camera lives in the
/// repository cache rather than the node stack.
fn brain_consumes_camera_config() -> &'static str {
    r#"
    {
        peppy_schema: "node_v1",
        manifest: {
            name: "my_robot_brain",
            tag: "0.1.0",
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
                    { local_node_id: "uvc_camera", name: "video_stream" }
                ],
            },
            services: { exposes: [] },
            actions: { exposes: [] },
        },
        execution: {
            language: "rust",
            run_cmd: ["sleep", "10"],
        },
    }
    "#
}

/// Initialise an empty git repo that's locally cloneable (file:// URL).
/// Returns `(repo_dir, branch_name)`.
fn init_local_git_repo(repo_dir: &Path) -> String {
    let repo = git2::Repository::init(repo_dir).expect("git init");
    // Make a single empty commit so HEAD points at a valid ref the
    // checkout cache can fetch.
    let sig = git2::Signature::now("Test", "test@example.com").expect("sig");
    let mut index = repo.index().expect("index");
    let tree_id = index.write_tree().expect("write_tree");
    let tree = repo.find_tree(tree_id).expect("find_tree");
    repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
        .expect("commit");
    repo.head()
        .expect("head")
        .shorthand()
        .expect("shorthand")
        .to_owned()
}

async fn sync_with_flag(
    started: &StartedCoreNode,
    node_root: &Path,
    include_repositories: bool,
) -> core_node_api::encoding::NodeSyncResponse {
    poll_node_sync(
        &NodeSyncRequest::new(node_root, common::TEST_GIT_HASH, include_repositories),
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        &started.core_node_name,
        Duration::from_secs(20),
    )
    .await
    .expect("node_sync request should complete")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn include_repositories_false_does_not_resolve_fs_dep_from_repository() {
    let started = start_core_node_with_mock_messenger().await;

    // Camera lives only in the repository cache — flag=false means the
    // resolver never looks there, so the dep is missing from the stack.
    let camera_dir = tempdir().expect("camera tempdir");
    write_node_config(camera_dir.path(), camera_config());
    TestPackagesCache::new()
        .fs_entry("uvc_camera", "0.1.0", camera_dir.path())
        .write(&started.peppy_dirs);

    let brain_dir = tempdir().expect("brain tempdir");
    write_node_config(brain_dir.path(), brain_consumes_camera_config());

    let response = sync_with_flag(&started, brain_dir.path(), false).await;

    assert!(!response.success, "sync should fail without -r");
    assert!(
        response
            .error_message
            .contains("does not exist in the stack"),
        "error should be the missing-from-stack message, got: {}",
        response.error_message
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn include_repositories_true_resolves_fs_dep_from_repository() {
    let started = start_core_node_with_mock_messenger().await;

    let camera_dir = tempdir().expect("camera tempdir");
    write_node_config(camera_dir.path(), camera_config());
    TestPackagesCache::new()
        .fs_entry("uvc_camera", "0.1.0", camera_dir.path())
        .write(&started.peppy_dirs);

    let brain_dir = tempdir().expect("brain tempdir");
    write_node_config(brain_dir.path(), brain_consumes_camera_config());

    let response = sync_with_flag(&started, brain_dir.path(), true).await;

    assert!(
        response.success,
        "sync should succeed with -r, got error: {}",
        response.error_message
    );
    assert!(
        response.resolved_from_stack.is_empty(),
        "stack provenance should be empty (camera was repo-resolved): {:?}",
        response.resolved_from_stack
    );
    assert_eq!(response.resolved_from_repositories.len(), 1);
    let entry = &response.resolved_from_repositories[0];
    assert_eq!(entry.name, "uvc_camera");
    assert_eq!(entry.tag, "0.1.0");
    assert_eq!(entry.source_kind, RepoSourceKind::Fs);

    // peppygen for the consumed topic should exist.
    let consumed_topic_path = brain_dir
        .path()
        .join(PEPPYGEN_OUTPUT_PATH)
        .join("src")
        .join("consumed_topics")
        .join("uvc_camera_video_stream.rs");
    assert!(
        consumed_topic_path.exists(),
        "expected peppygen file at {}",
        consumed_topic_path.display()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn include_repositories_true_caches_git_checkout_across_deps() {
    let started = start_core_node_with_mock_messenger().await;

    // One git source repo with two node directories. Brain depends on
    // both → both are materialized in a single sync, but only one git
    // checkout should land on disk.
    let source_repo_parent = tempdir().expect("source repo parent");
    let source_repo_dir = source_repo_parent.path().join("source-repo");
    std::fs::create_dir_all(&source_repo_dir).expect("create source repo dir");
    let branch = init_local_git_repo(&source_repo_dir);
    // Commit two nodes.
    std::fs::create_dir_all(source_repo_dir.join("nodes/dep_a")).expect("dep_a dir");
    std::fs::write(
        source_repo_dir.join("nodes/dep_a").join(NODE_CONFIG_FILE),
        r#"{
            peppy_schema: "node_v1",
            manifest: { name: "dep_a", tag: "0.1.0" },
            interfaces: {
                topics: {
                    emits: [{ name: "topic_a", qos_profile: "sensor_data", message_format: { v: "u32" } }],
                    consumes: [],
                },
            },
            execution: { language: "rust", run_cmd: ["sleep", "10"] },
        }"#,
    )
    .expect("write dep_a config");
    std::fs::create_dir_all(source_repo_dir.join("nodes/dep_b")).expect("dep_b dir");
    std::fs::write(
        source_repo_dir.join("nodes/dep_b").join(NODE_CONFIG_FILE),
        r#"{
            peppy_schema: "node_v1",
            manifest: { name: "dep_b", tag: "0.1.0" },
            interfaces: {
                topics: {
                    emits: [{ name: "topic_b", qos_profile: "sensor_data", message_format: { v: "u32" } }],
                    consumes: [],
                },
            },
            execution: { language: "rust", run_cmd: ["sleep", "10"] },
        }"#,
    )
    .expect("write dep_b config");

    // Stage the new files into the repo so a fresh clone sees them.
    let repo = git2::Repository::open(&source_repo_dir).expect("reopen repo");
    let mut index = repo.index().expect("index");
    index
        .add_all(["nodes/*"].iter(), git2::IndexAddOption::DEFAULT, None)
        .expect("add_all");
    index.write().expect("write index");
    let tree_id = index.write_tree().expect("write_tree");
    let tree = repo.find_tree(tree_id).expect("find_tree");
    let parent_oid = repo.head().unwrap().target().unwrap();
    let parent = repo.find_commit(parent_oid).expect("find_commit");
    let sig = git2::Signature::now("Test", "test@example.com").expect("sig");
    repo.commit(Some("HEAD"), &sig, &sig, "add nodes", &tree, &[&parent])
        .expect("commit");

    let repo_url = source_repo_dir.display().to_string();
    TestPackagesCache::new()
        .git_entry("dep_a", "0.1.0", &repo_url, &branch, "nodes/dep_a")
        .git_entry("dep_b", "0.1.0", &repo_url, &branch, "nodes/dep_b")
        .write(&started.peppy_dirs);

    // Brain depends on both deps and consumes one topic from each.
    let brain_dir = tempdir().expect("brain tempdir");
    write_node_config(
        brain_dir.path(),
        r#"
        {
            peppy_schema: "node_v1",
            manifest: {
                name: "my_robot_brain",
                tag: "0.1.0",
                depends_on: {
                    nodes: [
                        { name: "dep_a", tag: "0.1.0", local_id: "a" },
                        { name: "dep_b", tag: "0.1.0", local_id: "b" }
                    ]
                },
            },
            interfaces: {
                topics: {
                    emits: [],
                    consumes: [
                        { local_node_id: "a", name: "topic_a" },
                        { local_node_id: "b", name: "topic_b" }
                    ],
                },
                services: { exposes: [] },
                actions: { exposes: [] },
            },
            execution: { language: "rust", run_cmd: ["sleep", "10"] },
        }
        "#,
    );

    let response = sync_with_flag(&started, brain_dir.path(), true).await;
    assert!(
        response.success,
        "sync should succeed, got error: {}",
        response.error_message
    );
    assert_eq!(response.resolved_from_repositories.len(), 2);
    for entry in &response.resolved_from_repositories {
        assert_eq!(entry.source_kind, RepoSourceKind::Git);
    }

    // Exactly one checkout directory — both deps share the same
    // (repo_url, ref) so `ensure_checkout` reuses the clone.
    let checkout_count = std::fs::read_dir(started.peppy_dirs.git_checkouts_dir())
        .expect("git_checkouts_dir should exist")
        .count();
    assert_eq!(
        checkout_count, 1,
        "same git URL+ref must be cloned once across all deps"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn include_repositories_true_stack_takes_priority_over_repository() {
    let started = start_core_node_with_mock_messenger().await;

    // Stack version emits BOTH `topic_x` AND `topic_y`. Repo version emits
    // only `topic_y`. Brain consumes `topic_x`. If the resolver reaches
    // the repo (i.e. doesn't prefer the stack), validation will fail.
    let stack_camera_dir = tempdir().expect("stack camera tempdir");
    common::write_peppy_json5(
        stack_camera_dir.path(),
        r#"
        {
            peppy_schema: "node_v1",
            manifest: { name: "uvc_camera", tag: "0.1.0" },
            interfaces: {
                topics: {
                    emits: [
                        { name: "topic_x", qos_profile: "sensor_data", message_format: { v: "u32" } },
                        { name: "topic_y", qos_profile: "sensor_data", message_format: { v: "u32" } }
                    ],
                    consumes: [],
                },
            },
            execution: { language: "rust", run_cmd: ["sleep", "10"] },
        }
        "#,
    );
    let add_result = common::send_node_add_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        stack_camera_dir.path(),
        Duration::from_secs(5),
        Duration::from_secs(10),
        None,
    )
    .await
    .expect("node_add should succeed");
    assert!(
        add_result.success,
        "stack camera node_add should succeed, got: {}",
        add_result.error_message.unwrap_or_default()
    );

    // Repo version has only `topic_y`, missing `topic_x`.
    let repo_camera_dir = tempdir().expect("repo camera tempdir");
    write_node_config(
        repo_camera_dir.path(),
        r#"
        {
            peppy_schema: "node_v1",
            manifest: { name: "uvc_camera", tag: "0.1.0" },
            interfaces: {
                topics: {
                    emits: [
                        { name: "topic_y", qos_profile: "sensor_data", message_format: { v: "u32" } }
                    ],
                    consumes: [],
                },
            },
            execution: { language: "rust", run_cmd: ["sleep", "10"] },
        }
        "#,
    );
    TestPackagesCache::new()
        .fs_entry("uvc_camera", "0.1.0", repo_camera_dir.path())
        .write(&started.peppy_dirs);

    // Brain consumes `topic_x` — only the stack version exposes it.
    let brain_dir = tempdir().expect("brain tempdir");
    write_node_config(
        brain_dir.path(),
        r#"
        {
            peppy_schema: "node_v1",
            manifest: {
                name: "my_robot_brain",
                tag: "0.1.0",
                depends_on: {
                    nodes: [{ name: "uvc_camera", tag: "0.1.0", local_id: "uvc_camera" }]
                },
            },
            interfaces: {
                topics: {
                    emits: [],
                    consumes: [{ local_node_id: "uvc_camera", name: "topic_x" }],
                },
                services: { exposes: [] },
                actions: { exposes: [] },
            },
            execution: { language: "rust", run_cmd: ["sleep", "10"] },
        }
        "#,
    );

    let response = sync_with_flag(&started, brain_dir.path(), true).await;
    assert!(
        response.success,
        "sync should succeed via stack version, got error: {}",
        response.error_message
    );
    assert!(
        response
            .resolved_from_stack
            .iter()
            .any(|d| d == "uvc_camera:0.1.0"),
        "stack provenance should list uvc_camera:0.1.0, got {:?}",
        response.resolved_from_stack
    );
    assert!(
        !response
            .resolved_from_repositories
            .iter()
            .any(|e| e.name == "uvc_camera"),
        "repo provenance should NOT list uvc_camera (stack wins), got {:?}",
        response.resolved_from_repositories
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn include_repositories_true_walks_transitive_dep() {
    let started = start_core_node_with_mock_messenger().await;

    // C is a leaf, B depends on C, A depends on B. Brain depends on A.
    // All three live only in the repo cache — the BFS in
    // materialize_repo_deps must walk through every level.
    let c_dir = tempdir().expect("c tempdir");
    write_node_config(
        c_dir.path(),
        r#"{
            peppy_schema: "node_v1",
            manifest: { name: "dep_c", tag: "0.1.0" },
            interfaces: {
                topics: {
                    emits: [{ name: "topic_c", qos_profile: "sensor_data", message_format: { v: "u32" } }],
                    consumes: [],
                },
            },
            execution: { language: "rust", run_cmd: ["sleep", "10"] },
        }"#,
    );
    let b_dir = tempdir().expect("b tempdir");
    write_node_config(
        b_dir.path(),
        r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "dep_b",
                tag: "0.1.0",
                depends_on: { nodes: [{ name: "dep_c", tag: "0.1.0", local_id: "c" }] },
            },
            interfaces: {
                topics: {
                    emits: [{ name: "topic_b", qos_profile: "sensor_data", message_format: { v: "u32" } }],
                    consumes: [{ local_node_id: "c", name: "topic_c" }],
                },
            },
            execution: { language: "rust", run_cmd: ["sleep", "10"] },
        }"#,
    );
    let a_dir = tempdir().expect("a tempdir");
    write_node_config(
        a_dir.path(),
        r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "dep_a",
                tag: "0.1.0",
                depends_on: { nodes: [{ name: "dep_b", tag: "0.1.0", local_id: "b" }] },
            },
            interfaces: {
                topics: {
                    emits: [{ name: "topic_a", qos_profile: "sensor_data", message_format: { v: "u32" } }],
                    consumes: [{ local_node_id: "b", name: "topic_b" }],
                },
            },
            execution: { language: "rust", run_cmd: ["sleep", "10"] },
        }"#,
    );
    TestPackagesCache::new()
        .fs_entry("dep_a", "0.1.0", a_dir.path())
        .fs_entry("dep_b", "0.1.0", b_dir.path())
        .fs_entry("dep_c", "0.1.0", c_dir.path())
        .write(&started.peppy_dirs);

    let brain_dir = tempdir().expect("brain tempdir");
    write_node_config(
        brain_dir.path(),
        r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "my_robot_brain",
                tag: "0.1.0",
                depends_on: { nodes: [{ name: "dep_a", tag: "0.1.0", local_id: "a" }] },
            },
            interfaces: {
                topics: {
                    emits: [],
                    consumes: [{ local_node_id: "a", name: "topic_a" }],
                },
                services: { exposes: [] },
                actions: { exposes: [] },
            },
            execution: { language: "rust", run_cmd: ["sleep", "10"] },
        }"#,
    );

    let response = sync_with_flag(&started, brain_dir.path(), true).await;
    assert!(
        response.success,
        "sync should succeed, got error: {}",
        response.error_message
    );
    let names: Vec<String> = response
        .resolved_from_repositories
        .iter()
        .map(|e| format!("{}:{}", e.name, e.tag))
        .collect();
    for expected in ["dep_a:0.1.0", "dep_b:0.1.0", "dep_c:0.1.0"] {
        assert!(
            names.iter().any(|n| n == expected),
            "expected {} in repo provenance, got {:?}",
            expected,
            names
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn include_repositories_true_missing_from_stack_and_repo_fails() {
    let started = start_core_node_with_mock_messenger().await;

    // Empty nodes.json5 — nothing to materialize from.
    TestPackagesCache::new().write(&started.peppy_dirs);

    let brain_dir = tempdir().expect("brain tempdir");
    write_node_config(brain_dir.path(), brain_consumes_camera_config());

    let response = sync_with_flag(&started, brain_dir.path(), true).await;

    assert!(!response.success, "sync should fail with missing dep");
    assert!(
        response
            .error_message
            .contains("not found in node stack or repository cache"),
        "error should mention both layers, got: {}",
        response.error_message
    );
    assert!(
        response.error_message.contains("peppy repo refresh"),
        "error should suggest repo refresh, got: {}",
        response.error_message
    );
}
