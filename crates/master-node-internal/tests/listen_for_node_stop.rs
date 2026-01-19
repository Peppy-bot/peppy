mod common;

use common::{
    CALLER_INSTANCE_ID, send_node_add_and_wait, start_master_node_with_mock_messenger,
    write_peppy_json5,
};
use config::node::Name;
use master_node::encoding::NodeStopRequest;
use peppylib::messaging::MessengerHandle;
use peppylib::services::shutdown::listen_for_shutdown;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

/// Checks if a process with the given PID is still running.
fn is_process_running(pid: u32) -> bool {
    // SAFETY: kill(pid, 0) is safe - it doesn't send any signal,
    // just checks if the process exists and we have permission to signal it.
    let result = unsafe { libc::kill(pid as i32, 0) };
    if result == 0 {
        true
    } else {
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        errno != libc::ESRCH
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_stop_success() {
    const TARGET_NODE_NAME: &str = "stoppable_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const TARGET_INSTANCE_ID: &str = "stoppable_instance";

    let started_master = start_master_node_with_mock_messenger().await;
    let node_stack = started_master.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    // Add the node to the stack so it can be discovered by instance_id
    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
                start_cmd: ["sleep", "10"]
            }},
            parameters: {{}}
        }}"#
    );
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_response = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
        source_dir.path(),
        Duration::from_secs(5),
        Duration::from_secs(5),
        None,
    )
    .await
    .expect("node_add should complete");

    assert!(
        add_response.success,
        "node_add should succeed, got error: {:?}",
        add_response.error_message
    );
    assert!(node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG));

    // Spawn a real process to simulate the running node
    let mut child_process = Command::new("sleep")
        .arg("60")
        .spawn()
        .expect("failed to spawn sleep process");
    let pid = child_process.id();

    // Register the instance with the actual PID
    let instance_id = Name::new(TARGET_INSTANCE_ID).expect("valid instance id");
    node_stack
        .add_instance(
            TARGET_NODE_NAME,
            TARGET_NODE_TAG,
            Some(&instance_id),
            Some(pid),
        )
        .expect("add_instance should succeed");

    // Verify the process is running before we try to stop it
    assert!(
        is_process_running(pid),
        "process {} should be running before stop",
        pid
    );

    // Simulate the target node exposing the shutdown service.
    // When it receives the shutdown signal, it will kill the actual process.
    let shutdown_handle =
        MessengerHandle::from_shared(Arc::clone(&started_master.shared_messenger));
    let (shutdown_task, shutdown_rx) = listen_for_shutdown(
        &shutdown_handle,
        &started_master.master_node_name,
        TARGET_INSTANCE_ID,
        TARGET_NODE_NAME,
    )
    .await
    .expect("failed to start shutdown service");

    // Allow the shutdown service to fully establish its listener
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Spawn a task to kill the process when shutdown is received
    let kill_task = tokio::spawn(async move {
        shutdown_rx
            .await
            .expect("shutdown channel should not be dropped");
        // Kill the actual process when shutdown signal is received
        child_process.kill().expect("failed to kill child process");
        child_process
            .wait()
            .expect("failed to wait for child process");
    });

    let response = NodeStopRequest::new(TARGET_INSTANCE_ID)
        .poll(
            &started_master.caller_handle,
            &started_master.master_node_name,
            CALLER_INSTANCE_ID,
            &started_master.master_node_name,
            &started_master.master_node_name,
            Duration::from_secs(10),
        )
        .await
        .expect("node_stop request should complete");

    assert!(response.success, "node_stop should succeed");
    assert!(
        response.error_message.is_none(),
        "success response should not include error_message, got: {:?}",
        response.error_message
    );

    // Verify the process has been killed
    assert!(
        !is_process_running(pid),
        "process {} should no longer be running after successful stop",
        pid
    );

    // Wait for the kill task to complete
    tokio::time::timeout(Duration::from_millis(500), kill_task)
        .await
        .expect("kill task should complete within timeout")
        .expect("kill task should not panic");

    shutdown_task.abort();
    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_stop_fails_when_instance_id_not_found() {
    const MISSING_INSTANCE_ID: &str = "missing_instance";

    let started_master = start_master_node_with_mock_messenger().await;

    let response = NodeStopRequest::new(MISSING_INSTANCE_ID)
        .poll(
            &started_master.caller_handle,
            &started_master.master_node_name,
            CALLER_INSTANCE_ID,
            &started_master.master_node_name,
            &started_master.master_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_stop request should complete");

    assert!(!response.success, "node_stop should fail");
    let error_message = response
        .error_message
        .as_ref()
        .expect("node_stop failure should include error_message");
    assert!(
        error_message.contains("not found in node stack"),
        "error should mention missing instance, got: {}",
        error_message
    );
    assert!(
        error_message.contains(MISSING_INSTANCE_ID),
        "error should include missing instance id, got: {}",
        error_message
    );

    started_master.task.abort();
}
