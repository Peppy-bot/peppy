mod common;

use common::{
    AbortOnDrop, NodeRunTestTimeouts, create_test_node_with_name, send_node_add_then_build,
    send_node_run_and_wait, send_node_run_and_wait_with_env, start_core_node_with_health_monitor,
    start_core_node_with_health_timeout, start_core_node_with_mock_messenger,
    start_core_node_with_real_messenger, write_peppy_json5,
};
use config::consts::DEFAULT_ALPINE_BASE_IMAGE;
use config::node::Name as NodeName;
use core_node_api::encoding::NodeRunFeedback;
use peppylib::messaging::MessengerHandle;
use peppylib::services::health::listen_for_node_health;
use peppylib::services::ready::listen_for_node_ready;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::Mutex;

/// Container tests share a single Lima VM instance and must run serially
/// to avoid concurrent limactl operations (start/stop) that cause failures.
static CONTAINER_TEST_MUTEX: Mutex<()> = Mutex::const_new(());

/// Creates a temp directory with a peppy.json5 file
fn create_node_config_dir(peppy_json5: &str) -> TempDir {
    let temp_dir = TempDir::new().expect("failed to create temp directory");
    write_peppy_json5(temp_dir.path(), peppy_json5);
    temp_dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_run_success() {
    // These must match the values used in create_test_node()
    const TARGET_NODE_NAME: &str = "runnable_node";
    const TARGET_NODE_TAG: &str = "v1";
    const TARGET_INSTANCE_ID: &str = "runnable_instance";

    let started_core_node = start_core_node_with_real_messenger().await;

    // Use a pre-built test node to avoid compilation delays during the test
    let node_dir = create_test_node_with_name(TARGET_NODE_NAME, TARGET_NODE_TAG);

    // Add the node to the core node's node stack
    let add_response = send_node_add_then_build(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &node_dir,
        Duration::from_secs(30),
        // Longer timeout to account for build_cmd execution and copying the test node folder,
        // which may include build artifacts.
        Duration::from_secs(120),
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
    let _artifact_path = add_response.artifact_path;

    // Get the actual messaging endpoint from the Zenoh session
    let (messaging_host, messaging_port) = started_core_node
        .caller_handle
        .messaging_endpoint()
        .await
        .expect("zenoh endpoint should be available");

    // Create a runtime config for the node_run request
    let runtime_config_json5 = common::build_runtime_config_json5(
        messaging_host.as_str(),
        messaging_port,
        &started_core_node.core_node_name,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        TARGET_INSTANCE_ID,
        Default::default(),
    );

    let start_response = send_node_run_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &runtime_config_json5,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &NodeRunTestTimeouts {
            goal: Duration::from_secs(30),
            result: Duration::from_secs(60),
        },
        None,
    )
    .await
    .expect("node_run action should complete");

    // The start should succeed because the health check was responded to
    assert!(
        start_response.result.success,
        "node_run should succeed, got error: {:?}",
        start_response.result.error_message
    );

    // Verify that the PID is returned on success
    assert!(
        start_response.result.pid.is_some(),
        "node_run should return a PID on success"
    );
    assert!(
        start_response.result.pid.unwrap() > 0,
        "node_run PID should be a positive number"
    );

    // Verify that the instance was added to the node stack
    let instance_id = NodeName::new(TARGET_INSTANCE_ID).expect("valid instance id");
    let found_instance = started_core_node
        .node_stack
        .find_by_instance_id(&instance_id);
    assert!(
        found_instance.is_some(),
        "instance should be registered in the node stack after successful start"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_run_timeout() {
    const TARGET_NODE_NAME: &str = "runnable_node";
    const TARGET_NODE_TAG: &str = "v1";
    const TARGET_INSTANCE_ID: &str = "runnable_instance";

    // Use a short health timeout so the test doesn't take too long
    let started = start_core_node_with_health_timeout(Duration::from_secs(2)).await;

    // Create a node config with a run_cmd that won't respond to health checks
    // Using "sleep 10" as a simple command that runs but doesn't respond
    let peppy_json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "v1",
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME);

    // Create temp directory with peppy.json5
    let temp_dir = create_node_config_dir(&peppy_json5);

    // Add the node to the core node's node stack
    let add_response = send_node_add_then_build(
        &started.caller_handle,
        &started.core_node_name,
        temp_dir.path(),
        Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .await
    .expect("node_add should succeed");

    assert!(
        add_response.success,
        "node_add should succeed, got error: {:?}",
        add_response.error_message
    );

    // Set up a ready listener so node_run can proceed to the health-check phase.
    // We intentionally do NOT set up a health listener to force the health check to time out.
    let ready_handle = MessengerHandle::from_shared(Arc::clone(&started.shared_messenger));
    let _ready_task = AbortOnDrop(
        listen_for_node_ready(
            &ready_handle,
            &started.core_node_name,
            TARGET_INSTANCE_ID,
            common::test_node_target(TARGET_NODE_NAME),
        )
        .await
        .expect("failed to start ready service"),
    );

    // Allow the ready service to fully establish its listener
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Create a runtime config for the node_run request
    let runtime_config_json5 = common::default_runtime_config_json5(
        &started.core_node_name,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        TARGET_INSTANCE_ID,
    );

    // Call node_run - this should timeout because the node won't respond to health checks
    let start_response = send_node_run_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        &runtime_config_json5,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &NodeRunTestTimeouts {
            goal: Duration::from_secs(5),
            result: Duration::from_secs(5),
        },
        None,
    )
    .await
    .expect("node_run action should complete");

    // The start should fail because the health check timed out
    assert!(
        !start_response.result.success,
        "node_run should fail due to health check timeout"
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
        "node_run should not return a PID on failure"
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
async fn listen_for_node_run_not_found() {
    const TARGET_NODE_NAME: &str = "nonexistent_node";
    const TARGET_NODE_TAG: &str = "v1";
    const TARGET_INSTANCE_ID: &str = "nonexistent_instance";

    let started = start_core_node_with_mock_messenger().await;

    // Note: We intentionally do NOT add any node to the node stack
    // This simulates trying to start a node that doesn't exist

    // Create a runtime config for a node that was never added
    let runtime_config_json5 = common::default_runtime_config_json5(
        &started.core_node_name,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        TARGET_INSTANCE_ID,
    );

    // Call node_run - this should fail because the node doesn't exist in the node stack
    let start_response = send_node_run_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        &runtime_config_json5,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &NodeRunTestTimeouts {
            goal: Duration::from_secs(5),
            result: Duration::from_secs(5),
        },
        None,
    )
    .await
    .expect("node_run action should complete");

    // The start should fail because the node was not found
    assert!(
        !start_response.result.success,
        "node_run should fail because node not found"
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
        "node_run should not return a PID on failure"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_run_streams_stdout_and_stderr() {
    const TARGET_NODE_NAME: &str = "stream_output_node";
    const TARGET_NODE_TAG: &str = "v1";
    const TARGET_INSTANCE_ID: &str = "stream_output_instance";
    const STDOUT_MARKER: &str = "peppy_start_stdout_marker";
    const STDERR_MARKER: &str = "peppy_start_stderr_marker";

    let started = start_core_node_with_mock_messenger().await;

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    // The node prints both markers immediately, then sleeps so it stays alive
    // while the gated start waits on the delayed health responder. The sleep just
    // needs to outlast that wait; teardown kills the node well before it elapses.
    let peppy_json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                run_cmd: ["sh", "-c", "echo {STDOUT_MARKER}; echo {STDERR_MARKER} 1>&2; sleep 30"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG)
    .replace("{STDOUT_MARKER}", STDOUT_MARKER)
    .replace("{STDERR_MARKER}", STDERR_MARKER);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_response = send_node_add_then_build(
        &started.caller_handle,
        &started.core_node_name,
        source_dir.path(),
        Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .await
    .expect("node_add should succeed");

    assert!(
        add_response.success,
        "node_add should succeed, got error: {:?}",
        add_response.error_message
    );

    // Bring up only the ready responder. The health responder is deliberately
    // delayed by `send_node_run_with_delayed_health` until both markers have
    // streamed: that keeps the action's feedback stream open until the output is
    // captured, instead of racing the daemon's start-success stream close (which
    // can fire before a process even emits its first line under load).
    let node_messenger = MessengerHandle::from_shared(Arc::clone(&started.shared_messenger));
    let _ready_task = AbortOnDrop(
        listen_for_node_ready(
            &node_messenger,
            &started.core_node_name,
            TARGET_INSTANCE_ID,
            common::test_node_target(TARGET_NODE_NAME),
        )
        .await
        .expect("node ready service should start"),
    );

    // Allow the ready service to establish its listener.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let runtime_config_json5 = common::default_runtime_config_json5(
        &started.core_node_name,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        TARGET_INSTANCE_ID,
    );

    let markers_present = |feedback: &[NodeRunFeedback]| {
        let saw_stdout = feedback
            .iter()
            .any(|entry| entry.is_stdout() && entry.line.trim() == STDOUT_MARKER);
        let saw_stderr = feedback
            .iter()
            .any(|entry| entry.is_stderr() && entry.line.trim() == STDERR_MARKER);
        saw_stdout && saw_stderr
    };

    let (start_response, feedback) = common::send_node_run_with_delayed_health(
        &started.caller_handle,
        &node_messenger,
        &started.core_node_name,
        &runtime_config_json5,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        TARGET_INSTANCE_ID,
        &NodeRunTestTimeouts {
            goal: Duration::from_secs(5),
            result: Duration::from_secs(30),
        },
        markers_present,
    )
    .await
    .expect("node_run action should complete");

    assert!(
        start_response.result.success,
        "node_run should succeed, got error: {:?}",
        start_response.result.error_message
    );

    // Verify that the PID is returned on success
    assert!(
        start_response.result.pid.is_some(),
        "node_run should return a PID on success"
    );

    assert!(
        markers_present(&feedback),
        "feedback should include both the stdout and stderr markers, got: {:?}",
        feedback
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_run_writes_log_file() {
    const TARGET_NODE_NAME: &str = "log_file_node";
    const TARGET_NODE_TAG: &str = "v1";
    const TARGET_INSTANCE_ID: &str = "log_file_instance";
    const STDOUT_MARKER: &str = "peppy_logfile_stdout_marker";
    const STDERR_MARKER: &str = "peppy_logfile_stderr_marker";

    let started = start_core_node_with_mock_messenger().await;

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                run_cmd: ["sh", "-c", "echo {STDOUT_MARKER}; echo {STDERR_MARKER} 1>&2; sleep 5"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG)
    .replace("{STDOUT_MARKER}", STDOUT_MARKER)
    .replace("{STDERR_MARKER}", STDERR_MARKER);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_response = send_node_add_then_build(
        &started.caller_handle,
        &started.core_node_name,
        source_dir.path(),
        Duration::from_secs(5),
        Duration::from_secs(5),
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
            &started.core_node_name,
            TARGET_INSTANCE_ID,
            common::test_node_target(TARGET_NODE_NAME),
        )
        .await
        .expect("node ready service should start"),
    );
    let _health_task = AbortOnDrop(
        listen_for_node_health(
            &node_messenger,
            &started.core_node_name,
            TARGET_INSTANCE_ID,
            common::test_node_target(TARGET_NODE_NAME),
        )
        .await
        .expect("node health service should start"),
    );

    // Allow ready/health services to establish listeners.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let runtime_config_json5 = common::default_runtime_config_json5(
        &started.core_node_name,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        TARGET_INSTANCE_ID,
    );

    let start_response = send_node_run_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        &runtime_config_json5,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &NodeRunTestTimeouts {
            goal: Duration::from_secs(5),
            result: Duration::from_secs(10),
        },
        None,
    )
    .await
    .expect("node_run action should complete");

    assert!(
        start_response.result.success,
        "node_run should succeed, got error: {:?}",
        start_response.result.error_message
    );

    // Verify that the PID is returned on success
    assert!(
        start_response.result.pid.is_some(),
        "node_run should return a PID on success"
    );

    // Verify the goal response contains the correct log_path
    assert!(
        start_response.goal_response.accepted,
        "goal should be accepted"
    );
    let expected_log_path = started
        .peppy_dirs
        .logs_dir_run()
        .join(format!("{}.log", TARGET_INSTANCE_ID));
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
        "log file should contain stdout marker with [stdout] prefix, got:
{}",
        log_content
    );

    // Check that stderr marker is present with correct prefix
    assert!(
        log_content.contains(&format!("[stderr] {}", STDERR_MARKER)),
        "log file should contain stderr marker with [stderr] prefix, got:
{}",
        log_content
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_run_reports_all_missing_parameters() {
    const TARGET_NODE_NAME: &str = "params_node";
    const TARGET_NODE_TAG: &str = "v1";
    const TARGET_INSTANCE_ID: &str = "params_instance";

    let started = start_core_node_with_mock_messenger().await;

    // Create a node config with multiple required parameters
    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                parameters: {
                    device: {
                        physical: "string",
                        sim: "string"
                    },
                    video: {
                        frame_rate: "u16",
                        encoding: "string"
                    }
                },
                run_cmd: ["echo", "test"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_response = send_node_add_then_build(
        &started.caller_handle,
        &started.core_node_name,
        source_dir.path(),
        Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .await
    .expect("node_add should succeed");

    assert!(
        add_response.success,
        "node_add should succeed, got error: {:?}",
        add_response.error_message
    );

    // Create a runtime config WITHOUT providing any parameters
    let runtime_config_json5 = common::default_runtime_config_json5(
        &started.core_node_name,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        TARGET_INSTANCE_ID,
    );

    // Call node_run - this should fail with all missing parameters listed
    let start_response = send_node_run_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        &runtime_config_json5,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &NodeRunTestTimeouts {
            goal: Duration::from_secs(5),
            result: Duration::from_secs(5),
        },
        None,
    )
    .await
    .expect("node_run action should complete");

    // The start should fail due to missing parameters
    assert!(
        !start_response.result.success,
        "node_run should fail due to missing parameters"
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
        "node_run should not return a PID on failure"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_run_reports_only_missing_parameters_when_some_provided() {
    const TARGET_NODE_NAME: &str = "partial_params_node";
    const TARGET_NODE_TAG: &str = "v1";
    const TARGET_INSTANCE_ID: &str = "partial_params_instance";

    let started = start_core_node_with_mock_messenger().await;

    // Create a node config with multiple required parameters
    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                parameters: {
                    device: {
                        physical: "string",
                        sim: "string"
                    },
                    video: {
                        frame_rate: "u16",
                        encoding: "string"
                    }
                },
                run_cmd: ["echo", "test"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_response = send_node_add_then_build(
        &started.caller_handle,
        &started.core_node_name,
        source_dir.path(),
        Duration::from_secs(5),
        Duration::from_secs(5),
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

    let runtime_config_json5 = common::build_runtime_config_json5(
        "127.0.0.1",
        config::consts::DEFAULT_MESSAGING_PORT,
        &started.core_node_name,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        TARGET_INSTANCE_ID,
        arguments,
    );

    // Call node_run - this should fail with only the missing video parameters listed
    let start_response = send_node_run_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        &runtime_config_json5,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &NodeRunTestTimeouts {
            goal: Duration::from_secs(5),
            result: Duration::from_secs(5),
        },
        None,
    )
    .await
    .expect("node_run action should complete");

    // The start should fail due to missing video parameters
    assert!(
        !start_response.result.success,
        "node_run should fail due to missing parameters"
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
        "node_run should not return a PID on failure"
    );
}

/// Tests that a new goal can be processed after a previous action was abandoned
/// (goal accepted but result never polled).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_run_abandoned_action_does_not_block_next_goal() {
    use config::node::QoSProfile;
    use core_node_api::encoding::NodeRunGoal;
    use peppylib::ActionMessenger;

    const FIRST_NODE_NAME: &str = "abandoned_start_node";
    const FIRST_NODE_TAG: &str = "v1";
    const FIRST_INSTANCE_ID: &str = "abandoned_start_instance";
    const SECOND_NODE_NAME: &str = "second_start_node";
    const SECOND_NODE_TAG: &str = "v1";
    const SECOND_INSTANCE_ID: &str = "second_start_instance";

    let started = start_core_node_with_mock_messenger().await;

    // Create and add first node
    let first_source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let first_peppy_json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "{FIRST_NODE_NAME}",
                tag: "{FIRST_NODE_TAG}",
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "30"]
            }
        }"#
    .replace("{FIRST_NODE_NAME}", FIRST_NODE_NAME)
    .replace("{FIRST_NODE_TAG}", FIRST_NODE_TAG);
    write_peppy_json5(first_source_dir.path(), &first_peppy_json5);

    let first_add_response = send_node_add_then_build(
        &started.caller_handle,
        &started.core_node_name,
        first_source_dir.path(),
        Duration::from_secs(5),
        Duration::from_secs(5),
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
    let second_peppy_json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "{SECOND_NODE_NAME}",
                tag: "{SECOND_NODE_TAG}",
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "30"]
            }
        }"#
    .replace("{SECOND_NODE_NAME}", SECOND_NODE_NAME)
    .replace("{SECOND_NODE_TAG}", SECOND_NODE_TAG);
    write_peppy_json5(second_source_dir.path(), &second_peppy_json5);

    let second_add_response = send_node_add_then_build(
        &started.caller_handle,
        &started.core_node_name,
        second_source_dir.path(),
        Duration::from_secs(5),
        Duration::from_secs(5),
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
            &started.core_node_name,
            FIRST_INSTANCE_ID,
            common::test_node_target(FIRST_NODE_NAME),
        )
        .await
        .expect("first ready service should start"),
    );
    let _first_health_task = AbortOnDrop(
        listen_for_node_health(
            &node_messenger,
            &started.core_node_name,
            FIRST_INSTANCE_ID,
            common::test_node_target(FIRST_NODE_NAME),
        )
        .await
        .expect("first health service should start"),
    );

    let _second_ready_task = AbortOnDrop(
        listen_for_node_ready(
            &node_messenger,
            &started.core_node_name,
            SECOND_INSTANCE_ID,
            common::test_node_target(SECOND_NODE_NAME),
        )
        .await
        .expect("second ready service should start"),
    );
    let _second_health_task = AbortOnDrop(
        listen_for_node_health(
            &node_messenger,
            &started.core_node_name,
            SECOND_INSTANCE_ID,
            common::test_node_target(SECOND_NODE_NAME),
        )
        .await
        .expect("second health service should start"),
    );

    // Allow ready/health services to establish listeners
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Create runtime config for first node
    let first_runtime_config_json5 = common::default_runtime_config_json5(
        &started.core_node_name,
        FIRST_NODE_NAME,
        FIRST_NODE_TAG,
        FIRST_INSTANCE_ID,
    );

    // Send first goal but DON'T wait for result (simulating abandoned action)
    let first_goal = NodeRunGoal::new(
        &first_runtime_config_json5,
        FIRST_NODE_NAME,
        FIRST_NODE_TAG,
        60,
    );
    let first_goal_payload = first_goal.encode().expect("failed to encode first goal");

    let first_action_handle = ActionMessenger::send_goal(
        &started.caller_handle,
        &started.core_node_name,
        common::CALLER_INSTANCE_ID,
        common::core_node_target(&started.core_node_name),
        core_node::names::NODE_RUN_ACTION,
        Some(&started.core_node_name),
        None,
        first_goal_payload,
        QoSProfile::default(),
        Duration::from_secs(5),
    )
    .await
    .expect("first goal should be sent");

    // Verify first goal was accepted
    let first_goal_response_payload = first_action_handle.goal_response().payload();
    let first_goal_response =
        core_node_api::encoding::NodeRunGoalResponse::decode(&first_goal_response_payload)
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
    let second_runtime_config_json5 = common::default_runtime_config_json5(
        &started.core_node_name,
        SECOND_NODE_NAME,
        SECOND_NODE_TAG,
        SECOND_INSTANCE_ID,
    );

    let second_start_response = send_node_run_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        &second_runtime_config_json5,
        SECOND_NODE_NAME,
        SECOND_NODE_TAG,
        &NodeRunTestTimeouts {
            goal: Duration::from_secs(5),
            result: Duration::from_secs(10),
        },
        None,
    )
    .await
    .expect("second node_run request should complete");

    assert!(
        second_start_response.result.success,
        "second node_run should succeed even after first action was abandoned, got error: {:?}",
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_run_uses_env_overrides_for_path() {
    const TARGET_NODE_NAME: &str = "env_path_node";
    const TARGET_NODE_TAG: &str = "v1";
    const TARGET_INSTANCE_ID: &str = "env_path_instance";

    let started = start_core_node_with_mock_messenger().await;

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                run_cmd: ["printout", "3"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_response = send_node_add_then_build(
        &started.caller_handle,
        &started.core_node_name,
        source_dir.path(),
        Duration::from_secs(30),
        Duration::from_secs(120),
    )
    .await
    .expect("node_add should succeed");
    assert!(
        add_response.success,
        "node_add should succeed, got error: {:?}",
        add_response.error_message
    );

    let instance_messenger = MessengerHandle::from_shared(Arc::clone(&started.shared_messenger));
    let _ready_task = AbortOnDrop(
        listen_for_node_ready(
            &instance_messenger,
            &started.core_node_name,
            TARGET_INSTANCE_ID,
            common::test_node_target(TARGET_NODE_NAME),
        )
        .await
        .expect("failed to start ready service"),
    );
    let _health_task = AbortOnDrop(
        listen_for_node_health(
            &instance_messenger,
            &started.core_node_name,
            TARGET_INSTANCE_ID,
            common::test_node_target(TARGET_NODE_NAME),
        )
        .await
        .expect("failed to start health service"),
    );

    tokio::time::sleep(Duration::from_millis(50)).await;

    let runtime_config_json5 = common::default_runtime_config_json5(
        &started.core_node_name,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        TARGET_INSTANCE_ID,
    );

    // First attempt without env overrides: printout should not be found.
    let start_response_missing = send_node_run_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        &runtime_config_json5,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &NodeRunTestTimeouts {
            goal: Duration::from_secs(10),
            result: Duration::from_secs(10),
        },
        None,
    )
    .await
    .expect("node_run request should complete");

    assert!(
        !start_response_missing.result.success,
        "node_run should fail when printout is missing from daemon PATH"
    );
    assert!(
        start_response_missing
            .result
            .error_message
            .as_ref()
            .is_some_and(|msg| msg.contains("No such file or directory")),
        "expected a spawn failure, got: {:?}",
        start_response_missing.result.error_message
    );

    // Create a temp bin directory with a `printout` script.
    let bin_dir = tempfile::tempdir().expect("failed to create temp bin dir");
    let printout_path = bin_dir.path().join("printout");
    std::fs::write(
        &printout_path,
        "#!/bin/sh
sleep \"${1:-3}\"
",
    )
    .expect("failed to write printout script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&printout_path)
            .expect("failed to get printout metadata")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&printout_path, perms)
            .expect("failed to set printout permissions");
    }

    let current_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{}", bin_dir.path().display(), current_path);
    let env_vars = vec![("PATH".to_string(), new_path)];

    // Second attempt with env overrides: printout should be found.
    let start_response = send_node_run_and_wait_with_env(
        &started.caller_handle,
        &started.core_node_name,
        &runtime_config_json5,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &NodeRunTestTimeouts {
            goal: Duration::from_secs(10),
            result: Duration::from_secs(10),
        },
        None,
        env_vars,
    )
    .await
    .expect("node_run request should complete");

    assert!(
        start_response.result.success,
        "node_run should succeed when caller PATH is passed, got error: {:?}",
        start_response.result.error_message
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_run_injects_runtime_env_vars() {
    const TARGET_NODE_NAME: &str = "runtime_env_start_node";
    const TARGET_NODE_TAG: &str = "v1";
    const TARGET_INSTANCE_ID: &str = "runtime_env_start_instance";

    let started = start_core_node_with_mock_messenger().await;

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                run_cmd: [
                    "sh",
                    "-c",
                    "test -n \"$PEPPY_APPTAINER_BIN\" && test \"$PEPPY_NODE_NAME\" = \"{TARGET_NODE_NAME}\" && test \"$PEPPY_NODE_TAG\" = \"{TARGET_NODE_TAG}\" && sleep 10"
                ]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_response = send_node_add_then_build(
        &started.caller_handle,
        &started.core_node_name,
        source_dir.path(),
        Duration::from_secs(30),
        Duration::from_secs(120),
    )
    .await
    .expect("node_add should succeed");
    assert!(
        add_response.success,
        "node_add should succeed, got error: {:?}",
        add_response.error_message
    );

    let instance_messenger = MessengerHandle::from_shared(Arc::clone(&started.shared_messenger));
    let _ready_task = AbortOnDrop(
        listen_for_node_ready(
            &instance_messenger,
            &started.core_node_name,
            TARGET_INSTANCE_ID,
            common::test_node_target(TARGET_NODE_NAME),
        )
        .await
        .expect("failed to start ready service"),
    );
    let _health_task = AbortOnDrop(
        listen_for_node_health(
            &instance_messenger,
            &started.core_node_name,
            TARGET_INSTANCE_ID,
            common::test_node_target(TARGET_NODE_NAME),
        )
        .await
        .expect("failed to start health service"),
    );

    tokio::time::sleep(Duration::from_millis(50)).await;

    let runtime_config_json5 = common::default_runtime_config_json5(
        &started.core_node_name,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        TARGET_INSTANCE_ID,
    );

    let start_response = send_node_run_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        &runtime_config_json5,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &NodeRunTestTimeouts {
            goal: Duration::from_secs(10),
            result: Duration::from_secs(10),
        },
        None,
    )
    .await
    .expect("node_run request should complete");

    assert!(
        start_response.result.success,
        "node_run should succeed when runtime env vars are injected, got error: {:?}",
        start_response.result.error_message
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_run_with_container_success() {
    let _guard = CONTAINER_TEST_MUTEX.lock().await;

    const TARGET_NODE_NAME: &str = "container_start_node";
    const TARGET_NODE_TAG: &str = "v1";
    const TARGET_INSTANCE_ID: &str = "container_start_instance";

    let started = start_core_node_with_mock_messenger().await;

    // Create a temp directory to bind-mount into the container with a test file
    let mount_dir = tempfile::tempdir().expect("failed to create temp mount dir");
    std::fs::write(mount_dir.path().join("mount_test.txt"), "mount_content")
        .expect("failed to write mount test file");
    let mount_dir_str = mount_dir.path().to_string_lossy().to_string();

    // Create source directory with container config and apptainer definition
    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
        peppy_schema: "node_v1",
        manifest: {
            name: "TARGET_NODE_NAME",
            tag: "TARGET_NODE_TAG",
        },
        execution: {
            language: "rust",
            container: {
                def_file: "apptainer.def",
                mount_paths: [
                    "MOUNT_DIR:MOUNT_DIR:ro"
                ]
            }
        }
    }"#
    .replace("TARGET_NODE_NAME", TARGET_NODE_NAME)
    .replace("TARGET_NODE_TAG", TARGET_NODE_TAG)
    .replace("MOUNT_DIR", &mount_dir_str);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let apptainer_def = format!(
        r#"
Bootstrap: docker
From: {DEFAULT_ALPINE_BASE_IMAGE}

%labels
    Name {TARGET_NODE_NAME}
    Version {TARGET_NODE_TAG}

%runscript
    echo "Received env var $MY_ENV_VAR"
    if [ -f {mount_dir_str}/mount_test.txt ]; then
        echo "Mount path verified: $(cat {mount_dir_str}/mount_test.txt)"
    fi
    exec sleep 300
"#
    );
    std::fs::write(source_dir.path().join("apptainer.def"), &apptainer_def)
        .expect("failed to write apptainer definition");

    // Add the node first (container add flow).
    let add_response = send_node_add_then_build(
        &started.caller_handle,
        &started.core_node_name,
        source_dir.path(),
        Duration::from_secs(30),
        Duration::from_secs(120),
    )
    .await
    .expect("node_add should succeed");

    assert!(
        add_response.success,
        "node_add should succeed, got error: {:?}",
        add_response.error_message
    );

    // Set up ready/health services for the container instance
    let node_messenger = MessengerHandle::from_shared(Arc::clone(&started.shared_messenger));
    let _ready_task = AbortOnDrop(
        listen_for_node_ready(
            &node_messenger,
            &started.core_node_name,
            TARGET_INSTANCE_ID,
            common::test_node_target(TARGET_NODE_NAME),
        )
        .await
        .expect("node ready service should start"),
    );
    let _health_task = AbortOnDrop(
        listen_for_node_health(
            &node_messenger,
            &started.core_node_name,
            TARGET_INSTANCE_ID,
            common::test_node_target(TARGET_NODE_NAME),
        )
        .await
        .expect("node health service should start"),
    );

    // Allow ready/health services to establish listeners.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Create a runtime config for the node_run request
    let runtime_config_json5 = common::default_runtime_config_json5(
        &started.core_node_name,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        TARGET_INSTANCE_ID,
    );

    let env_vars = vec![("MY_ENV_VAR".to_string(), "hello_from_peppy".to_string())];

    let start_response = send_node_run_and_wait_with_env(
        &started.caller_handle,
        &started.core_node_name,
        &runtime_config_json5,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &NodeRunTestTimeouts {
            goal: Duration::from_secs(30),
            result: Duration::from_secs(60),
        },
        None,
        env_vars,
    )
    .await
    .expect("node_run action should complete");

    // The start should succeed
    assert!(
        start_response.result.success,
        "node_run should succeed, got error: {:?}",
        start_response.result.error_message
    );

    // Verify that the PID is returned on success
    assert!(
        start_response.result.pid.is_some(),
        "node_run should return a PID on success"
    );
    assert!(
        start_response.result.pid.unwrap() > 0,
        "node_run PID should be a positive number"
    );

    // Verify that the instance was added to the node stack
    let instance_id = NodeName::new(TARGET_INSTANCE_ID).expect("valid instance id");
    let found_instance = started.node_stack.find_by_instance_id(&instance_id);
    assert!(
        found_instance.is_some(),
        "instance should be registered in the node stack after successful start"
    );

    // Verify the goal response contains a valid log_path
    assert!(
        start_response.goal_response.accepted,
        "goal should be accepted"
    );
    let expected_log_path = started
        .peppy_dirs
        .logs_dir_run()
        .join(format!("{}.log", TARGET_INSTANCE_ID));
    assert_eq!(
        start_response.goal_response.log_path, expected_log_path,
        "goal response log_path should match expected path"
    );

    // Verify the log file exists and contains expected content
    let log_path = &start_response.goal_response.log_path;
    assert!(log_path.exists(), "log file should exist at {:?}", log_path);

    let log_content = std::fs::read_to_string(log_path).expect("should be able to read log file");
    assert!(
        log_content.contains("Executing apptainer run"),
        "log file should contain the apptainer run command, got:
{}",
        log_content
    );
    assert!(
        log_content.contains("Received env var hello_from_peppy"),
        "log file should contain the env var output from the runscript, got:
{}",
        log_content
    );

    // Verify that mount_paths are logged as bind mounts
    assert!(
        log_content.contains("bind_mounts:"),
        "log file should contain bind_mounts info, got:
{}",
        log_content
    );
    assert!(
        log_content.contains(&mount_dir_str),
        "log file should contain the mount directory path, got:
{}",
        log_content
    );

    // Verify the mount path was accessible inside the container
    assert!(
        log_content.contains("Mount path verified: mount_content"),
        "log file should confirm mount path was accessible in container, got:
{}",
        log_content
    );
}

/// Verifies that `start_container_node` auto-creates a missing bind-mount
/// source directory AND emits a loud warning about it to the per-instance
/// start log. The auto-create restores the dev-branch ergonomics for nodes
/// that declare scratch / output directories (e.g. `/tmp/<node>`); the
/// warning is the contract that prevents a typo'd file bind from silently
/// becoming an empty directory. Both halves of the contract are asserted —
/// if a future refactor drops the warning, this test must fail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_run_with_container_creates_missing_mount_dir_and_warns() {
    let _guard = CONTAINER_TEST_MUTEX.lock().await;

    const TARGET_NODE_NAME: &str = "container_mount_create_node";
    const TARGET_NODE_TAG: &str = "v1";
    const TARGET_INSTANCE_ID: &str = "container_mount_create_instance";

    let started = start_core_node_with_mock_messenger().await;

    // Use a non-existent subdirectory inside a temp dir as the mount source.
    // The framework must auto-create it and emit a warning about doing so.
    let parent_dir = tempfile::tempdir().expect("failed to create temp parent dir");
    let mount_dir = parent_dir.path().join("nonexistent_subdir");
    assert!(!mount_dir.exists(), "mount dir should not exist yet");
    let mount_dir_str = mount_dir.to_string_lossy().to_string();

    // Create source directory with container config and apptainer definition
    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
        peppy_schema: "node_v1",
        manifest: {
            name: "TARGET_NODE_NAME",
            tag: "TARGET_NODE_TAG",
        },
        execution: {
            language: "rust",
            container: {
                def_file: "apptainer.def",
                mount_paths: [
                    "MOUNT_DIR:MOUNT_DIR:rw"
                ]
            }
        }
    }"#
    .replace("TARGET_NODE_NAME", TARGET_NODE_NAME)
    .replace("TARGET_NODE_TAG", TARGET_NODE_TAG)
    .replace("MOUNT_DIR", &mount_dir_str);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let apptainer_def = format!(
        r#"
Bootstrap: docker
From: {DEFAULT_ALPINE_BASE_IMAGE}

%labels
    Name {TARGET_NODE_NAME}
    Version {TARGET_NODE_TAG}

%runscript
    exec sleep 300
"#
    );
    std::fs::write(source_dir.path().join("apptainer.def"), &apptainer_def)
        .expect("failed to write apptainer definition");

    // Add the node first (container add flow).
    let add_response = send_node_add_then_build(
        &started.caller_handle,
        &started.core_node_name,
        source_dir.path(),
        Duration::from_secs(30),
        Duration::from_secs(120),
    )
    .await
    .expect("node_add should succeed");

    assert!(
        add_response.success,
        "node_add should succeed, got error: {:?}",
        add_response.error_message
    );

    // Set up ready/health services for the container instance. The container
    // will actually launch (auto-create succeeds), so these need to be live.
    let node_messenger = MessengerHandle::from_shared(Arc::clone(&started.shared_messenger));
    let _ready_task = AbortOnDrop(
        listen_for_node_ready(
            &node_messenger,
            &started.core_node_name,
            TARGET_INSTANCE_ID,
            common::test_node_target(TARGET_NODE_NAME),
        )
        .await
        .expect("node ready service should start"),
    );
    let _health_task = AbortOnDrop(
        listen_for_node_health(
            &node_messenger,
            &started.core_node_name,
            TARGET_INSTANCE_ID,
            common::test_node_target(TARGET_NODE_NAME),
        )
        .await
        .expect("node health service should start"),
    );

    tokio::time::sleep(Duration::from_millis(50)).await;

    let runtime_config_json5 = common::default_runtime_config_json5(
        &started.core_node_name,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        TARGET_INSTANCE_ID,
    );

    let start_response = send_node_run_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        &runtime_config_json5,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &NodeRunTestTimeouts {
            goal: Duration::from_secs(30),
            result: Duration::from_secs(60),
        },
        None,
    )
    .await
    .expect("node_run action should complete");

    // The start must succeed: the framework auto-creates the missing dir.
    assert!(
        start_response.result.success,
        "node_run should succeed after auto-creating the bind source, got error: {:?}",
        start_response.result.error_message
    );

    // The host-side directory must now exist as a directory.
    assert!(
        mount_dir.is_dir(),
        "mount dir should have been auto-created by the framework"
    );

    // The per-instance start log must contain the warning line so that an
    // unintended mkdir (e.g. a typo'd file bind) is visible to the operator.
    let log_path = &start_response.goal_response.log_path;
    let log_content =
        std::fs::read_to_string(log_path).expect("should be able to read instance start log");
    assert!(
        log_content.contains("auto-created missing bind mount source:"),
        "feedback log should contain the auto-create warning, got:
{}",
        log_content
    );
    assert!(
        log_content.contains(&mount_dir_str),
        "feedback log warning should reference the offending path {:?}, got:
{}",
        mount_dir_str,
        log_content
    );

    // The instance should now be registered in the node stack.
    let instance_id = NodeName::new(TARGET_INSTANCE_ID).expect("valid instance id");
    assert!(
        started
            .node_stack
            .find_by_instance_id(&instance_id)
            .is_some(),
        "instance should be registered in the node stack after successful start"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_run_container_failure_includes_stderr_in_error() {
    let _guard = CONTAINER_TEST_MUTEX.lock().await;

    const TARGET_NODE_NAME: &str = "failing_container_node";
    const TARGET_NODE_TAG: &str = "v1";
    const TARGET_INSTANCE_ID: &str = "failing_container_instance";
    const STDERR_MARKER: &str = "peppy_container_fatal_error_marker";

    let started = start_core_node_with_mock_messenger().await;

    // Create a container node whose runscript writes a diagnostic to stderr
    // then exits immediately. This causes the ready signal to fail because the
    // process dies, and the stderr output should be captured in the error.
    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
        peppy_schema: "node_v1",
        manifest: {
            name: "TARGET_NODE_NAME",
            tag: "TARGET_NODE_TAG",
        },
        execution: {
            language: "rust",
            container: {
                def_file: "apptainer.def",
            }
        }
    }"#
    .replace("TARGET_NODE_NAME", TARGET_NODE_NAME)
    .replace("TARGET_NODE_TAG", TARGET_NODE_TAG);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let apptainer_def = format!(
        r#"
Bootstrap: docker
From: {DEFAULT_ALPINE_BASE_IMAGE}

%labels
    Name {TARGET_NODE_NAME}
    Version {TARGET_NODE_TAG}

%runscript
    echo "{STDERR_MARKER}" >&2
    exit 1
"#
    );
    std::fs::write(source_dir.path().join("apptainer.def"), &apptainer_def)
        .expect("failed to write apptainer definition");

    // Add the node first (builds the .sif image).
    let add_response = send_node_add_then_build(
        &started.caller_handle,
        &started.core_node_name,
        source_dir.path(),
        Duration::from_secs(30),
        Duration::from_secs(120),
    )
    .await
    .expect("node_add should succeed");

    assert!(
        add_response.success,
        "node_add should succeed, got error: {:?}",
        add_response.error_message
    );

    // Do NOT set up ready/health services — the process will exit immediately
    // which means the ready signal will fail (process died).

    let runtime_config_json5 = common::default_runtime_config_json5(
        &started.core_node_name,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        TARGET_INSTANCE_ID,
    );

    let start_response = send_node_run_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        &runtime_config_json5,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &NodeRunTestTimeouts {
            goal: Duration::from_secs(30),
            result: Duration::from_secs(60),
        },
        None,
    )
    .await
    .expect("node_run action should complete");

    // The start should fail because the process exits immediately
    assert!(
        !start_response.result.success,
        "node_run should fail because the container process exits immediately"
    );

    // The error message should include stderr output from the container process
    let error_msg = start_response
        .result
        .error_message
        .as_ref()
        .expect("error_message should be present");
    assert!(
        error_msg.contains(STDERR_MARKER),
        "error should include stderr from the container process, got: {}",
        error_msg
    );

    // Verify the log file contains the streamed output
    let log_path = &start_response.goal_response.log_path;
    assert!(log_path.exists(), "log file should exist at {:?}", log_path);

    let log_content = std::fs::read_to_string(log_path).expect("should be able to read log file");
    assert!(
        log_content.contains("Executing apptainer run"),
        "log file should contain the apptainer run command, got:
{}",
        log_content
    );
    assert!(
        log_content.contains(STDERR_MARKER),
        "log file should contain the stderr marker from the container process, got:
{}",
        log_content
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_run_logs_error_on_spawn_failure() {
    const TARGET_NODE_NAME: &str = "spawn_failure_node";
    const TARGET_NODE_TAG: &str = "v1";
    const TARGET_INSTANCE_ID: &str = "spawn_failure_instance";

    let started = start_core_node_with_mock_messenger().await;

    // Create a process node with a run_cmd that cannot be found.
    // This will cause command.spawn() to fail in start_node().
    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                run_cmd: ["nonexistent_binary_peppy_test_xyz"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_response = send_node_add_then_build(
        &started.caller_handle,
        &started.core_node_name,
        source_dir.path(),
        Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .await
    .expect("node_add should succeed");

    assert!(
        add_response.success,
        "node_add should succeed, got error: {:?}",
        add_response.error_message
    );

    let runtime_config_json5 = common::default_runtime_config_json5(
        &started.core_node_name,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        TARGET_INSTANCE_ID,
    );

    let start_response = send_node_run_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        &runtime_config_json5,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &NodeRunTestTimeouts {
            goal: Duration::from_secs(5),
            result: Duration::from_secs(10),
        },
        None,
    )
    .await
    .expect("node_run action should complete");

    assert!(
        !start_response.result.success,
        "node_run should fail because the binary does not exist"
    );

    let error_msg = start_response
        .result
        .error_message
        .as_ref()
        .expect("error_message should be present");
    assert!(
        error_msg.contains("Failed to start node"),
        "error should mention start failure, got: {}",
        error_msg
    );
    assert!(
        error_msg.contains("nonexistent_binary_peppy_test_xyz"),
        "error should include the command that failed, got: {}",
        error_msg
    );

    // The log file should exist and contain the error — not be empty
    let log_path = &start_response.goal_response.log_path;
    assert!(log_path.exists(), "log file should exist at {:?}", log_path);

    let log_content = std::fs::read_to_string(log_path).expect("should be able to read log file");
    assert!(
        !log_content.is_empty(),
        "log file should not be empty when a start failure occurs"
    );
    assert!(
        log_content.contains("[error]"),
        "log file should contain an [error] entry, got:
{}",
        log_content
    );
    assert!(
        log_content.contains("Failed to start node"),
        "log file should contain the failure message, got:
{}",
        log_content
    );
    assert!(
        log_content.contains("nonexistent_binary_peppy_test_xyz"),
        "log file should contain the command that failed, got:
{}",
        log_content
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_run_marks_node_unhealthy_on_failed_health_checks() {
    const TARGET_NODE_NAME: &str = "health_monitor_node";
    const TARGET_NODE_TAG: &str = "v1";
    const TARGET_INSTANCE_ID: &str = "health_monitor_instance";

    // Use fast health monitor settings so the test completes quickly:
    // check every 200ms with a 100ms per-attempt timeout.
    let started =
        start_core_node_with_health_monitor(Duration::from_millis(200), Duration::from_millis(100))
            .await;

    // Create a node with a simple long-running run_cmd
    let peppy_json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "300"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);

    let temp_dir = create_node_config_dir(&peppy_json5);

    let add_response = send_node_add_then_build(
        &started.caller_handle,
        &started.core_node_name,
        temp_dir.path(),
        Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .await
    .expect("node_add should succeed");

    assert!(
        add_response.success,
        "node_add should succeed, got error: {:?}",
        add_response.error_message
    );

    // Set up ready+health listeners so the initial start succeeds
    let node_messenger = MessengerHandle::from_shared(Arc::clone(&started.shared_messenger));
    let _ready_task = AbortOnDrop(
        listen_for_node_ready(
            &node_messenger,
            &started.core_node_name,
            TARGET_INSTANCE_ID,
            common::test_node_target(TARGET_NODE_NAME),
        )
        .await
        .expect("node ready service should start"),
    );
    let health_task = AbortOnDrop(
        listen_for_node_health(
            &node_messenger,
            &started.core_node_name,
            TARGET_INSTANCE_ID,
            common::test_node_target(TARGET_NODE_NAME),
        )
        .await
        .expect("node health service should start"),
    );

    // Allow ready/health services to establish listeners
    tokio::time::sleep(Duration::from_millis(50)).await;

    let runtime_config_json5 = common::default_runtime_config_json5(
        &started.core_node_name,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        TARGET_INSTANCE_ID,
    );

    let start_response = send_node_run_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        &runtime_config_json5,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &NodeRunTestTimeouts {
            goal: Duration::from_secs(10),
            result: Duration::from_secs(30),
        },
        None,
    )
    .await
    .expect("node_run action should complete");

    assert!(
        start_response.result.success,
        "node_run should succeed, got error: {:?}",
        start_response.result.error_message
    );

    let pid = start_response
        .result
        .pid
        .expect("should have a PID on success");

    // Verify the instance is in the stack
    let instance_id = NodeName::new(TARGET_INSTANCE_ID).expect("valid instance id");
    assert!(
        started
            .node_stack
            .find_by_instance_id(&instance_id)
            .is_some(),
        "instance should be in the stack after successful start"
    );

    // Kill the process to simulate an unexpected death
    let _ = std::process::Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status();

    // Drop the health listener so subsequent health polls from the monitor
    // will fail (the node process is dead and the mock health service is gone)
    drop(health_task);

    // Wait for the health monitor to detect the failure and mark the instance
    // unhealthy. With interval=200ms and timeout=100ms the first failed probe
    // lands within ~300ms; give it 5 seconds for CI safety. The instance must
    // stay in the stack the whole time, only its health flag flips.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match started.node_stack.find_by_instance_id(&instance_id) {
            Some(instance) if !instance.healthy() => break,
            Some(_) => {}
            None => {
                panic!("instance was removed from the stack; it should only be marked unhealthy")
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("instance was not marked unhealthy within timeout");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The unhealthy instance stays tracked, so `stack list` / `node info` keep
    // reporting it instead of it vanishing from the stack.
    assert!(
        started
            .node_stack
            .find_by_instance_id(&instance_id)
            .is_some_and(|instance| !instance.healthy()),
        "unhealthy instance should remain tracked in the stack"
    );

    // Verify the stack log recorded the unhealthy transition.
    let stack_log_path = started.peppy_dirs.stack_log_path();
    assert!(
        stack_log_path.exists(),
        "stack log file should exist at {:?}",
        stack_log_path
    );
    let log_content =
        std::fs::read_to_string(&stack_log_path).expect("should be able to read stack log");
    assert!(
        log_content.contains(TARGET_INSTANCE_ID),
        "stack log should mention the instance id, got:
{}",
        log_content
    );
    assert!(
        log_content.contains("unhealthy"),
        "stack log should record the unhealthy transition, got:
{}",
        log_content
    );
    assert!(
        log_content.contains(TARGET_NODE_NAME),
        "stack log should mention the node name, got:
{}",
        log_content
    );
}
