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
    add_and_build_forking_node, children_of, is_process_running, poll_until,
    spawn_real_starting_instance, spawn_real_stuck_instance, start_core_node_with_mock_messenger,
};
use config::node::Name;
use peppylib::messaging::MessengerHandle;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn teardown_force_kills_whole_process_group() {
    const NODE_NAME: &str = "doomed_node";
    const NODE_TAG: &str = "v1";
    const INSTANCE_ID: &str = "doomed_instance";

    let started = start_core_node_with_mock_messenger().await;
    // Shares the same inner graph as the core node's stack, so the spawned
    // instance is visible to teardown.
    let node_stack = Arc::new(started.node_stack.clone());

    let _source_dir = add_and_build_forking_node(&started, NODE_NAME, NODE_TAG).await;

    let instance_id = Name::new(INSTANCE_ID).expect("valid instance id");
    // "stuck": does NOT answer SHUTDOWN_SERVICE, so teardown must force-kill.
    let running = spawn_real_stuck_instance(&started, NODE_NAME, NODE_TAG, &instance_id).await;
    let node_pid = running.pid;
    // Drop the guard's stop-on-drop so only `teardown_all_instances` kills it.
    std::mem::forget(running);

    // Wait for the two grandchildren to appear, then snapshot their pids.
    poll_until(
        Duration::from_secs(5),
        "expected the node to fork two grandchildren",
        || (children_of(node_pid).len() >= 2).then_some(()),
    )
    .await;
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
        poll_until(
            Duration::from_secs(5),
            &format!("grandchild {gc} should be gone after teardown (group kill)"),
            || (!is_process_running(gc)).then_some(()),
        )
        .await;
    }

    // The core (root) node — i.e. this test process — must be untouched.
    assert!(
        is_process_running(std::process::id()),
        "teardown must never kill the root/core node (the daemon itself)"
    );
}

/// Teardown must also force-kill a node caught mid-launch (`Starting`): the
/// child is spawned and its pid recorded inside `prepare_and_spawn`, before
/// `commit_started` runs. Regression guard for the `Starting`-window orphan and
/// for keeping `Starting` instances in `collect_doomed_instances`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn teardown_force_kills_instance_still_in_starting() {
    const NODE_NAME: &str = "starting_node";
    const NODE_TAG: &str = "v1";
    const INSTANCE_ID: &str = "starting_instance";

    let started = start_core_node_with_mock_messenger().await;
    let node_stack = Arc::new(started.node_stack.clone());

    let _source_dir = add_and_build_forking_node(&started, NODE_NAME, NODE_TAG).await;

    let instance_id = Name::new(INSTANCE_ID).expect("valid instance id");
    // Drive prepare_and_spawn WITHOUT commit_started: the instance stays in
    // `Starting` with a live child — the mid-launch state that, before the fix,
    // carried no pid in the registry and so was skipped by the force phase.
    let starting = spawn_real_starting_instance(&started, NODE_NAME, NODE_TAG, &instance_id).await;
    let node_pid = starting.pid;

    // The instance really is in `Starting`, and already carries the pid the
    // force phase needs (recorded inside prepare_and_spawn under the entity lock).
    {
        let handle = started
            .node_stack
            .find(NODE_NAME, NODE_TAG)
            .expect("entity should exist");
        let guard = handle.read();
        let inst = guard
            .instances()
            .iter()
            .find(|i| i.instance_id() == &instance_id)
            .expect("starting instance should be registered");
        assert_eq!(
            inst.state(),
            node_stack::InstanceState::Starting,
            "instance must still be Starting (commit_started was not called)"
        );
        assert_eq!(
            inst.pid(),
            Some(node_pid),
            "Starting instance must carry the spawned child's pid"
        );
    }

    // Wait for the two grandchildren to appear, then snapshot their pids.
    poll_until(
        Duration::from_secs(5),
        "expected the node to fork two grandchildren",
        || (children_of(node_pid).len() >= 2).then_some(()),
    )
    .await;
    let grandchildren = children_of(node_pid);
    assert!(
        is_process_running(node_pid),
        "Starting node {node_pid} should be running before teardown"
    );

    // System under test: teardown force-kills the whole group of a Starting node.
    let messenger = MessengerHandle::from_shared(Arc::clone(&started.shared_messenger));
    core_node::teardown_all_instances(&messenger, &started.core_node_name, "core", &node_stack)
        .await;

    assert!(
        !is_process_running(node_pid),
        "Starting node {node_pid} should be gone after teardown"
    );
    for &gc in &grandchildren {
        poll_until(
            Duration::from_secs(5),
            &format!("grandchild {gc} should be gone after teardown (group kill)"),
            || (!is_process_running(gc)).then_some(()),
        )
        .await;
    }

    // Drop the guard explicitly (its best-effort group kill is now a no-op).
    drop(starting);
}
