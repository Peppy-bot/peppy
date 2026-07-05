mod common;

use common::{
    AbortOnDrop, CALLER_INSTANCE_ID, NodeRunTestTimeouts, add_and_build_forking_node,
    build_staged_node, children_of, create_test_node_with_name, install_kill_on_shutdown_listener,
    instance_state_in_any_state, is_process_running, poll_until, send_node_add_and_wait,
    send_node_add_then_build, send_node_run_and_wait, spawn_real_running_instance,
    spawn_real_stuck_instance, start_core_node_with_mock_messenger,
    start_core_node_with_real_messenger, write_peppy_json5,
};
use config::runtime::Name;
use core_node::force_kill_deadline;
use core_node_api::encoding::{NodeStopRequest, NodeStopResponse};
use core_node_api::names;
use peppylib::core_node::transport::poll_node_stop;
use peppylib::messaging::{MessengerHandle, ServiceMessenger};
use peppylib::services::health::listen_for_node_health;
use peppylib::services::ready::listen_for_node_ready;
use peppylib::services::shutdown::listen_for_shutdown;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

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
            peppy_schema: "node/v1",
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
    // Drop the guard's stop-on-drop behavior by forgetting it; node_stop
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

/// A cooperative node that legitimately takes longer than the bare grace window
/// to disappear (a real one runs its hooks within `grace`, then a Python node
/// joins its asyncio loop and the interpreter finalizes) must still be reported
/// graceful, not force-killed. This pins the Issue 2 fix: the daemon's
/// force-kill deadline is `force_kill_deadline(grace)` (grace + loop-join +
/// finalize), not the bare `grace`. We make the node exit at `grace + 2s` (past
/// the old `grace` deadline, comfortably inside the new one) and assert it is
/// classified graceful. Under the old behavior the process is still alive at the
/// `grace` deadline and would be SIGKILLed and reported force-killed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_stop_reports_graceful_for_node_that_exits_after_grace_but_within_deadline() {
    const TARGET_NODE_NAME: &str = "slow_cooperative_node";
    const TARGET_NODE_TAG: &str = "v1";
    const TARGET_INSTANCE_ID: &str = "slow_cooperative_instance";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let grace = node_stack.shutdown_grace();
    let deadline = force_kill_deadline(grace);
    // Exit after the bare grace but well inside the full deadline, so the test
    // genuinely distinguishes "daemon waits grace" (old, force-kill) from
    // "daemon waits grace + teardown margin" (new, graceful).
    let exit_delay = grace + Duration::from_secs(2);
    assert!(
        exit_delay > grace && exit_delay < deadline,
        "exit delay {exit_delay:?} must sit between the grace {grace:?} and the \
         force-kill deadline {deadline:?} for this test to be meaningful",
    );

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "60"]
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
    build_staged_node(&started_core_node, TARGET_NODE_NAME, TARGET_NODE_TAG).await;

    let instance_id = Name::new(TARGET_INSTANCE_ID).expect("valid instance id");
    // No built-in shutdown listener: our delayed-kill listener below is the only
    // one, so the process exits exactly at `exit_delay` rather than immediately.
    let running = spawn_real_stuck_instance(
        &started_core_node,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &instance_id,
    )
    .await;
    let pid = running.pid;
    std::mem::forget(running); // node_stop is responsible for reaping the child.
    assert!(
        is_process_running(pid),
        "process {pid} should be running before stop"
    );

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
    tokio::time::sleep(Duration::from_millis(50)).await;

    // The cooperative node acks the shutdown immediately but only exits after
    // `exit_delay`, modeling hook cleanup plus runtime teardown that runs past
    // the bare grace window.
    let kill_task = tokio::spawn(async move {
        shutdown_rx
            .await
            .expect("shutdown channel should not be dropped");
        tokio::time::sleep(exit_delay).await;
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
        deadline + Duration::from_secs(5),
    )
    .await
    .expect("node_stop request should complete");

    assert!(response.success, "node_stop should succeed");
    assert!(
        !response.force_killed,
        "a node that exits cooperatively after the grace window but within the \
         force-kill deadline must be reported graceful, not force-killed"
    );
    assert!(
        !is_process_running(pid),
        "process {pid} should be gone after a graceful stop"
    );

    tokio::time::timeout(Duration::from_secs(1), kill_task)
        .await
        .expect("kill task should complete")
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
/// and the call returns success only once the whole group is gone, with no orphan.
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

    let _source_dir = add_and_build_forking_node(&started, NODE_NAME, NODE_TAG).await;

    let instance_id = Name::new(INSTANCE_ID).expect("valid instance id");
    // "stuck": installs NO shutdown listener, so the node never answers
    // SHUTDOWN_SERVICE and node_stop must force-kill it.
    let running = spawn_real_stuck_instance(&started, NODE_NAME, NODE_TAG, &instance_id).await;
    let node_pid = running.pid;
    // Drop the guard's stop-on-drop so only node_stop (the SUT) kills it.
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
    // handler's full force-kill deadline (grace + runtime teardown) plus the
    // reap budget and messaging round-trips.
    let stop_timeout = force_kill_deadline(node_stack.shutdown_grace()) + Duration::from_secs(8);
    let response = poll_node_stop(
        &NodeStopRequest::new(INSTANCE_ID),
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        common::core_node_target(&started.core_node_name),
        stop_timeout,
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
    // assert synchronously; the reap is best-effort under a bounded timeout, so
    // success may be reported a beat before the kernel finishes the teardown.
    poll_until(
        Duration::from_secs(5),
        &format!("node process {node_pid} should be gone after node_stop"),
        || (!is_process_running(node_pid)).then_some(()),
    )
    .await;
    for &gc in &grandchildren {
        poll_until(
            Duration::from_secs(5),
            &format!("grandchild {gc} should be gone after node_stop (group kill)"),
            || (!is_process_running(gc)).then_some(()),
        )
        .await;
    }

    // The instance must be removed from the registry (find_by_instance_id
    // returns Some only for Running instances).
    assert!(
        node_stack.find_by_instance_id(&instance_id).is_none(),
        "instance should be removed from the node stack after a successful stop"
    );

    // The core (root) node, i.e. this test process, must be untouched.
    assert!(
        is_process_running(std::process::id()),
        "node_stop must never kill the root/core node (the daemon itself)"
    );
}

/// Full end-to-end graceful stop with a REAL peppylib node: a compiled
/// `NodeBuilder` binary is started through the real `node_run` action over
/// real zenoh messaging, then stopped through the real `node_stop` service.
/// The node's runtime must receive the cooperative `SHUTDOWN_SERVICE`, cancel
/// its cancellation token, and exit within the grace window, so the stop is
/// classified graceful (`force_killed == false`) rather than force-killed.
///
/// This closes the loop between the daemon-side tests above (which simulate
/// the node's shutdown listener in-process) and the node-side tests in
/// `public-peppy-libs/peppy-shared/peppylib-rs/tests/runner.rs` (which simulate the daemon's shutdown
/// send): here both halves are the production code.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_stop_reports_graceful_for_real_node_builder_node() {
    const TARGET_NODE_NAME: &str = "graceful_builder_node";
    const TARGET_NODE_TAG: &str = "v1";
    const TARGET_INSTANCE_ID: &str = "graceful_builder_instance";

    let started = start_core_node_with_real_messenger().await;
    let node_stack = started.node_stack.clone();

    // A real cargo node whose main() is `NodeBuilder::new().run(...)`. Held
    // for the whole test body; the TempDir guard reclaims the build dir.
    let node_dir = create_test_node_with_name(TARGET_NODE_NAME, TARGET_NODE_TAG);

    let add_response = send_node_add_then_build(
        &started.caller_handle,
        &started.core_node_name,
        node_dir.path(),
        Duration::from_secs(30),
        // Longer timeout to account for copying the test node folder, which
        // includes build artifacts.
        Duration::from_secs(120),
    )
    .await
    .expect("node_add should complete");
    assert!(
        add_response.success,
        "node_add should succeed, got error: {:?}",
        add_response.error_message
    );

    // Point the node's runtime config at the real zenoh endpoint so the
    // spawned process can join the messaging network.
    let (messaging_host, messaging_port) = started
        .caller_handle
        .messaging_endpoint()
        .await
        .expect("zenoh endpoint should be available");
    let runtime_config_json5 = common::build_runtime_config_json5(
        messaging_host.as_str(),
        messaging_port,
        &started.core_node_name,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        TARGET_INSTANCE_ID,
        Default::default(),
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
    assert!(
        start_response.result.success,
        "node_run should succeed, got error: {:?}",
        start_response.result.error_message
    );
    let pid = start_response
        .result
        .pid
        .expect("node_run should return a pid");
    assert!(
        is_process_running(pid),
        "node process {pid} should be running before stop"
    );

    // System under test: the real stop handler sends SHUTDOWN_SERVICE, the
    // node's NodeBuilder runtime cancels its cancellation token and exits.
    // The timeout must exceed the handler's full force-kill deadline (grace +
    // runtime teardown) plus the reap budget and messaging round-trips.
    let stop_timeout = force_kill_deadline(node_stack.shutdown_grace()) + Duration::from_secs(8);
    let response = poll_node_stop(
        &NodeStopRequest::new(TARGET_INSTANCE_ID),
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        common::core_node_target(&started.core_node_name),
        stop_timeout,
    )
    .await
    .expect("node_stop request should complete");

    assert!(
        response.success,
        "node_stop should succeed, got error: {:?}",
        response.error_message
    );
    // The whole point: a real NodeBuilder node must honor the cooperative
    // shutdown within the grace period and never be reported as force-killed.
    assert!(
        !response.force_killed,
        "a real NodeBuilder node should exit cooperatively, not be force-killed"
    );

    // The process must be gone. Poll rather than assert synchronously: the
    // reap is best-effort under a bounded timeout.
    poll_until(
        Duration::from_secs(5),
        &format!("node process {pid} should be gone after node_stop"),
        || (!is_process_running(pid)).then_some(()),
    )
    .await;

    let instance_id = Name::new(TARGET_INSTANCE_ID).expect("valid instance id");
    assert!(
        node_stack.find_by_instance_id(&instance_id).is_none(),
        "instance should be removed from the node stack after a graceful stop"
    );
}

/// Regression for `node_stop` routing on a multi-daemon network: user node
/// names are not unique across core nodes, so two `node_stop` listeners with
/// the SAME node name + tag can coexist on DIFFERENT core nodes (the
/// per-instance-listener case `node_stop` is hand-written for). The service
/// root encodes only name + tag, so an unscoped discovery could be won by the
/// foreign core node's listener, which would answer "unknown instance" while
/// the right reply is dropped. `poll_node_stop` scopes its discovery to the
/// caller's bound core node, so the foreign listener must never answer.
///
/// Before the scoping fix, each iteration was a discovery race the foreign
/// listener (registered first, to bias the race against us) could win.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_stop_scoped_to_bound_core_node_ignores_foreign_listener() {
    const NODE_NAME: &str = "camera";
    const LOCAL_CORE_NODE: &str = "local_core_node";
    const LOCAL_LISTENER_INSTANCE_ID: &str = "local_listener_instance";
    const FOREIGN_CORE_NODE: &str = "foreign_core_node";
    const FOREIGN_LISTENER_INSTANCE_ID: &str = "foreign_listener_instance";
    const TARGET_INSTANCE_ID: &str = "doomed_instance";

    // One mock messaging network shared by both core nodes and the caller:
    // the multi-daemon topology where both listeners' queryables would match
    // an unscoped `node_stop` selector.
    let shared_messenger = common::create_mock_messenger().await;

    // Foreign listener FIRST: same node name + tag as the local one, hosted
    // by a different core node. It must never see a scoped request; if it
    // ever answers, the distinctive failure below fails the assertions.
    let foreign_hits = Arc::new(AtomicUsize::new(0));
    let _foreign_listener = {
        let handle = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
        let mut endpoint = ServiceMessenger::listen(
            &handle,
            FOREIGN_CORE_NODE,
            FOREIGN_LISTENER_INSTANCE_ID,
            common::test_node_target(NODE_NAME),
            names::NODE_STOP,
        )
        .await
        .expect("foreign node_stop listener should start");
        let foreign_hits = Arc::clone(&foreign_hits);
        AbortOnDrop(peppylib::runtime::spawn(async move {
            endpoint
                .handle_requests(move |_context| {
                    foreign_hits.fetch_add(1, Ordering::SeqCst);
                    async move {
                        NodeStopResponse::failure(
                            "foreign core node must never answer a scoped node_stop",
                        )
                        .encode()
                        .map_err(Into::into)
                    }
                })
                .await
        }))
    };

    let _local_listener = {
        let handle = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
        let mut endpoint = ServiceMessenger::listen(
            &handle,
            LOCAL_CORE_NODE,
            LOCAL_LISTENER_INSTANCE_ID,
            common::test_node_target(NODE_NAME),
            names::NODE_STOP,
        )
        .await
        .expect("local node_stop listener should start");
        AbortOnDrop(peppylib::runtime::spawn(async move {
            endpoint
                .handle_requests(|_context| async move {
                    NodeStopResponse::success().encode().map_err(Into::into)
                })
                .await
        }))
    };

    // Allow both listeners to fully establish their queryables.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Each iteration runs a fresh discover-then-pin sequence; without the
    // bound-core-node scope the foreign listener could win any of them.
    let caller_handle = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    for i in 0..10 {
        let response = poll_node_stop(
            &NodeStopRequest::new(TARGET_INSTANCE_ID),
            &caller_handle,
            LOCAL_CORE_NODE,
            CALLER_INSTANCE_ID,
            common::test_node_target(NODE_NAME),
            Duration::from_secs(5),
        )
        .await
        .expect("scoped node_stop request should complete");

        assert!(
            response.success,
            "scoped node_stop #{i} was answered by the foreign core node: {:?}",
            response.error_message
        );
    }

    assert_eq!(
        foreign_hits.load(Ordering::SeqCst),
        0,
        "the foreign core node's node_stop handler must never see a scoped request"
    );
}

