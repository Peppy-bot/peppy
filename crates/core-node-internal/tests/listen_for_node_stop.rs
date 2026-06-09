mod common;

use common::{
    AbortOnDrop, CALLER_INSTANCE_ID, build_staged_node, send_node_add_and_wait,
    spawn_real_running_instance, spawn_real_stuck_instance, start_core_node_with_mock_messenger,
    write_peppy_json5,
};
use config::node::Name;
use core_node_api::encoding::NodeStopRequest;
use peppylib::core_node::transport::poll_node_stop;
use peppylib::messaging::MessengerHandle;
use peppylib::services::shutdown::listen_for_shutdown;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// True if a process exists and is not a zombie — matches the daemon's own
/// `is_process_running` definition (sysinfo, status != Zombie), so the test
/// agrees with what `node_stop`/teardown consider "gone". A libc `kill(pid, 0)`
/// check would report a reaped-but-unwaited zombie as still running.
fn is_process_running(pid: u32) -> bool {
    let system = sysinfo::System::new_with_specifics(
        sysinfo::RefreshKind::nothing().with_processes(sysinfo::ProcessRefreshKind::nothing()),
    );
    match system.process(sysinfo::Pid::from_u32(pid)) {
        Some(process) => process.status() != sysinfo::ProcessStatus::Zombie,
        None => false,
    }
}

/// PIDs of live children of `parent_pid`, via sysinfo's parent links.
fn children_of(parent_pid: u32) -> Vec<u32> {
    let system = sysinfo::System::new_with_specifics(
        sysinfo::RefreshKind::nothing().with_processes(sysinfo::ProcessRefreshKind::everything()),
    );
    let parent = sysinfo::Pid::from_u32(parent_pid);
    system
        .processes()
        .values()
        .filter(|p| p.parent() == Some(parent))
        .map(|p| p.pid().as_u32())
        .collect()
}

