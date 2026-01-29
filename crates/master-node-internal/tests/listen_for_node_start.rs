mod common;

use common::{
    AbortOnDrop, NodeStartTestTimeouts, create_test_node_with_name, send_node_add_and_wait,
    send_node_start_and_wait, start_master_node_with_health_timeout,
    start_master_node_with_mock_messenger, start_master_node_with_real_messenger,
    write_peppy_json5,
};
use config::consts::logs_dir_start;
use config::node::Name as NodeName;
use config::peppy_config::Name;
use config::runtime::{NodeInstance, RuntimeConfig};
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

    let started_master = start_master_node_with_real_messenger().await;

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
        NodeInstance {
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

    // Verify that the PID is returned on success
    assert!(
        start_response.result.pid.is_some(),
        "node_start should return a PID on success"
    );
    assert!(
        start_response.result.pid.unwrap() > 0,
        "node_start PID should be a positive number"
    );

    // Verify that the instance was added to the node stack
    let instance_id = NodeName::new(TARGET_INSTANCE_ID).expect("valid instance id");
    let found_instance = started_master.node_stack.find_by_instance_id(&instance_id);
    assert!(
        found_instance.is_some(),
        "instance should be registered in the node stack after successful start"
    );
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
                language: "rust",
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
    let _ready_task = AbortOnDrop(
        listen_for_node_ready(
            &ready_handle,
            &started.master_node_name,
            TARGET_INSTANCE_ID,
            TARGET_NODE_NAME,
        )
        .await
        .expect("failed to start ready service"),
    );

    // Allow the ready service to fully establish its listener
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Create a runtime config for the node_start request
    let runtime_config = RuntimeConfig::new(
        "127.0.0.1",
        config::consts::DEFAULT_MESSAGING_PORT,
        NodeInstance {
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

    // Verify that PID is None on failure
    assert!(
        start_response.result.pid.is_none(),
        "node_start should not return a PID on failure"
    );

    // Verify that the instance was NOT added to the node stack since start failed
    let instance_id = NodeName::new(TARGET_INSTANCE_ID).expect("valid instance id");
    let found_instance = started.node_stack.find_by_instance_id(&instance_id);
    assert!(
        found_instance.is_none(),
        "instance should NOT be registered in the node stack after failed start"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_start_not_found() {
    const TARGET_NODE_NAME: &str = "nonexistent_node";
    const TARGET_INSTANCE_ID: &str = "nonexistent_instance";

    let started = start_master_node_with_mock_messenger().await;

    // Note: We intentionally do NOT add any node to the node stack
    // This simulates trying to start a node that doesn't exist

    // Create a runtime config for a node that was never added
    let runtime_config = RuntimeConfig::new(
        "127.0.0.1",
        config::consts::DEFAULT_MESSAGING_PORT,
        NodeInstance {
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

    // Verify that PID is None on failure
    assert!(
        start_response.result.pid.is_none(),
        "node_start should not return a PID on failure"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_start_streams_stdout_and_stderr() {
    const TARGET_NODE_NAME: &str = "stream_output_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const TARGET_INSTANCE_ID: &str = "stream_output_instance";
    const STDOUT_MARKER: &str = "peppy_start_stdout_marker";
    const STDERR_MARKER: &str = "peppy_start_stderr_marker";

    let started = start_master_node_with_mock_messenger().await;

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
                language: "rust",
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
    let _ready_task = AbortOnDrop(
        listen_for_node_ready(
            &node_messenger,
            &started.master_node_name,
            TARGET_INSTANCE_ID,
            TARGET_NODE_NAME,
        )
        .await
        .expect("node ready service should start"),
    );
    let _health_task = AbortOnDrop(
        listen_for_node_health(
            &node_messenger,
            &started.master_node_name,
            TARGET_INSTANCE_ID,
            TARGET_NODE_NAME,
        )
        .await
        .expect("node health service should start"),
    );

    // Allow ready/health services to establish listeners.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let runtime_config = RuntimeConfig::new(
        "127.0.0.1",
        config::consts::DEFAULT_MESSAGING_PORT,
        NodeInstance {
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

    // Verify that the PID is returned on success
    assert!(
        start_response.result.pid.is_some(),
        "node_start should return a PID on success"
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

    let _ = std::fs::remove_dir_all(&add_response.snapshot_path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_start_writes_log_file() {
    const TARGET_NODE_NAME: &str = "log_file_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const TARGET_INSTANCE_ID: &str = "log_file_instance";
    const STDOUT_MARKER: &str = "peppy_logfile_stdout_marker";
    const STDERR_MARKER: &str = "peppy_logfile_stderr_marker";

    let started = start_master_node_with_mock_messenger().await;

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
                language: "rust",
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
    let _ready_task = AbortOnDrop(
        listen_for_node_ready(
            &node_messenger,
            &started.master_node_name,
            TARGET_INSTANCE_ID,
            TARGET_NODE_NAME,
        )
        .await
        .expect("node ready service should start"),
    );
    let _health_task = AbortOnDrop(
        listen_for_node_health(
            &node_messenger,
            &started.master_node_name,
            TARGET_INSTANCE_ID,
            TARGET_NODE_NAME,
        )
        .await
        .expect("node health service should start"),
    );

    // Allow ready/health services to establish listeners.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let runtime_config = RuntimeConfig::new(
        "127.0.0.1",
        config::consts::DEFAULT_MESSAGING_PORT,
        NodeInstance {
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

    // Verify that the PID is returned on success
    assert!(
        start_response.result.pid.is_some(),
        "node_start should return a PID on success"
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

    let log_content = std::fs::read_to_string(log_path).expect("should be able to read log file");

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
    let _ = std::fs::remove_file(log_path);

    let _ = std::fs::remove_dir_all(&add_response.snapshot_path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_start_reports_all_missing_parameters() {
    const TARGET_NODE_NAME: &str = "params_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const TARGET_INSTANCE_ID: &str = "params_instance";

    let started = start_master_node_with_mock_messenger().await;

    // Create a node config with multiple required parameters
    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
                language: "rust",
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
        config::consts::DEFAULT_MESSAGING_PORT,
        NodeInstance {
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

    // Verify that PID is None on failure
    assert!(
        start_response.result.pid.is_none(),
        "node_start should not return a PID on failure"
    );

    let _ = std::fs::remove_dir_all(&add_response.snapshot_path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_start_reports_only_missing_parameters_when_some_provided() {
    const TARGET_NODE_NAME: &str = "partial_params_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const TARGET_INSTANCE_ID: &str = "partial_params_instance";

    let started = start_master_node_with_mock_messenger().await;

    // Create a node config with multiple required parameters
    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
                language: "rust",
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
        config::consts::DEFAULT_MESSAGING_PORT,
        NodeInstance {
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

    // Verify that PID is None on failure
    assert!(
        start_response.result.pid.is_none(),
        "node_start should not return a PID on failure"
    );

    let _ = std::fs::remove_dir_all(&add_response.snapshot_path);
}

/// Tests that a new goal can be processed after a previous action was abandoned
/// (goal accepted but result never polled).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_start_abandoned_action_does_not_block_next_goal() {
    use config::node::QoSProfile;
    use master_node::encoding::NodeStartGoal;
    use peppylib::ActionMessenger;

    const FIRST_NODE_NAME: &str = "abandoned_start_node";
    const FIRST_NODE_TAG: &str = "0.1.0";
    const FIRST_INSTANCE_ID: &str = "abandoned_start_instance";
    const SECOND_NODE_NAME: &str = "second_start_node";
    const SECOND_NODE_TAG: &str = "0.1.0";
    const SECOND_INSTANCE_ID: &str = "second_start_instance";

    let started = start_master_node_with_mock_messenger().await;

    // Create and add first node
    let first_source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let first_peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{FIRST_NODE_NAME}",
                tag: "{FIRST_NODE_TAG}",
                language: "rust",
                start_cmd: ["sleep", "30"]
            }}
        }}"#
    );
    write_peppy_json5(first_source_dir.path(), &first_peppy_json5);

    let first_add_response = send_node_add_and_wait(
        &started.caller_handle,
        &started.master_node_name,
        first_source_dir.path(),
        Duration::from_secs(5),
        Duration::from_secs(5),
        None,
    )
    .await
    .expect("first node_add should succeed");

    assert!(
        first_add_response.success,
        "first node_add should succeed, got error: {:?}",
        first_add_response.error_message
    );

    // Create and add second node
    let second_source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let second_peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{SECOND_NODE_NAME}",
                tag: "{SECOND_NODE_TAG}",
                language: "rust",
                start_cmd: ["sleep", "30"]
            }}
        }}"#
    );
    write_peppy_json5(second_source_dir.path(), &second_peppy_json5);

    let second_add_response = send_node_add_and_wait(
        &started.caller_handle,
        &started.master_node_name,
        second_source_dir.path(),
        Duration::from_secs(5),
        Duration::from_secs(5),
        None,
    )
    .await
    .expect("second node_add should succeed");

    assert!(
        second_add_response.success,
        "second node_add should succeed, got error: {:?}",
        second_add_response.error_message
    );

    // Set up ready/health services for both nodes
    let node_messenger = MessengerHandle::from_shared(Arc::clone(&started.shared_messenger));

    let _first_ready_task = AbortOnDrop(
        listen_for_node_ready(
            &node_messenger,
            &started.master_node_name,
            FIRST_INSTANCE_ID,
            FIRST_NODE_NAME,
        )
        .await
        .expect("first ready service should start"),
    );
    let _first_health_task = AbortOnDrop(
        listen_for_node_health(
            &node_messenger,
            &started.master_node_name,
            FIRST_INSTANCE_ID,
            FIRST_NODE_NAME,
        )
        .await
        .expect("first health service should start"),
    );

    let _second_ready_task = AbortOnDrop(
        listen_for_node_ready(
            &node_messenger,
            &started.master_node_name,
            SECOND_INSTANCE_ID,
            SECOND_NODE_NAME,
        )
        .await
        .expect("second ready service should start"),
    );
    let _second_health_task = AbortOnDrop(
        listen_for_node_health(
            &node_messenger,
            &started.master_node_name,
            SECOND_INSTANCE_ID,
            SECOND_NODE_NAME,
        )
        .await
        .expect("second health service should start"),
    );

    // Allow ready/health services to establish listeners
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Create runtime config for first node
    let first_runtime_config = RuntimeConfig::new(
        "127.0.0.1",
        config::consts::DEFAULT_MESSAGING_PORT,
        NodeInstance {
            instance_id: Name::new(FIRST_INSTANCE_ID).unwrap(),
            arguments: Default::default(),
        },
        FIRST_NODE_NAME,
        &started.master_node_name,
    )
    .expect("first runtime config should be valid");

    let first_runtime_config_json5 = serde_json5::to_string(&first_runtime_config)
        .expect("first runtime config should serialize");

    // Send first goal but DON'T wait for result (simulating abandoned action)
    let first_goal =
        NodeStartGoal::new(&first_runtime_config_json5, FIRST_NODE_NAME, FIRST_NODE_TAG);
    let first_goal_payload = first_goal.encode().expect("failed to encode first goal");

    let first_action_handle = ActionMessenger::send_goal(
        &started.caller_handle,
        &started.master_node_name,
        common::CALLER_INSTANCE_ID,
        &started.master_node_name,
        master_node::names::NODE_START_ACTION,
        Some(&started.master_node_name),
        None,
        first_goal_payload,
        QoSProfile::default(),
        Duration::from_secs(5),
    )
    .await
    .expect("first goal should be sent");

    // Verify first goal was accepted
    let first_goal_response_payload = first_action_handle.goal_response().payload().to_bytes();
    let first_goal_response =
        master_node::encoding::NodeStartGoalResponse::decode(&first_goal_response_payload)
            .expect("failed to decode first goal response");
    assert!(
        first_goal_response.accepted,
        "first goal should be accepted"
    );

    // Wait for the first action to complete (but don't poll for result)
    // The start operation should complete after ready + health checks pass
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Now send second goal - this should succeed even though we never polled
    // for the first action's result
    let second_runtime_config = RuntimeConfig::new(
        "127.0.0.1",
        config::consts::DEFAULT_MESSAGING_PORT,
        NodeInstance {
            instance_id: Name::new(SECOND_INSTANCE_ID).unwrap(),
            arguments: Default::default(),
        },
        SECOND_NODE_NAME,
        &started.master_node_name,
    )
    .expect("second runtime config should be valid");

    let second_runtime_config_json5 = serde_json5::to_string(&second_runtime_config)
        .expect("second runtime config should serialize");

    let second_start_response = send_node_start_and_wait(
        &started.caller_handle,
        &started.master_node_name,
        &second_runtime_config_json5,
        SECOND_NODE_NAME,
        SECOND_NODE_TAG,
        &NodeStartTestTimeouts {
            goal: Duration::from_secs(5),
            result: Duration::from_secs(10),
        },
        None,
    )
    .await
    .expect("second node_start request should complete");

    assert!(
        second_start_response.result.success,
        "second node_start should succeed even after first action was abandoned, got error: {:?}",
        second_start_response.result.error_message
    );

    // Verify both instances are registered in the node stack
    let first_instance_id = NodeName::new(FIRST_INSTANCE_ID).expect("valid instance id");
    let first_found = started.node_stack.find_by_instance_id(&first_instance_id);
    assert!(
        first_found.is_some(),
        "first instance should be registered after abandoned action completed"
    );

    let second_instance_id = NodeName::new(SECOND_INSTANCE_ID).expect("valid instance id");
    let second_found = started.node_stack.find_by_instance_id(&second_instance_id);
    assert!(
        second_found.is_some(),
        "second instance should be registered after successful start"
    );

    let _ = std::fs::remove_dir_all(&first_add_response.snapshot_path);
    let _ = std::fs::remove_dir_all(&second_add_response.snapshot_path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "Implement later"]
async fn listen_for_node_start_remove_node_on_unhealthy_node() {
    todo!(
        "After starting a node and the it is ready + healthy, send a `shutdown` signal if it doesn't responds to subsequent health checks (maybe because the process was killed or something).
        In order to implement this test, we should also have a `stack log` in `~/.peppy/stack_log.log` so that the user can check all the operations that were performed by the node stack, 
        including this one that can silently remove instances from the stack."
    )
}
