mod common;

use common::{
    GOAL_TIMEOUT, RESULT_TIMEOUT, StartedCoreNode, TestPackagesCache, poll_core_node,
    start_core_node_with_mock_messenger,
};
use config::consts::{NODE_CONFIG_FILE, PEPPYGEN_OUTPUT_PATH};
use config::failure_kind::PeppyFailureKind;
use core_node_api::encoding::{NodeSyncRequest, RepoSourceKind};
use daemon_config::consts::PEPPY_OUTPUT_DIR;
use peppylib::messaging::Failure;
use std::fs;
use std::path::Path;
use std::time::Duration;
use tempfile::tempdir;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_sync_success() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let node_dir = tempdir().expect("failed to create temp node directory");
    common::write_plain_peppy_json5(
        node_dir.path(),
        r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "example_node",
                tag: "v1",
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
    poll_core_node(
        &started_core_node,
        &NodeSyncRequest::new(node_dir.path(), expected_git_hash, false),
    )
    .await;

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

    let failure = common::poll_core_node_failure(
        &started_core_node,
        &NodeSyncRequest::new("", common::TEST_GIT_HASH, false),
    )
    .await;
    common::assert_failure_kind(&failure, PeppyFailureKind::Rejected);

    assert!(
        failure.message().contains("Missing `node_root_dir`"),
        "error should mention missing node_root_dir, got: {}",
        failure
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

    let failure = common::poll_core_node_failure(
        &started_core_node,
        &NodeSyncRequest::new(missing_dir, common::TEST_GIT_HASH, false),
    )
    .await;
    common::assert_failure_kind(&failure, PeppyFailureKind::Rejected);

    assert!(
        failure.message().contains("`node_root_dir` does not exist"),
        "error should mention missing directory, got: {}",
        failure
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_sync_node_root_dir_is_not_a_directory_fails() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let tmp = tempdir().expect("failed to create temp directory");
    let file_path = tmp.path().join("not_a_directory");
    fs::write(&file_path, "not a dir").expect("failed to write temp file");

    let failure = common::poll_core_node_failure(
        &started_core_node,
        &NodeSyncRequest::new(file_path, common::TEST_GIT_HASH, false),
    )
    .await;
    common::assert_failure_kind(&failure, PeppyFailureKind::Rejected);

    assert!(
        failure
            .message()
            .contains("`node_root_dir` is not a directory"),
        "error should mention not a directory, got: {}",
        failure
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_sync_missing_peppy_json5_fails() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let node_dir = tempdir().expect("failed to create temp node directory");
    let peppygen_dir = node_dir.path().join(PEPPYGEN_OUTPUT_PATH);
    let cargo_toml_path = node_dir.path().join("Cargo.toml");

    let failure = common::poll_core_node_failure(
        &started_core_node,
        &NodeSyncRequest::new(node_dir.path(), common::TEST_GIT_HASH, false),
    )
    .await;
    common::assert_failure_kind(&failure, PeppyFailureKind::Rejected);

    assert!(
        failure
            .message()
            .contains("Node config file does not exist"),
        "error should mention missing node config, got: {}",
        failure
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
    common::write_plain_peppy_json5(node_dir.path(), r#"{ manifest: [unclosed"#);

    let peppygen_dir = node_dir.path().join(PEPPYGEN_OUTPUT_PATH);

    let failure = common::poll_core_node_failure(
        &started_core_node,
        &NodeSyncRequest::new(node_dir.path(), common::TEST_GIT_HASH, false),
    )
    .await;
    common::assert_failure_kind(&failure, PeppyFailureKind::Rejected);

    assert!(
        failure.message().contains("Failed to parse node config"),
        "error should mention parse failure, got: {}",
        failure
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
    // The node subscribes to `video_stream` from `uvc_camera:v1`, but this node doesn't exist in the node stack
    // so the generation fails since it can't sync the Rust interfaces
    common::write_plain_peppy_json5(
        node_dir.path(),
        r#"
        {
            peppy_schema: "node/v1",
            manifest: {
                name: "my_robot_brain",
                tag: "v1",
                labels: ["brain"],
                depends_on: {
                    nodes: [
                        { name: "uvc_camera", tag: "v1", link_id: "uvc_camera" }
                    ]
                },
            },
            interfaces: {
                topics: {
                    emits: [],
                    consumes: [
                        {
                            link_id: "uvc_camera",
                            name: "video_stream",
                        },
                        {
                            link_id: "uvc_camera",
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

    let failure = common::poll_core_node_failure(
        &started_core_node,
        &NodeSyncRequest::new(node_dir.path(), common::TEST_GIT_HASH, false),
    )
    .await;
    common::assert_failure_kind(&failure, PeppyFailureKind::PreconditionFailed);

    assert!(
        failure.message().contains(
            "`my_robot_brain:v1` depends on `uvc_camera:v1`, but it does not exist in the stack"
        ),
        "error should mention missing dependency, got: {}",
        failure
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
    common::write_plain_peppy_json5(
        node_dir.path(),
        r#"
        {
            peppy_schema: "node/v1",
            manifest: {
                name: "my_robot_brain",
                tag: "v1",
                labels: ["brain"],
                depends_on: {
                    nodes: [
                        { name: "uvc_camera", tag: "v1", link_id: "uvc_camera" },
                        { name: "uvc_camera", tag: "v1", link_id: "uvc_camera_2" },
                        { name: "lidar_sensor", tag: "v1", link_id: "lidar_sensor" },
                        { name: "gps_module", tag: "v2", link_id: "gps_module" },
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

    let failure = common::poll_core_node_failure(
        &started_core_node,
        &NodeSyncRequest::new(node_dir.path(), common::TEST_GIT_HASH, false),
    )
    .await;
    common::assert_failure_kind(&failure, PeppyFailureKind::PreconditionFailed);

    // The error message should contain all three unique missing dependencies
    assert!(
        failure.message().contains("uvc_camera:v1"),
        "error should mention uvc_camera dependency, got: {}",
        failure
    );
    assert!(
        failure.message().contains("lidar_sensor:v1"),
        "error should mention lidar_sensor dependency, got: {}",
        failure
    );
    assert!(
        failure.message().contains("gps_module:v2"),
        "error should mention gps_module dependency, got: {}",
        failure
    );
    assert!(
        failure.message().contains("they do not exist"),
        "error should use plural form for multiple missing dependencies, got: {}",
        failure
    );
    // Verify deduplication: uvc_camera should only appear once despite two subscriptions
    assert_eq!(
        failure.message().matches("uvc_camera:v1").count(),
        1,
        "uvc_camera:v1 should appear exactly once (deduplicated), got: {}",
        failure
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
    common::write_plain_peppy_json5(
        uvc_camera_node_dir.path(),
        r#"
        {
            peppy_schema: "node/v1",
            manifest: {
                name: "uvc_camera",
                tag: "v1",
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

    sync_and_add_node(&started_core_node, uvc_camera_node_dir.path()).await;

    // The second node depends on the first, and it should work since the first node is now in the node stack
    let brain_node_dir = tempdir().expect("failed to create temp node directory");
    common::write_plain_peppy_json5(
        brain_node_dir.path(),
        r#"
        {
            peppy_schema: "node/v1",
            manifest: {
                name: "my_robot_brain",
                tag: "v1",
                labels: ["brain"],
                depends_on: {
                    nodes: [
                        { name: "uvc_camera", tag: "v1", link_id: "uvc_camera" }
                    ]
                },
            },
            interfaces: {
                topics: {
                    emits: [],
                    consumes: [
                        {
                          link_id: "uvc_camera",
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
    poll_core_node(
        &started_core_node,
        &NodeSyncRequest::new(brain_node_dir.path(), common::TEST_GIT_HASH, false),
    )
    .await;

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
        .join("uvc_camera")
        .join("video_stream.rs");
    assert!(
        consumed_topic_path.exists(),
        "peppygen uvc_camera/video_stream.rs should exist at {}",
        consumed_topic_path.display()
    );
}

/// A `uvc_camera:v1` that emits the `video_stream` topic.
const CAMERA_EMITS_VIDEO_STREAM: &str = r#"
        {
            peppy_schema: "node/v1",
            manifest: {
                name: "uvc_camera",
                tag: "v1",
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
        "#;

/// A brain that consumes `video_stream` from `uvc_camera:v1`.
const BRAIN_CONSUMES_VIDEO_STREAM: &str = r#"
        {
            peppy_schema: "node/v1",
            manifest: {
                name: "my_robot_brain",
                tag: "v1",
                labels: ["brain"],
                depends_on: {
                    nodes: [
                        { name: "uvc_camera", tag: "v1", link_id: "uvc_camera" }
                    ]
                },
            },
            interfaces: {
                topics: {
                    emits: [],
                    consumes: [
                        {
                          link_id: "uvc_camera",
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
        "#;

/// A `uvc_camera:v1` that exposes the `enable_camera` service.
const CAMERA_EXPOSES_ENABLE_CAMERA: &str = r#"
        {
            peppy_schema: "node/v1",
            manifest: {
                name: "uvc_camera",
                tag: "v1",
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
        "#;

/// A brain that consumes `enable_camera` from `uvc_camera:v1`.
const BRAIN_CONSUMES_ENABLE_CAMERA: &str = r#"
        {
            peppy_schema: "node/v1",
            manifest: {
                name: "my_robot_brain",
                tag: "v1",
                labels: ["brain"],
                depends_on: {
                    nodes: [
                        { name: "uvc_camera", tag: "v1", link_id: "uvc_camera" }
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
                          link_id: "uvc_camera",
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
        "#;

/// A `brain:v1` that exposes the `move_arm` action.
const BRAIN_EXPOSES_MOVE_ARM: &str = r#"
        {
            peppy_schema: "node/v1",
            manifest: {
                name: "brain",
                tag: "v1",
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
        "#;

/// A controller that consumes `move_arm` from `brain:v1`.
const CONTROLLER_CONSUMES_MOVE_ARM: &str = r#"
        {
            peppy_schema: "node/v1",
            manifest: {
                name: "controller",
                tag: "v1",
                labels: ["controller"],
                depends_on: {
                    nodes: [
                        { name: "brain", tag: "v1", link_id: "brain" }
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
                          link_id: "brain",
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
        "#;

/// A node that consumes a topic, a service or an action of a node on the
/// stack gets the generated client module for it.
#[rstest::rstest]
#[case::topic(
    CAMERA_EMITS_VIDEO_STREAM,
    BRAIN_CONSUMES_VIDEO_STREAM,
    "src/consumed_topics/uvc_camera/video_stream.rs"
)]
#[case::service(
    CAMERA_EXPOSES_ENABLE_CAMERA,
    BRAIN_CONSUMES_ENABLE_CAMERA,
    "src/consumed_services/uvc_camera/enable_camera.rs"
)]
#[case::action(
    BRAIN_EXPOSES_MOVE_ARM,
    CONTROLLER_CONSUMES_MOVE_ARM,
    "src/consumed_actions/brain/move_arm.rs"
)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_sync_generates_rust_consumed_interfaces(
    #[case] provider_config: &str,
    #[case] consumer_config: &str,
    #[case] generated_file: &str,
) {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let provider_node_dir = tempdir().expect("failed to create temp node directory");
    common::write_plain_peppy_json5(provider_node_dir.path(), provider_config);
    sync_and_add_node(&started_core_node, provider_node_dir.path()).await;

    let consumer_node_dir = tempdir().expect("failed to create temp node directory");
    common::write_plain_peppy_json5(consumer_node_dir.path(), consumer_config);
    poll_core_node(
        &started_core_node,
        &NodeSyncRequest::new(consumer_node_dir.path(), common::TEST_GIT_HASH, false),
    )
    .await;

    let consumer_peppygen_dir = consumer_node_dir.path().join(PEPPYGEN_OUTPUT_PATH);
    assert!(
        consumer_peppygen_dir.exists(),
        "peppygen directory should exist at {}",
        consumer_peppygen_dir.display()
    );

    let generated_path = consumer_peppygen_dir.join(generated_file);
    assert!(
        generated_path.exists(),
        "peppygen {generated_file} should exist at {}",
        generated_path.display()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_sync_generates_rust_parameters() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let node_dir = tempdir().expect("failed to create temp node directory");
    common::write_plain_peppy_json5(
        node_dir.path(),
        r#"
        {
            peppy_schema: "node/v1",
            manifest: {
                name: "uvc_camera",
                tag: "v1",
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

    sync_and_add_node(&started_core_node, node_dir.path()).await;

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
    common::write_plain_peppy_json5(
        node_dir.path(),
        r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "example_node",
                tag: "v1",
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            }
        }"#,
    );

    // First generation - creates the .peppy folder
    poll_core_node(
        &started_core_node,
        &NodeSyncRequest::new(node_dir.path(), common::TEST_GIT_HASH, false),
    )
    .await;

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
    poll_core_node(
        &started_core_node,
        &NodeSyncRequest::new(node_dir.path(), common::TEST_GIT_HASH, false),
    )
    .await;

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
async fn listen_for_node_sync_undeclared_link_id_fails() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let node_dir = tempdir().expect("failed to create temp node directory");
    // The node consumes a topic from link_id "nonexistent", but this
    // link_id is not declared in depends_on.nodes, so sync should fail.
    common::write_plain_peppy_json5(
        node_dir.path(),
        r#"
        {
            peppy_schema: "node/v1",
            manifest: {
                name: "my_robot_brain",
                tag: "v1",
                depends_on: {
                    nodes: []
                },
            },
            interfaces: {
                topics: {
                    emits: [],
                    consumes: [
                        {
                            link_id: "nonexistent",
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

    let failure = common::poll_core_node_failure(
        &started_core_node,
        &NodeSyncRequest::new(node_dir.path(), common::TEST_GIT_HASH, false),
    )
    .await;
    common::assert_failure_kind(&failure, PeppyFailureKind::PreconditionFailed);

    assert!(
        failure.message().contains("undeclared link_id"),
        "error should mention undeclared link_id, got: {}",
        failure
    );

    assert!(
        !peppygen_dir.exists(),
        "peppygen directory should not exist at {}",
        peppygen_dir.display()
    );
}

/// Syncs the node at `node_dir`, then adds it to the stack, so that a node
/// synced after it can depend on it. Panics if either step fails.
async fn sync_and_add_node(started: &StartedCoreNode, node_dir: &Path) {
    poll_core_node(
        started,
        &NodeSyncRequest::new(node_dir, common::TEST_GIT_HASH, false),
    )
    .await;

    let add_result = common::send_node_add_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        node_dir,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add should succeed");
    add_result.completed();
}

// -----------------------------------------------------------------------
// `include_repositories=true`: repository fallback resolution.
// -----------------------------------------------------------------------

/// Camera config emitting a `video_stream` topic. Used as a stable dep
/// across the repository tests below.
fn camera_config() -> &'static str {
    r#"
    {
        peppy_schema: "node/v1",
        manifest: {
            name: "uvc_camera",
            tag: "v1",
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

/// Brain that consumes `video_stream` from `uvc_camera:v1`, used to
/// drive the resolution path in tests where the camera lives in the
/// repository cache rather than the node stack.
fn brain_consumes_camera_config() -> &'static str {
    r#"
    {
        peppy_schema: "node/v1",
        manifest: {
            name: "my_robot_brain",
            tag: "v1",
            depends_on: {
                nodes: [
                    { name: "uvc_camera", tag: "v1", link_id: "uvc_camera" }
                ]
            },
        },
        interfaces: {
            topics: {
                emits: [],
                consumes: [
                    { link_id: "uvc_camera", name: "video_stream" }
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

async fn sync_with_flag(
    started: &StartedCoreNode,
    node_root: &Path,
    include_repositories: bool,
) -> Result<core_node_api::encoding::NodeSyncResponse, Failure> {
    common::poll_core_node_as(
        started,
        common::CALLER_INSTANCE_ID,
        &NodeSyncRequest::new(node_root, common::TEST_GIT_HASH, include_repositories),
        Duration::from_secs(20),
    )
    .await
    .map_err(|error| common::error_reply(&error))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn include_repositories_false_does_not_resolve_fs_dep_from_repository() {
    let started = start_core_node_with_mock_messenger().await;

    // Camera lives only in the repository cache; flag=false means the
    // resolver never looks there, so the dep is missing from the stack.
    let camera_dir = tempdir().expect("camera tempdir");
    common::write_plain_peppy_json5(camera_dir.path(), camera_config());
    TestPackagesCache::new()
        .fs_entry("uvc_camera", "v1", camera_dir.path())
        .write(&started.peppy_dirs);

    let brain_dir = tempdir().expect("brain tempdir");
    common::write_plain_peppy_json5(brain_dir.path(), brain_consumes_camera_config());

    let failure = sync_with_flag(&started, brain_dir.path(), false)
        .await
        .expect_err("sync should fail without -r");
    common::assert_failure_kind(&failure, PeppyFailureKind::PreconditionFailed);
    assert!(
        failure.message().contains("does not exist in the stack"),
        "error should be the missing-from-stack message, got: {}",
        failure
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn include_repositories_true_resolves_fs_dep_from_repository() {
    let started = start_core_node_with_mock_messenger().await;

    let camera_dir = tempdir().expect("camera tempdir");
    common::write_plain_peppy_json5(camera_dir.path(), camera_config());
    TestPackagesCache::new()
        .fs_entry("uvc_camera", "v1", camera_dir.path())
        .write(&started.peppy_dirs);

    let brain_dir = tempdir().expect("brain tempdir");
    common::write_plain_peppy_json5(brain_dir.path(), brain_consumes_camera_config());

    let response = sync_with_flag(&started, brain_dir.path(), true)
        .await
        .expect("sync should succeed with -r");

    assert!(
        response.resolved_from_stack.is_empty(),
        "stack provenance should be empty (camera was repo-resolved): {:?}",
        response.resolved_from_stack
    );
    assert_eq!(response.resolved_from_repositories.len(), 1);
    let entry = &response.resolved_from_repositories[0];
    assert_eq!(entry.name, "uvc_camera");
    assert_eq!(entry.tag, "v1");
    assert_eq!(entry.source_kind, RepoSourceKind::Fs);

    // peppygen for the consumed topic should exist.
    let consumed_topic_path = brain_dir
        .path()
        .join(PEPPYGEN_OUTPUT_PATH)
        .join("src")
        .join("consumed_topics")
        .join("uvc_camera")
        .join("video_stream.rs");
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
    git2::Repository::init(&source_repo_dir).expect("git init");
    // Commit two nodes.
    std::fs::create_dir_all(source_repo_dir.join("nodes/dep_a")).expect("dep_a dir");
    std::fs::write(
        source_repo_dir.join("nodes/dep_a").join(NODE_CONFIG_FILE),
        r#"{
            peppy_schema: "node/v1",
            manifest: { name: "dep_a", tag: "v1" },
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
            peppy_schema: "node/v1",
            manifest: { name: "dep_b", tag: "v1" },
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

    // Commit the nodes so a fresh clone sees them.
    let branch = common::commit_work_tree(&source_repo_dir);

    let repo_url = source_repo_dir.display().to_string();
    TestPackagesCache::new()
        .git_entry("dep_a", "v1", &repo_url, &branch, "nodes/dep_a")
        .git_entry("dep_b", "v1", &repo_url, &branch, "nodes/dep_b")
        .write(&started.peppy_dirs);

    // Brain depends on both deps and consumes one topic from each.
    let brain_dir = tempdir().expect("brain tempdir");
    common::write_plain_peppy_json5(
        brain_dir.path(),
        r#"
        {
            peppy_schema: "node/v1",
            manifest: {
                name: "my_robot_brain",
                tag: "v1",
                depends_on: {
                    nodes: [
                        { name: "dep_a", tag: "v1", link_id: "a" },
                        { name: "dep_b", tag: "v1", link_id: "b" }
                    ]
                },
            },
            interfaces: {
                topics: {
                    emits: [],
                    consumes: [
                        { link_id: "a", name: "topic_a" },
                        { link_id: "b", name: "topic_b" }
                    ],
                },
                services: { exposes: [] },
                actions: { exposes: [] },
            },
            execution: { language: "rust", run_cmd: ["sleep", "10"] },
        }
        "#,
    );

    let response = sync_with_flag(&started, brain_dir.path(), true)
        .await
        .expect("sync should succeed with -r");
    assert_eq!(response.resolved_from_repositories.len(), 2);
    for entry in &response.resolved_from_repositories {
        assert_eq!(entry.source_kind, RepoSourceKind::Git);
    }

    // Exactly one checkout directory: both deps share the same
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
            peppy_schema: "node/v1",
            manifest: { name: "uvc_camera", tag: "v1" },
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
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add should succeed");
    add_result.completed();

    // Repo version has only `topic_y`, missing `topic_x`.
    let repo_camera_dir = tempdir().expect("repo camera tempdir");
    common::write_plain_peppy_json5(
        repo_camera_dir.path(),
        r#"
        {
            peppy_schema: "node/v1",
            manifest: { name: "uvc_camera", tag: "v1" },
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
        .fs_entry("uvc_camera", "v1", repo_camera_dir.path())
        .write(&started.peppy_dirs);

    // Brain consumes `topic_x`; only the stack version exposes it.
    let brain_dir = tempdir().expect("brain tempdir");
    common::write_plain_peppy_json5(
        brain_dir.path(),
        r#"
        {
            peppy_schema: "node/v1",
            manifest: {
                name: "my_robot_brain",
                tag: "v1",
                depends_on: {
                    nodes: [{ name: "uvc_camera", tag: "v1", link_id: "uvc_camera" }]
                },
            },
            interfaces: {
                topics: {
                    emits: [],
                    consumes: [{ link_id: "uvc_camera", name: "topic_x" }],
                },
                services: { exposes: [] },
                actions: { exposes: [] },
            },
            execution: { language: "rust", run_cmd: ["sleep", "10"] },
        }
        "#,
    );

    let response = sync_with_flag(&started, brain_dir.path(), true)
        .await
        .expect("sync should succeed with -r");
    assert!(
        response
            .resolved_from_stack
            .iter()
            .any(|d| d == "uvc_camera:v1"),
        "stack provenance should list uvc_camera:v1, got {:?}",
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
    // All three live only in the repo cache; the BFS in
    // materialize_repo_deps must walk through every level.
    let c_dir = tempdir().expect("c tempdir");
    common::write_plain_peppy_json5(
        c_dir.path(),
        r#"{
            peppy_schema: "node/v1",
            manifest: { name: "dep_c", tag: "v1" },
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
    common::write_plain_peppy_json5(
        b_dir.path(),
        r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "dep_b",
                tag: "v1",
                depends_on: { nodes: [{ name: "dep_c", tag: "v1", link_id: "c" }] },
            },
            interfaces: {
                topics: {
                    emits: [{ name: "topic_b", qos_profile: "sensor_data", message_format: { v: "u32" } }],
                    consumes: [{ link_id: "c", name: "topic_c" }],
                },
            },
            execution: { language: "rust", run_cmd: ["sleep", "10"] },
        }"#,
    );
    let a_dir = tempdir().expect("a tempdir");
    common::write_plain_peppy_json5(
        a_dir.path(),
        r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "dep_a",
                tag: "v1",
                depends_on: { nodes: [{ name: "dep_b", tag: "v1", link_id: "b" }] },
            },
            interfaces: {
                topics: {
                    emits: [{ name: "topic_a", qos_profile: "sensor_data", message_format: { v: "u32" } }],
                    consumes: [{ link_id: "b", name: "topic_b" }],
                },
            },
            execution: { language: "rust", run_cmd: ["sleep", "10"] },
        }"#,
    );
    TestPackagesCache::new()
        .fs_entry("dep_a", "v1", a_dir.path())
        .fs_entry("dep_b", "v1", b_dir.path())
        .fs_entry("dep_c", "v1", c_dir.path())
        .write(&started.peppy_dirs);

    let brain_dir = tempdir().expect("brain tempdir");
    common::write_plain_peppy_json5(
        brain_dir.path(),
        r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "my_robot_brain",
                tag: "v1",
                depends_on: { nodes: [{ name: "dep_a", tag: "v1", link_id: "a" }] },
            },
            interfaces: {
                topics: {
                    emits: [],
                    consumes: [{ link_id: "a", name: "topic_a" }],
                },
                services: { exposes: [] },
                actions: { exposes: [] },
            },
            execution: { language: "rust", run_cmd: ["sleep", "10"] },
        }"#,
    );

    let response = sync_with_flag(&started, brain_dir.path(), true)
        .await
        .expect("sync should succeed with -r");
    let names: Vec<String> = response
        .resolved_from_repositories
        .iter()
        .map(|e| format!("{}:{}", e.name, e.tag))
        .collect();
    for expected in ["dep_a:v1", "dep_b:v1", "dep_c:v1"] {
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

    // Empty nodes.json5; nothing to materialize from.
    TestPackagesCache::new().write(&started.peppy_dirs);

    let brain_dir = tempdir().expect("brain tempdir");
    common::write_plain_peppy_json5(brain_dir.path(), brain_consumes_camera_config());

    let failure = sync_with_flag(&started, brain_dir.path(), true)
        .await
        .expect_err("sync should fail with missing dep");
    common::assert_failure_kind(&failure, PeppyFailureKind::AttemptFailed);
    assert!(
        failure
            .message()
            .contains("not found in node stack or repository cache"),
        "error should mention both layers, got: {}",
        failure
    );
    assert!(
        failure.message().contains("peppy repo refresh"),
        "error should suggest repo refresh, got: {}",
        failure
    );
}

/// A dependency that resolves through the repository still has its
/// exposure checked: consuming a topic it does not emit fails the sync,
/// names the topic and the dependency, and generates nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn include_repositories_true_topic_the_dependency_does_not_emit_fails() {
    let started = start_core_node_with_mock_messenger().await;

    let camera_dir = tempdir().expect("camera tempdir");
    common::write_plain_peppy_json5(camera_dir.path(), camera_config());
    TestPackagesCache::new()
        .fs_entry("uvc_camera", "v1", camera_dir.path())
        .write(&started.peppy_dirs);

    let brain_dir = tempdir().expect("brain tempdir");
    common::write_plain_peppy_json5(
        brain_dir.path(),
        &brain_consumes_camera_config().replace("video_stream", "depth_image"),
    );

    let failure = sync_with_flag(&started, brain_dir.path(), true)
        .await
        .expect_err("sync should fail on an unexposed topic");
    common::assert_failure_kind(&failure, PeppyFailureKind::PreconditionFailed);
    assert_eq!(
        failure.message(),
        "`my_robot_brain:v1` expects topic `depth_image` from `uvc_camera:v1`, \
         but it is not exposed"
    );
    assert!(
        !brain_dir.path().join(PEPPYGEN_OUTPUT_PATH).exists(),
        "a failed sync must not generate peppygen"
    );
}

/// A `manifest.implements` slot resolved from a git-sourced contract cache
/// materializes the repository checkout before it reads the contract
/// manifest. The contract cache records a *repo-relative* path (e.g.
/// `cameras/depth_camera.json5`), which resolves only inside the checkout,
/// not in the daemon's CWD.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_sync_resolves_git_sourced_implements_contract() {
    let started = start_core_node_with_mock_messenger().await;

    // Build a local git repo that hosts the contract manifest at the
    // same repo-relative location the real `interfaces_hub` uses.
    let source_parent = tempdir().expect("source parent tempdir");
    let source_repo_dir = source_parent.path().join("interfaces_hub");
    git2::Repository::init(&source_repo_dir).expect("git init");

    const CONTRACT_BODY: &str = r#"{
        peppy_schema: "contract/v1",
        manifest: { name: "depth_camera", tag: "v1" },
        interfaces: {
            topics: [
                { name: "video_stream", qos_profile: "sensor_data" }
            ]
        }
    }"#;

    std::fs::create_dir_all(source_repo_dir.join("cameras")).expect("cameras dir");
    std::fs::write(
        source_repo_dir.join("cameras/depth_camera.json5"),
        CONTRACT_BODY,
    )
    .expect("write contract file");

    // Commit the contract file so a fresh clone sees it on the resolved ref.
    let branch = common::commit_work_tree(&source_repo_dir);

    let repo_url = source_repo_dir.display().to_string();

    TestPackagesCache::new()
        .contract_git_entry(
            "depth_camera",
            "v1",
            &repo_url,
            &branch,
            "cameras/depth_camera.json5",
            CONTRACT_BODY,
        )
        .write(&started.peppy_dirs);

    // The node under sync implements the git-sourced contract.
    let node_dir = tempdir().expect("node tempdir");
    common::write_plain_peppy_json5(
        node_dir.path(),
        r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "depth_publisher", tag: "v1",
                implements: [
                    { name: "depth_camera", tag: "v1", link_id: "depth" }
                ],
            },
            interfaces: {
                topics: { emits: [{ link_id: "depth", name: "video_stream" }], consumes: [] },
            },
            execution: { language: "rust", run_cmd: ["sleep", "10"] }
        }"#,
    );

    sync_with_flag(&started, node_dir.path(), true)
        .await
        .expect("sync should succeed with -r");

    // The contract's checkout should exist on disk now; this proves
    // `ensure_checkout` ran rather than `std::fs::read` silently relying
    // on a path that happened to exist in the daemon's CWD.
    let checkout_count = std::fs::read_dir(started.peppy_dirs.git_checkouts_dir())
        .expect("git_checkouts_dir should exist")
        .count();
    assert_eq!(
        checkout_count, 1,
        "exactly one git checkout dir should have been materialized"
    );
}
