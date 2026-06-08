//! Component test for `teardown_all_instances` — the daemon-side force-kill the
//! serve runner invokes on a catchable shutdown (ctrl+C / SIGTERM).
//!
//! Spawns a real node whose `run_cmd` forks grandchildren, all in the node's
//! process group (nodes are spawned as group leaders). The node does NOT answer
//! the cooperative `SHUTDOWN_SERVICE` (a "stuck" instance), so the graceful
//! phase times out and the force phase must SIGKILL the whole group — proving no
//! node, and none of its descendants, is left orphaned. The core (root) node is
//! never killed: that's the daemon (this test process) itself, so the test
//! simply continuing to run is the proof.

mod common;

use common::{
    build_staged_node, send_node_add_and_wait, spawn_real_stuck_instance,
    start_core_node_with_mock_messenger, write_peppy_json5,
};
use config::node::Name;
use peppylib::messaging::MessengerHandle;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// True if a process exists and is not a zombie (matches the daemon's own
/// `is_process_running` definition, so the test agrees with what teardown
/// considers "gone").
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
async fn teardown_force_kills_whole_process_group() {
    const NODE_NAME: &str = "doomed_node";
    const NODE_TAG: &str = "v1";
    const INSTANCE_ID: &str = "doomed_instance";

    let started = start_core_node_with_mock_messenger().await;
    // Shares the same inner graph as the core node's stack, so the spawned
    // instance is visible to teardown.
    let node_stack = Arc::new(started.node_stack.clone());

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
    // "stuck": does NOT answer SHUTDOWN_SERVICE, so teardown must force-kill.
    let running = spawn_real_stuck_instance(&started, NODE_NAME, NODE_TAG, &instance_id).await;
    let node_pid = running.pid;
    // Drop the guard's stop-on-drop so only `teardown_all_instances` kills it.
    std::mem::forget(running);

    // Wait for the two grandchildren to appear, then snapshot their pids.
    assert!(
        poll_until(Duration::from_secs(5), || children_of(node_pid).len() >= 2),
        "expected the node to fork two grandchildren"
    );
    let grandchildren = children_of(node_pid);
    assert!(
        is_process_running(node_pid),
        "node process {node_pid} should be running before teardown"
    );
    for &gc in &grandchildren {
        assert!(
            is_process_running(gc),
            "grandchild {gc} should be running before teardown"
        );
    }

    // The system under test: cooperative phase times out (stuck node), then the
    // force phase SIGKILLs the whole group.
    let messenger = MessengerHandle::from_shared(Arc::clone(&started.shared_messenger));
    core_node::teardown_all_instances(&messenger, &started.core_node_name, "core", &node_stack)
        .await;

    // The node and every grandchild must be gone — no orphans.
    assert!(
        !is_process_running(node_pid),
        "node process {node_pid} should be gone after teardown"
    );
    for &gc in &grandchildren {
        assert!(
            poll_until(Duration::from_secs(5), || !is_process_running(gc)),
            "grandchild {gc} should be gone after teardown (group kill)"
        );
    }

    // The core (root) node — i.e. this test process — must be untouched.
    assert!(
        is_process_running(std::process::id()),
        "teardown must never kill the root/core node (the daemon itself)"
    );
}
