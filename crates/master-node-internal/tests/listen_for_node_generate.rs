mod common;

use common::{CALLER_INSTANCE_ID, start_master_node};
use config::consts::{NODE_CONFIG_FILE, NODE_CONFIG_FINGERPRINT_FILE, PEPPYGEN_OUTPUT_PATH};
use master_node::encoding::NodeGenerateRequest;
use std::fs;
use std::path::Path;
use std::time::Duration;
use tempfile::tempdir;

fn write_node_config(node_dir: &Path, peppy_json5: &str) {
    let config_path = node_dir.join(NODE_CONFIG_FILE);
    fs::write(&config_path, peppy_json5).expect("failed to write peppy.json5");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_generate_success() {
    let started_master = start_master_node().await;

    let node_dir = tempdir().expect("failed to create temp node directory");
    write_node_config(
        node_dir.path(),
        r#"{
            schema_version: 1,
            manifest: {
                name: "example_node",
                tag: "0.1.0",
                start_cmd: ["sleep", "10"]
            }
        }"#,
    );

    let response = NodeGenerateRequest::new(node_dir.path())
        .poll(
            &started_master.caller_handle,
            &started_master.master_node_name,
            CALLER_INSTANCE_ID,
            &started_master.master_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_generate request should complete");

    assert!(
        response.success,
        "node_generate should succeed, got error: {}",
        response.error_message
    );

    let peppygen_dir = node_dir.path().join(PEPPYGEN_OUTPUT_PATH);
    assert!(
        peppygen_dir.exists(),
        "peppygen directory should exist at {}",
        peppygen_dir.display()
    );

    let fingerprint_path = peppygen_dir.join(NODE_CONFIG_FINGERPRINT_FILE);
    assert!(
        fingerprint_path.exists(),
        "fingerprint file should exist at {}",
        fingerprint_path.display()
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

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_generate_missing_node_root_dir_fails() {
    let started_master = start_master_node().await;

    let response = NodeGenerateRequest::new("")
        .poll(
            &started_master.caller_handle,
            &started_master.master_node_name,
            CALLER_INSTANCE_ID,
            &started_master.master_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_generate request should complete");

    assert!(!response.success, "node_generate should fail");
    assert!(
        response.error_message.contains("Missing `node_root_dir`"),
        "error should mention missing node_root_dir, got: {}",
        response.error_message
    );

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_generate_node_root_dir_does_not_exist_fails() {
    let started_master = start_master_node().await;

    let tmp = tempdir().expect("failed to create temp directory");
    let missing_dir = tmp.path().join("does_not_exist");
    assert!(
        !missing_dir.exists(),
        "missing_dir should not exist at {}",
        missing_dir.display()
    );

    let response = NodeGenerateRequest::new(missing_dir)
        .poll(
            &started_master.caller_handle,
            &started_master.master_node_name,
            CALLER_INSTANCE_ID,
            &started_master.master_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_generate request should complete");

    assert!(!response.success, "node_generate should fail");
    assert!(
        response
            .error_message
            .contains("`node_root_dir` does not exist"),
        "error should mention missing directory, got: {}",
        response.error_message
    );

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_generate_node_root_dir_is_not_a_directory_fails() {
    let started_master = start_master_node().await;

    let tmp = tempdir().expect("failed to create temp directory");
    let file_path = tmp.path().join("not_a_directory");
    fs::write(&file_path, "not a dir").expect("failed to write temp file");

    let response = NodeGenerateRequest::new(file_path)
        .poll(
            &started_master.caller_handle,
            &started_master.master_node_name,
            CALLER_INSTANCE_ID,
            &started_master.master_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_generate request should complete");

    assert!(!response.success, "node_generate should fail");
    assert!(
        response
            .error_message
            .contains("`node_root_dir` is not a directory"),
        "error should mention not a directory, got: {}",
        response.error_message
    );

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_generate_missing_peppy_json5_fails() {
    let started_master = start_master_node().await;

    let node_dir = tempdir().expect("failed to create temp node directory");
    let peppygen_dir = node_dir.path().join(PEPPYGEN_OUTPUT_PATH);
    let cargo_toml_path = node_dir.path().join("Cargo.toml");

    let response = NodeGenerateRequest::new(node_dir.path())
        .poll(
            &started_master.caller_handle,
            &started_master.master_node_name,
            CALLER_INSTANCE_ID,
            &started_master.master_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_generate request should complete");

    assert!(!response.success, "node_generate should fail");
    assert!(
        response
            .error_message
            .contains("Failed to generate peppygen"),
        "error should mention generation failure, got: {}",
        response.error_message
    );
    assert!(
        response.error_message.contains("Cannot find the node"),
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

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_generate_invalid_peppy_json5_fails() {
    let started_master = start_master_node().await;

    let node_dir = tempdir().expect("failed to create temp node directory");
    write_node_config(node_dir.path(), r#"{ manifest: [unclosed"#);

    let peppygen_dir = node_dir.path().join(PEPPYGEN_OUTPUT_PATH);

    let response = NodeGenerateRequest::new(node_dir.path())
        .poll(
            &started_master.caller_handle,
            &started_master.master_node_name,
            CALLER_INSTANCE_ID,
            &started_master.master_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_generate request should complete");

    assert!(!response.success, "node_generate should fail");
    assert!(
        response
            .error_message
            .contains("Failed to generate peppygen"),
        "error should mention generation failure, got: {}",
        response.error_message
    );
    assert!(
        !peppygen_dir.exists(),
        "peppygen directory should not exist at {}",
        peppygen_dir.display()
    );

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_generate_missing_dependency_fails() {
    let started_master = start_master_node().await;

    let node_dir = tempdir().expect("failed to create temp node directory");
    // The node subscribes to `video_stream` from `uvc_camera:0.1.0`, but this node doesn't exist in the node stack
    // so the generation fails since it can't generate the Rust interfaces
    write_node_config(
        node_dir.path(),
        r#"
        {
            schema_version: 1,
            manifest: {
                name: "my_robot_brain",
                tag: "0.1.0",
                labels: ["brain"],
                add_cmd: ["cargo", "build", "--release"],
                start_cmd: ["cargo", "run", "--release"],
            },
            parameters: {},
            interfaces: {
                exposes: {
                    topics: [],
                    services: [],
                    actions: [],
                },
                subscribes_to: {
                    topics: [
                        {
                            id: "camera_front",
                            node: "uvc_camera",
                            name: "video_stream",
                            tag: "0.1.0",
                        }
                    ],
                },
            },
        }
        "#,
    );

    todo!(
        "Finish. Currently this works so there is an issue with the business logic of `generate` service"
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_generate_generates_rust_interfaces() {
    let started_master = start_master_node().await;

    let uvc_camera_node_dir = tempdir().expect("failed to create temp node directory");
    write_node_config(
        uvc_camera_node_dir.path(),
        r#"
        {
            schema_version: 1,
            manifest: {
                name: "my_robot_brain",
                tag: "0.1.0",
                labels: ["brain"],
                add_cmd: ["cargo", "build", "--release"],
                start_cmd: ["cargo", "run", "--release"],
            },
            parameters: {},
            interfaces: {
                exposes: {
                    topics: [
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
                            image: {
                              $type: "array",
                              $items: "u8",
                              $length: 3
                            },
                        },
                      }
                    ],
                    services: [],
                    actions: [],
                },
                subscribes_to: {
                    topics: [
                        {
                            id: "camera_front",
                            node: "uvc_camera",
                            name: "video_stream",
                            tag: "0.1.0",
                        }
                    ],
                },
            },
        }
        "#,
    );

    // TODO: Add uvc_camera to the node stack

    // The second node depends on the first, but it's fine since the first node is now in the node stack
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
                add_cmd: ["cargo", "build", "--release"],
                start_cmd: ["cargo", "run", "--release"],
            },
            parameters: {},
            interfaces: {
                exposes: {
                    topics: [],
                    services: [],
                    actions: [],
                },
                subscribes_to: {
                    topics: [
                        {
                          id: "camera_front",
                          node: "uvc_camera",
                          name: "video_stream",
                          tag: "0.1.0",
                        }
                    ],
                },
            },
        }
        "#,
    );

    todo!(
        "Finish. Check that the Rust interface in the my_robot_brain node has the generated Rust interface to communicate with uvc_camera"
    )
}
