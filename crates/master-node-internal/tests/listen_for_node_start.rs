mod common;

use common::{
    NodeStartTestTimeouts, create_test_node_with_name, send_node_add_and_wait,
    send_node_start_and_wait, start_master_node, start_master_node_with_health_timeout,
    start_master_node_with_zenoh_messenger, write_peppy_json5,
};
use config::consts::logs_dir_start;
use config::node::Name as NodeName;
use config::peppy_config::{DeploymentInstance, Name};
use config::runtime::RuntimeConfig;
use master_node::encoding::NodeStartFeedback;
use peppylib::messaging::MessengerHandle;
use peppylib::services::health::listen_for_node_health;
use peppylib::services::ready::listen_for_node_ready;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

/// Creates a temp directory with a peppy.json5 file
fn create_node_config_dir(peppy_json5: &str) -> TempDir {
    let temp_dir = TempDir::new().expect("failed to create temp directory");
    write_peppy_json5(temp_dir.path(), peppy_json5);
    temp_dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_start_success() {
    // These must match the values used in create_test_node()
    const TARGET_NODE_NAME: &str = "runnable_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const TARGET_INSTANCE_ID: &str = "runnable_instance";

    let started_master = start_master_node_with_zenoh_messenger().await;

    // Use a pre-built test node to avoid compilation delays during the test
    let node_dir = create_test_node_with_name(TARGET_NODE_NAME, TARGET_NODE_TAG);

    // Add the node to the master node's node stack
    let add_response = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
        &node_dir,
        Duration::from_secs(30),
        // Longer timeout to account for add_cmd execution and copying the test node folder,
        // which may include build artifacts.
        Duration::from_secs(120),
        None,
    )
    .await
    .expect("node_add should succeed");

    assert!(
        add_response.success,
        "node_add should succeed, got error: {:?}",
        add_response.error_message
    );
    // Intentionally keep the started node process running: nodes are expected to linger
    // and the test should still tear down cleanly.
    let _snapshot_node_path = add_response.snapshot_path;

    // Get the actual messaging endpoint from the Zenoh session
    let (messaging_host, messaging_port) = started_master
        .caller_handle
        .messaging_endpoint()
        .await
        .expect("zenoh endpoint should be available");

    // Create a runtime config for the node_start request
    let runtime_config = RuntimeConfig::new(
        messaging_host.as_str(),
        messaging_port,
        DeploymentInstance {
            instance_id: Name::new(TARGET_INSTANCE_ID).unwrap(),
            arguments: Default::default(),
        },
        TARGET_NODE_NAME,
        &started_master.master_node_name,
    )
    .expect("runtime config should be valid");

    let runtime_config_json5 =
        serde_json5::to_string(&runtime_config).expect("runtime config should serialize");

    let start_response = send_node_start_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
        &runtime_config_json5,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &NodeStartTestTimeouts {
            goal: Duration::from_secs(30),
            result: Duration::from_secs(60),
        },
        None,
    )
    .await
    .expect("node_start action should complete");

    // The start should succeed because the health check was responded to
    assert!(
        start_response.result.success,
        "node_start should succeed, got error: {:?}",
        start_response.result.error_message
    );

    // Verify that the instance was added to the node stack
    let instance_id = NodeName::new(TARGET_INSTANCE_ID).expect("valid instance id");
    let found_instance = started_master.node_stack.find_by_instance_id(&instance_id);
    assert!(
        found_instance.is_some(),
        "instance should be registered in the node stack after successful start"
    );

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_start_timeout() {
    const TARGET_NODE_NAME: &str = "runnable_node";
    const TARGET_INSTANCE_ID: &str = "runnable_instance";

    // Use a short health timeout so the test doesn't take too long
    let started = start_master_node_with_health_timeout(Duration::from_secs(2)).await;

    // Create a node config with a start_cmd that won't respond to health checks
    // Using "sleep 10" as a simple command that runs but doesn't respond
    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{}",
                tag: "0.1.0",
                start_cmd: ["sleep", "10"]
            }},
            parameters: {{}}
        }}"#,
        TARGET_NODE_NAME
    );

    // Create temp directory with peppy.json5
    let temp_dir = create_node_config_dir(&peppy_json5);

    // Add the node to the master node's node stack
    let add_response = send_node_add_and_wait(
        &started.caller_handle,
        &started.master_node_name,
        temp_dir.path(),
        Duration::from_secs(5),
        Duration::from_secs(5),
        None,
    )
    .await
    .expect("node_add should succeed");

    assert!(
        add_response.success,
        "node_add should succeed, got error: {:?}",
        add_response.error_message
    );

    // Set up a ready listener so node_start can proceed to the health-check phase.
    // We intentionally do NOT set up a health listener to force the health check to time out.
    let ready_handle = MessengerHandle::from_shared(Arc::clone(&started.shared_messenger));
    let ready_task = listen_for_node_ready(
        &ready_handle,
        &started.master_node_name,
        TARGET_INSTANCE_ID,
        TARGET_NODE_NAME,
    )
    .await
    .expect("failed to start ready service");

    // Allow the ready service to fully establish its listener
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Create a runtime config for the node_start request
    let runtime_config = RuntimeConfig::new(
        "127.0.0.1",
        7448,
        DeploymentInstance {
            instance_id: Name::new(TARGET_INSTANCE_ID).unwrap(),
            arguments: Default::default(),
        },
        TARGET_NODE_NAME,
        &started.master_node_name,
    )
    .expect("runtime config should be valid");

    let runtime_config_json5 =
        serde_json5::to_string(&runtime_config).expect("runtime config should serialize");

    // Call node_start - this should timeout because the node won't respond to health checks
    let start_response = send_node_start_and_wait(
        &started.caller_handle,
        &started.master_node_name,
        &runtime_config_json5,
        TARGET_NODE_NAME,
        "0.1.0",
        &NodeStartTestTimeouts {
            goal: Duration::from_secs(5),
            result: Duration::from_secs(5),
        },
        None,
    )
    .await
    .expect("node_start action should complete");

    // The start should fail because the health check timed out
    assert!(
        !start_response.result.success,
        "node_start should fail due to health check timeout"
    );
    assert!(
        start_response
            .result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("health check timed out"))
            .unwrap_or(false),
        "error message should indicate health check failure, got: {:?}",
        start_response.result.error_message
    );

    // Verify that the instance was NOT added to the node stack since start failed
    let instance_id = NodeName::new(TARGET_INSTANCE_ID).expect("valid instance id");
    let found_instance = started.node_stack.find_by_instance_id(&instance_id);
    assert!(
        found_instance.is_none(),
        "instance should NOT be registered in the node stack after failed start"
    );

    // Clean up
    ready_task.abort();

    // Abort the master node task
    started.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_start_not_found() {
    const TARGET_NODE_NAME: &str = "nonexistent_node";
    const TARGET_INSTANCE_ID: &str = "nonexistent_instance";

    let started = start_master_node().await;

    // Note: We intentionally do NOT add any node to the node stack
    // This simulates trying to start a node that doesn't exist

    // Create a runtime config for a node that was never added
    let runtime_config = RuntimeConfig::new(
        "127.0.0.1",
        7448,
        DeploymentInstance {
            instance_id: Name::new(TARGET_INSTANCE_ID).unwrap(),
            arguments: Default::default(),
        },
        TARGET_NODE_NAME,
        &started.master_node_name,
    )
    .expect("runtime config should be valid");

    let runtime_config_json5 =
        serde_json5::to_string(&runtime_config).expect("runtime config should serialize");

    // Call node_start - this should fail because the node doesn't exist in the node stack
    let start_response = send_node_start_and_wait(
        &started.caller_handle,
        &started.master_node_name,
        &runtime_config_json5,
        TARGET_NODE_NAME,
        "0.1.0",
        &NodeStartTestTimeouts {
            goal: Duration::from_secs(5),
            result: Duration::from_secs(5),
        },
        None,
    )
    .await
    .expect("node_start action should complete");

    // The start should fail because the node was not found
    assert!(
        !start_response.result.success,
        "node_start should fail because node not found"
    );
    assert!(
        start_response
            .result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("not found in node stack"))
            .unwrap_or(false),
        "error message should indicate node not found, got: {:?}",
        start_response.result.error_message
    );

    // Abort the master node task
    started.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_start_streams_stdout_and_stderr() {
    const TARGET_NODE_NAME: &str = "stream_output_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const TARGET_INSTANCE_ID: &str = "stream_output_instance";
    const STDOUT_MARKER: &str = "peppy_start_stdout_marker";
    const STDERR_MARKER: &str = "peppy_start_stderr_marker";

    let started = start_master_node().await;

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
                start_cmd: ["sh", "-c", "echo {STDOUT_MARKER}; echo {STDERR_MARKER} 1>&2; sleep 5"]
            }}
        }}"#
    );
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_response = send_node_add_and_wait(
        &started.caller_handle,
        &started.master_node_name,
        source_dir.path(),
        Duration::from_secs(5),
        Duration::from_secs(5),
        None,
    )
    .await
    .expect("node_add should succeed");

    assert!(
        add_response.success,
        "node_add should succeed, got error: {:?}",
        add_response.error_message
    );

    let node_messenger = MessengerHandle::from_shared(Arc::clone(&started.shared_messenger));
    let ready_task = listen_for_node_ready(
        &node_messenger,
        &started.master_node_name,
        TARGET_INSTANCE_ID,
        TARGET_NODE_NAME,
    )
    .await
    .expect("node ready service should start");
    let health_task = listen_for_node_health(
        &node_messenger,
        &started.master_node_name,
        TARGET_INSTANCE_ID,
        TARGET_NODE_NAME,
    )
    .await
    .expect("node health service should start");

    // Allow ready/health services to establish listeners.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let runtime_config = RuntimeConfig::new(
        "127.0.0.1",
        7448,
        DeploymentInstance {
            instance_id: Name::new(TARGET_INSTANCE_ID).unwrap(),
            arguments: Default::default(),
        },
        TARGET_NODE_NAME,
        &started.master_node_name,
    )
    .expect("runtime config should be valid");

    let runtime_config_json5 =
        serde_json5::to_string(&runtime_config).expect("runtime config should serialize");

    let (feedback_tx, mut feedback_rx) =
        tokio::sync::mpsc::unbounded_channel::<NodeStartFeedback>();
    let start_response = send_node_start_and_wait(
        &started.caller_handle,
        &started.master_node_name,
        &runtime_config_json5,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &NodeStartTestTimeouts {
            goal: Duration::from_secs(5),
            result: Duration::from_secs(10),
        },
        Some(feedback_tx),
    )
    .await
    .expect("node_start action should complete");

    assert!(
        start_response.result.success,
        "node_start should succeed, got error: {:?}",
        start_response.result.error_message
    );

    let mut feedback = Vec::new();
    while let Ok(entry) = feedback_rx.try_recv() {
        feedback.push(entry);
    }
    let saw_stdout = feedback
        .iter()
        .any(|entry| entry.is_stdout() && entry.line.trim() == STDOUT_MARKER);
    let saw_stderr = feedback
        .iter()
        .any(|entry| entry.is_stderr() && entry.line.trim() == STDERR_MARKER);

    assert!(saw_stdout, "stdout feedback should include marker");
    assert!(saw_stderr, "stderr feedback should include marker");

    ready_task.abort();
    health_task.abort();
    let _ = std::fs::remove_dir_all(&add_response.snapshot_path);
    started.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_start_writes_log_file() {
    const TARGET_NODE_NAME: &str = "log_file_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const TARGET_INSTANCE_ID: &str = "log_file_instance";
    const STDOUT_MARKER: &str = "peppy_logfile_stdout_marker";
    const STDERR_MARKER: &str = "peppy_logfile_stderr_marker";

    let started = start_master_node().await;

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
                start_cmd: ["sh", "-c", "echo {STDOUT_MARKER}; echo {STDERR_MARKER} 1>&2; sleep 5"]
            }}
        }}"#
    );
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_response = send_node_add_and_wait(
        &started.caller_handle,
        &started.master_node_name,
        source_dir.path(),
        Duration::from_secs(5),
        Duration::from_secs(5),
        None,
    )
    .await
    .expect("node_add should succeed");

    assert!(
        add_response.success,
        "node_add should succeed, got error: {:?}",
        add_response.error_message
    );

    let node_messenger = MessengerHandle::from_shared(Arc::clone(&started.shared_messenger));
    let ready_task = listen_for_node_ready(
        &node_messenger,
        &started.master_node_name,
        TARGET_INSTANCE_ID,
        TARGET_NODE_NAME,
    )
    .await
    .expect("node ready service should start");
    let health_task = listen_for_node_health(
        &node_messenger,
        &started.master_node_name,
        TARGET_INSTANCE_ID,
        TARGET_NODE_NAME,
    )
    .await
    .expect("node health service should start");

    // Allow ready/health services to establish listeners.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let runtime_config = RuntimeConfig::new(
        "127.0.0.1",
        7448,
        DeploymentInstance {
            instance_id: Name::new(TARGET_INSTANCE_ID).unwrap(),
            arguments: Default::default(),
        },
        TARGET_NODE_NAME,
        &started.master_node_name,
    )
    .expect("runtime config should be valid");

    let runtime_config_json5 =
        serde_json5::to_string(&runtime_config).expect("runtime config should serialize");

    let start_response = send_node_start_and_wait(
        &started.caller_handle,
        &started.master_node_name,
        &runtime_config_json5,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &NodeStartTestTimeouts {
            goal: Duration::from_secs(5),
            result: Duration::from_secs(10),
        },
        None,
    )
    .await
    .expect("node_start action should complete");

    assert!(
        start_response.result.success,
        "node_start should succeed, got error: {:?}",
        start_response.result.error_message
    );

    // Verify the goal response contains the correct log_path
    assert!(
        start_response.goal_response.accepted,
        "goal should be accepted"
    );
    let expected_log_path = logs_dir_start().join(format!("{}.log", TARGET_INSTANCE_ID));
    assert_eq!(
        start_response.goal_response.log_path, expected_log_path,
        "goal response log_path should match expected path"
    );

    // Verify the log file exists and contains expected content
    let log_path = &start_response.goal_response.log_path;
    assert!(log_path.exists(), "log file should exist at {:?}", log_path);

    let log_content = std::fs::read_to_string(&log_path).expect("should be able to read log file");

    // Check that stdout marker is present with correct prefix
    assert!(
        log_content.contains(&format!("[stdout] {}", STDOUT_MARKER)),
        "log file should contain stdout marker with [stdout] prefix, got:\n{}",
        log_content
    );

    // Check that stderr marker is present with correct prefix
    assert!(
        log_content.contains(&format!("[stderr] {}", STDERR_MARKER)),
        "log file should contain stderr marker with [stderr] prefix, got:\n{}",
        log_content
    );

    // Clean up log file
    let _ = std::fs::remove_file(&log_path);

    ready_task.abort();
    health_task.abort();
    let _ = std::fs::remove_dir_all(&add_response.snapshot_path);
    started.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_start_reports_all_missing_parameters() {
    const TARGET_NODE_NAME: &str = "params_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const TARGET_INSTANCE_ID: &str = "params_instance";

    let started = start_master_node().await;

    // Create a node config with multiple required parameters
    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
                start_cmd: ["echo", "test"]
            }},
            parameters: {{
                device: {{
                    physical: "string",
                    sim: "string"
                }},
                video: {{
                    frame_rate: "u16",
                    encoding: "string"
                }}
            }}
        }}"#
    );
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_response = send_node_add_and_wait(
        &started.caller_handle,
        &started.master_node_name,
        source_dir.path(),
        Duration::from_secs(5),
        Duration::from_secs(5),
        None,
    )
    .await
    .expect("node_add should succeed");

    assert!(
        add_response.success,
        "node_add should succeed, got error: {:?}",
        add_response.error_message
    );

    // Create a runtime config WITHOUT providing any parameters
    let runtime_config = RuntimeConfig::new(
        "127.0.0.1",
        7448,
        DeploymentInstance {
            instance_id: Name::new(TARGET_INSTANCE_ID).unwrap(),
            arguments: Default::default(), // No parameters provided
        },
        TARGET_NODE_NAME,
        &started.master_node_name,
    )
    .expect("runtime config should be valid");

    let runtime_config_json5 =
        serde_json5::to_string(&runtime_config).expect("runtime config should serialize");

    // Call node_start - this should fail with all missing parameters listed
    let start_response = send_node_start_and_wait(
        &started.caller_handle,
        &started.master_node_name,
        &runtime_config_json5,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &NodeStartTestTimeouts {
            goal: Duration::from_secs(5),
            result: Duration::from_secs(5),
        },
        None,
    )
    .await
    .expect("node_start action should complete");

    // The start should fail due to missing parameters
    assert!(
        !start_response.result.success,
        "node_start should fail due to missing parameters"
    );

    let error_msg = start_response
        .result
        .error_message
        .as_ref()
        .expect("error message should be present");

    // Verify the error message contains "Missing required parameters"
    assert!(
        error_msg.contains("Missing required parameters"),
        "error message should indicate missing parameters, got: {}",
        error_msg
    );

    // Verify ALL missing parameters are listed (not just the first one)
    assert!(
        error_msg.contains("device.physical"),
        "error message should list device.physical, got: {}",
        error_msg
    );
    assert!(
        error_msg.contains("device.sim"),
        "error message should list device.sim, got: {}",
        error_msg
    );
    assert!(
        error_msg.contains("video.frame_rate"),
        "error message should list video.frame_rate, got: {}",
        error_msg
    );
    assert!(
        error_msg.contains("video.encoding"),
        "error message should list video.encoding, got: {}",
        error_msg
    );

    let _ = std::fs::remove_dir_all(&add_response.snapshot_path);
    started.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_start_reports_only_missing_parameters_when_some_provided() {
    const TARGET_NODE_NAME: &str = "partial_params_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const TARGET_INSTANCE_ID: &str = "partial_params_instance";

    let started = start_master_node().await;

    // Create a node config with multiple required parameters
    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
                start_cmd: ["echo", "test"]
            }},
            parameters: {{
                device: {{
                    physical: "string",
                    sim: "string"
                }},
                video: {{
                    frame_rate: "u16",
                    encoding: "string"
                }}
            }}
        }}"#
    );
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_response = send_node_add_and_wait(
        &started.caller_handle,
        &started.master_node_name,
        source_dir.path(),
        Duration::from_secs(5),
        Duration::from_secs(5),
        None,
    )
    .await
    .expect("node_add should succeed");

    assert!(
        add_response.success,
        "node_add should succeed, got error: {:?}",
        add_response.error_message
    );

    // Create a runtime config with SOME parameters provided (device is complete, video is missing)
    use config::AnyType;
    use std::collections::BTreeMap;

    let mut device_args = BTreeMap::new();
    device_args.insert(
        "physical".to_string(),
        AnyType::String("/dev/video0".to_string()),
    );
    device_args.insert(
        "sim".to_string(),
        AnyType::String("mock:camera".to_string()),
    );

    let mut arguments = BTreeMap::new();
    arguments.insert("device".to_string(), AnyType::Object(device_args));
    // video is NOT provided

    let runtime_config = RuntimeConfig::new(
        "127.0.0.1",
        7448,
        DeploymentInstance {
            instance_id: Name::new(TARGET_INSTANCE_ID).unwrap(),
            arguments,
        },
        TARGET_NODE_NAME,
        &started.master_node_name,
    )
    .expect("runtime config should be valid");

    let runtime_config_json5 =
        serde_json5::to_string(&runtime_config).expect("runtime config should serialize");

    // Call node_start - this should fail with only the missing video parameters listed
    let start_response = send_node_start_and_wait(
        &started.caller_handle,
        &started.master_node_name,
        &runtime_config_json5,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &NodeStartTestTimeouts {
            goal: Duration::from_secs(5),
            result: Duration::from_secs(5),
        },
        None,
    )
    .await
    .expect("node_start action should complete");

    // The start should fail due to missing video parameters
    assert!(
        !start_response.result.success,
        "node_start should fail due to missing parameters"
    );

    let error_msg = start_response
        .result
        .error_message
        .as_ref()
        .expect("error message should be present");

    // Verify the error message contains "Missing required parameters"
    assert!(
        error_msg.contains("Missing required parameters"),
        "error message should indicate missing parameters, got: {}",
        error_msg
    );

    // Verify only the video parameters are listed as missing (device is complete)
    assert!(
        !error_msg.contains("device.physical"),
        "device.physical was provided and should NOT be in error, got: {}",
        error_msg
    );
    assert!(
        !error_msg.contains("device.sim"),
        "device.sim was provided and should NOT be in error, got: {}",
        error_msg
    );
    assert!(
        error_msg.contains("video.frame_rate"),
        "error message should list video.frame_rate, got: {}",
        error_msg
    );
    assert!(
        error_msg.contains("video.encoding"),
        "error message should list video.encoding, got: {}",
        error_msg
    );

    let _ = std::fs::remove_dir_all(&add_response.snapshot_path);
    started.task.abort();
}