fn poll_until(deadline: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    cond()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_stop_success() {
    const TARGET_NODE_NAME: &str = "stoppable_node";
    const TARGET_NODE_TAG: &str = "v1";
    const TARGET_INSTANCE_ID: &str = "stoppable_instance";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    // Add the node to the stack so it can be discovered by instance_id
    let peppy_json5 = r#"{
            peppy_schema: "node_v1",
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_response = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
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
    build_staged_node(&started_core_node, TARGET_NODE_NAME, TARGET_NODE_TAG).await;

    // Drive the real start lifecycle so the entity tracks a live child
    // process (spawned from the node's `run_cmd = ["sleep", "10"]`).
    let instance_id = Name::new(TARGET_INSTANCE_ID).expect("valid instance id");
    let running = spawn_real_running_instance(
        &started_core_node,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &instance_id,
    )
    .await;
    let pid = running.pid;
    // Drop the guard's stop-on-drop behavior by forgetting it — node_stop
    // itself is responsible for reaping the child in this test.
    std::mem::forget(running);

    // Verify the process is running before we try to stop it
    assert!(
        is_process_running(pid),
        "process {} should be running before stop",
        pid
    );

    // Simulate the target node exposing the shutdown service.
    // When it receives the shutdown signal, it will kill the actual process.
    let shutdown_handle =
        MessengerHandle::from_shared(Arc::clone(&started_core_node.shared_messenger));
    let (shutdown_task, shutdown_rx) = listen_for_shutdown(
        &shutdown_handle,
        &started_core_node.core_node_name,
        TARGET_INSTANCE_ID,
        common::test_node_target(TARGET_NODE_NAME),
    )
    .await
    .expect("failed to start shutdown service");
    let _shutdown_task = AbortOnDrop(shutdown_task);

    // Allow the shutdown service to fully establish its listener
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Spawn a task to SIGKILL the entity-tracked pid when shutdown is
    // received (simulating the target node's own exit path).
    let kill_task = tokio::spawn(async move {
        shutdown_rx
            .await
            .expect("shutdown channel should not be dropped");
        let _ = std::process::Command::new("kill")
            .arg("-KILL")
            .arg(pid.to_string())
            .status();
    });

    let response = poll_node_stop(
        &NodeStopRequest::new(TARGET_INSTANCE_ID),
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        CALLER_INSTANCE_ID,
        common::core_node_target(&started_core_node.core_node_name),
        &started_core_node.core_node_name,
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
    // The node honored the cooperative shutdown, so it must NOT be reported as
    // force-killed.
    assert!(
        !response.force_killed,
        "a cooperatively-stopped node should not be reported as force-killed"
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_stop_fails_when_instance_id_not_found() {
    const MISSING_INSTANCE_ID: &str = "missing_instance";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let response = poll_node_stop(
        &NodeStopRequest::new(MISSING_INSTANCE_ID),
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        CALLER_INSTANCE_ID,
        common::core_node_target(&started_core_node.core_node_name),
        &started_core_node.core_node_name,
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
}

/// `node_stop` must behave like the daemon's SIGINT teardown: a node that
/// ignores the cooperative `SHUTDOWN_SERVICE` is force-killed by process group,
/// and the call returns success only once the whole group is gone — no orphan.
///
/// The node forks two grandchildren and waits; all three share the node's
/// process group (nodes are spawned as group leaders). The instance is spawned
/// "stuck" (no shutdown listener installed), so the cooperative phase times out
/// and `node_stop`'s force phase must SIGKILL the whole group. Mirrors
/// `teardown_all_instances.rs::teardown_force_kills_whole_process_group`, but
/// drives it through the real `node_stop` service via `poll_node_stop`. Before
/// the force-kill fix this returned a failure and left the process alive.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_stop_force_kills_whole_process_group() {
    const NODE_NAME: &str = "stuck_stop_node";
    const NODE_TAG: &str = "v1";
    const INSTANCE_ID: &str = "stuck_stop_instance";

    let started = start_core_node_with_mock_messenger().await;
    // Shares the same inner graph as the live core node's stack, so the spawned
    // instance is visible to the node_stop handler and we can assert removal.
    let node_stack = started.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("temp source dir");
    // The node forks two grandchildren and waits; all three share the node's
    // process group (the node is spawned as group leader).
    let peppy_json5 = r#"{
            peppy_schema: "node_v1",
            manifest: { name: "{NAME}", tag: "{TAG}" },
            execution: {
                language: "rust",
                run_cmd: ["sh", "-c", "sleep 1000 & sleep 1000 & wait"]
            }
        }"#
    .replace("{NAME}", NODE_NAME)
    .replace("{TAG}", NODE_TAG);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_response = send_node_add_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        source_dir.path(),
        Duration::from_secs(5),
        Duration::from_secs(5),
        None,
    )
    .await
    .expect("node_add should complete");
    assert!(add_response.success, "node_add failed: {add_response:?}");
    build_staged_node(&started, NODE_NAME, NODE_TAG).await;

    let instance_id = Name::new(INSTANCE_ID).expect("valid instance id");
    // "stuck": installs NO shutdown listener, so the node never answers
    // SHUTDOWN_SERVICE and node_stop must force-kill it.
    let running = spawn_real_stuck_instance(&started, NODE_NAME, NODE_TAG, &instance_id).await;
    let node_pid = running.pid;
    // Drop the guard's stop-on-drop so only node_stop (the SUT) kills it.
    std::mem::forget(running);

    // Wait for the two grandchildren to appear, then snapshot their pids.
    assert!(
        poll_until(Duration::from_secs(5), || children_of(node_pid).len() >= 2),
        "expected the node to fork two grandchildren"
    );
    let grandchildren = children_of(node_pid);
    assert!(
        is_process_running(node_pid),
        "node process {node_pid} should be running before stop"
    );
    for &gc in &grandchildren {
        assert!(
            is_process_running(gc),
            "grandchild {gc} should be running before stop"
        );
    }

    // System under test: cooperative phase times out (stuck node), then the
    // force phase SIGKILLs the whole group. The timeout must exceed the
    // handler's graceful (3s) + reap (2s) budget plus messaging round-trips.
    let response = poll_node_stop(
        &NodeStopRequest::new(INSTANCE_ID),
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        common::core_node_target(&started.core_node_name),
        &started.core_node_name,
        Duration::from_secs(20),
    )
    .await
    .expect("node_stop request should complete");

    // New behavior: a stuck node is force-killed and reported as success
    // (it used to fail with "did not terminate within timeout").
    assert!(
        response.success,
        "node_stop must force-kill a stuck node and succeed, got error: {:?}",
        response.error_message
    );
    assert!(
        response.error_message.is_none(),
        "success response should not include error_message, got: {:?}",
        response.error_message
    );
    // The user must be told this was a force-kill, not a graceful exit.
    assert!(
        response.force_killed,
        "a stuck node that ignored shutdown should be reported as force-killed"
    );

    // No orphans: the node and every grandchild must be gone. Poll rather than
    // assert synchronously — the reap is best-effort under a bounded timeout, so
    // success may be reported a beat before the kernel finishes the teardown.
    assert!(
        poll_until(Duration::from_secs(5), || !is_process_running(node_pid)),
        "node process {node_pid} should be gone after node_stop"
    );
    for &gc in &grandchildren {
        assert!(
            poll_until(Duration::from_secs(5), || !is_process_running(gc)),
            "grandchild {gc} should be gone after node_stop (group kill)"
        );
    }

    // The instance must be removed from the registry (find_by_instance_id
    // returns Some only for Running instances).
    assert!(
        node_stack.find_by_instance_id(&instance_id).is_none(),
        "instance should be removed from the node stack after a successful stop"
    );

    // The core (root) node — i.e. this test process — must be untouched.
    assert!(
        is_process_running(std::process::id()),
        "node_stop must never kill the root/core node (the daemon itself)"
    );
}