/// An explicit `node_stop` of a node that has a LIVE exit watcher (started
/// through the real `node_run` path) must end with the instance fully removed -
/// gone from every lifecycle state, not lingering as a terminal `Failed`, and
/// the intentional kill must never be recorded as a crash. The stop path claims
/// the instance (`mark_stopping`) before signaling it, so the watcher that
/// observes the force-kill leaves removal to the stop path instead of relabeling
/// the kill. The other stop tests drive instances spawned without a watcher, so
/// this is the one that exercises the watcher-versus-stop interaction end to end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_stop_with_live_exit_watcher_removes_without_recording_a_crash() {
    const TARGET_NODE_NAME: &str = "watched_stoppable_node";
    const TARGET_NODE_TAG: &str = "v1";
    const TARGET_INSTANCE_ID: &str = "watched_stoppable_instance";

    let started = start_core_node_with_mock_messenger().await;

    let peppy_json5 = r#"{
            peppy_schema: "node/v1",
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
    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
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

    // Mocks satisfy the readiness gate so the real node_run path commits the
    // instance and spawns its exit watcher (the bare `sleep` cannot speak the
    // protocol itself).
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
    let instance_id = Name::new(TARGET_INSTANCE_ID).expect("valid instance id");

    // Bridge the cooperative shutdown to a real kill so the stop completes fast
    // instead of waiting out the whole force-kill deadline. Crucially the kill
    // lands strictly AFTER the stop path's `mark_stopping` claim, so the watcher
    // sees an intentional stop, not a crash.
    let _kill_on_shutdown =
        install_kill_on_shutdown_listener(&started, TARGET_NODE_NAME, &instance_id, pid).await;

    let response = poll_node_stop(
        &NodeStopRequest::new(TARGET_INSTANCE_ID),
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        common::core_node_target(&started.core_node_name),
        Duration::from_secs(30),
    )
    .await
    .expect("node_stop request should complete");
    assert!(
        response.success,
        "node_stop should succeed, got error: {:?}",
        response.error_message
    );

    poll_until(
        Duration::from_secs(5),
        "node process should be gone after stop",
        || (!is_process_running(pid)).then_some(()),
    )
    .await;

    // The instance is removed from EVERY state, not left as a terminal record.
    // (The Running-only `find_by_instance_id` would also return None for a
    // lingering `Failed`, so this must check across all states to be meaningful.)
    poll_until(
        Duration::from_secs(5),
        "instance should be fully removed from the stack after stop, not lingering as terminal",
        || {
            instance_state_in_any_state(&started.node_stack, &instance_id)
                .is_none()
                .then_some(())
        },
    )
    .await;

    // The claim made the watcher leave the kill alone, so no crash was recorded
    // for this instance. On correct code this holds regardless of watcher/stop
    // ordering; a regression that dropped the claim would let the watcher relabel
    // the force-kill as `failed` in the stack log.
    let log_content =
        std::fs::read_to_string(started.peppy_dirs.stack_log_path()).unwrap_or_default();
    assert!(
        !log_content
            .lines()
            .any(|line| line.contains(TARGET_INSTANCE_ID) && line.contains("failed")),
        "an explicit stop must never be recorded as a crash, got:
{}",
        log_content
    );
}
