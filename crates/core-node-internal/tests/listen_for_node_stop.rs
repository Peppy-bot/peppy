mod common;

use common::{
    AbortOnDrop, CALLER_INSTANCE_ID, GOAL_TIMEOUT, NodeRunTestTimeouts, RESULT_TIMEOUT,
    add_and_build_forking_node, answer_readiness, assert_failure, build_staged_node,
    create_test_node_with_name, error_reply, install_kill_on_shutdown_listener, is_process_running,
    poll_until, send_node_add_and_wait, send_node_add_then_build, send_node_run_and_wait,
    spawn_real_stuck_instance, start_core_node_with_mock_messenger,
    start_core_node_with_real_messenger, wait_for_forked_grandchildren,
    wait_until_grandchildren_exit, wait_until_process_exits, write_peppy_json5,
};
use config::failure_kind::PeppyFailureKind;
use config::runtime::Name;
use core_node::force_kill_deadline;
use core_node_api::ServiceId;
use core_node_api::encoding::{NodeStopRequest, NodeStopResponse};
use peppylib::core_node::transport::poll_node_stop;
use peppylib::messaging::{Failure, MessengerHandle, ServiceMessenger};
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
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add should complete");

    add_response.completed();
    assert!(node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG));
    build_staged_node(&started_core_node, TARGET_NODE_NAME, TARGET_NODE_TAG).await;

    // Drive the real start lifecycle so the entity tracks a live child
    // process (spawned from the node's `run_cmd = ["sleep", "10"]`). The stuck
    // variant installs no shutdown listener, so the listener below is the only
    // SHUTDOWN_SERVICE responder of the instance. With a second responder the
    // mock delivers the daemon's request to both, the daemon closes its reply
    // stream on the first answer, and the slower listener drops the request
    // without running its handler.
    let instance_id = Name::new(TARGET_INSTANCE_ID).expect("valid instance id");
    let running = spawn_real_stuck_instance(
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
    common::wait_until_service_reachable(
        &shutdown_handle,
        &started_core_node.core_node_name,
        TARGET_NODE_NAME,
        peppylib::messaging::SHUTDOWN_SERVICE,
        &started_core_node.core_node_name,
        TARGET_INSTANCE_ID,
        Duration::from_secs(5),
    )
    .await;

    // Spawn a task to SIGKILL the entity-tracked pid when shutdown is
    // received (simulating the target node's own exit path). It reports the
    // request before the kill, and the daemon reports a graceful stop only
    // once the process is gone, so the report is in place when the stop
    // returns.
    let (shutdown_asked_tx, mut shutdown_asked_rx) = tokio::sync::oneshot::channel();
    let kill_task = tokio::spawn(async move {
        shutdown_rx
            .await
            .expect("shutdown channel should not be dropped");
        let _ = shutdown_asked_tx.send(());
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

    assert!(
        shutdown_asked_rx.try_recv().is_ok(),
        "node_stop should send the shutdown request to the instance"
    );
    kill_task.await.expect("kill task should not panic");
}

/// A cooperative node that legitimately takes longer than the bare grace window
/// to disappear (a real one runs its hooks within `grace`, then a Python node
/// joins its asyncio loop and the interpreter finalizes) must still be reported
/// graceful, not force-killed: the daemon's force-kill deadline is
/// `force_kill_deadline(grace)` (grace + loop-join + finalize), not the bare
/// `grace`. We make the node exit at `grace + 2s` (past a bare `grace` deadline,
/// comfortably inside `force_kill_deadline(grace)`) and assert it is classified
/// graceful. With a bare `grace` deadline the process would still be alive when
/// the deadline passed, and would be SIGKILLed and reported force-killed.
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
    // tells a daemon that waits only the grace (and force-kills) from one that
    // waits grace + teardown margin (and reports a graceful stop).
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
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        None,
    )
    .await
    .expect("node_add should complete");
    add_response.completed();
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
    common::wait_until_service_reachable(
        &shutdown_handle,
        &started_core_node.core_node_name,
        TARGET_NODE_NAME,
        peppylib::messaging::SHUTDOWN_SERVICE,
        &started_core_node.core_node_name,
        TARGET_INSTANCE_ID,
        Duration::from_secs(5),
    )
    .await;

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
        &started_core_node.core_node_name,
        deadline + Duration::from_secs(5),
    )
    .await
    .expect("node_stop request should complete");

    assert!(
        !response.force_killed,
        "a node that exits cooperatively after the grace window but within the \
         force-kill deadline must be reported graceful, not force-killed"
    );
    assert!(
        !is_process_running(pid),
        "process {pid} should be gone after a graceful stop"
    );

    // Only the kill task ends the `sleep 60` before the force-kill deadline,
    // so a graceful stop means it has already killed the process.
    kill_task.await.expect("kill task should not panic");
}

/// `peppy node stop` of the core node's own instance is refused, and the
/// instance stays tracked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_stop_of_the_core_nodes_instance_is_refused() {
    let started = start_core_node_with_mock_messenger().await;
    let graph = started.node_stack.to_serialized_graph();
    let root_id = graph
        .nodes
        .iter()
        .find(|node| node.name == started.core_node_name && node.tag == started.core_node_tag)
        .and_then(|node| node.instances.first())
        .map(|instance| instance.instance_id.clone())
        .expect("the stack lists the core node's own instance");

    let error = poll_node_stop(
        &NodeStopRequest::new(&root_id),
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        common::core_node_target(&started.core_node_name),
        &started.core_node_name,
        Duration::from_secs(5),
    )
    .await
    .expect_err("the daemon refuses the stop");
    let failure = error_reply(&error);
    assert_failure(
        &failure,
        PeppyFailureKind::Rejected,
        "Cannot stop the core node",
    );
    assert_eq!(failure.message(), "Cannot stop the core node");
    assert!(
        started
            .node_stack
            .find_instance_any_state(&Name::new(&root_id).unwrap())
            .is_some(),
        "the core node's instance stays tracked"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_stop_fails_when_instance_id_not_found() {
    const MISSING_INSTANCE_ID: &str = "missing_instance";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let error = poll_node_stop(
        &NodeStopRequest::new(MISSING_INSTANCE_ID),
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        CALLER_INSTANCE_ID,
        common::core_node_target(&started_core_node.core_node_name),
        &started_core_node.core_node_name,
        Duration::from_secs(5),
    )
    .await
    .expect_err("node_stop should fail");

    let failure = error_reply(&error);
    assert_failure(
        &failure,
        PeppyFailureKind::Rejected,
        "not found in node stack",
    );
    assert_failure(&failure, PeppyFailureKind::Rejected, MISSING_INSTANCE_ID);
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

    let grandchildren = wait_for_forked_grandchildren(node_pid).await;
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
        &started.core_node_name,
        stop_timeout,
    )
    .await
    .expect("node_stop request should complete");

    // A stuck node is force-killed, and its stop is a success: the
    // `expect` above gives the error reply of a failed stop.
    // The user must be told this was a force-kill, not a graceful exit.
    assert!(
        response.force_killed,
        "a stuck node that ignored shutdown should be reported as force-killed"
    );

    // No orphans: the node and every grandchild must be gone. Poll rather than
    // assert synchronously; the reap is best-effort under a bounded timeout, so
    // success may be reported a beat before the kernel finishes the teardown.
    wait_until_process_exits(
        node_pid,
        Duration::from_secs(5),
        &format!("node process {node_pid} should be gone after node_stop"),
    )
    .await;
    wait_until_grandchildren_exit(&grandchildren, "node_stop").await;

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
/// `peppy-shared/peppylib-rs/tests/runner.rs` (which simulate the daemon's
/// shutdown send): here both halves are the production code.
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

    send_node_add_then_build(
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

    // The daemon assembles the runtime config, endpoint included, so the test
    // only says WHICH instance to start.
    let instance_plan = common::default_instance_plan(TARGET_INSTANCE_ID);

    let start_response = send_node_run_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        instance_plan,
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
    let pid = start_response
        .end
        .completed()
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
        &started.core_node_name,
        stop_timeout,
    )
    .await
    .expect("node_stop request should complete");

    // The whole point: a real NodeBuilder node must honor the cooperative
    // shutdown within the grace period and never be reported as force-killed.
    assert!(
        !response.force_killed,
        "a real NodeBuilder node should exit cooperatively, not be force-killed"
    );

    // The process must be gone. Poll rather than assert synchronously: the
    // reap is best-effort under a bounded timeout.
    wait_until_process_exits(
        pid,
        Duration::from_secs(5),
        &format!("node process {pid} should be gone after node_stop"),
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
/// `scope_core_node` it is given (here the caller's own core node), so the
/// foreign listener must never answer.
///
/// The foreign listener registers first, to bias every unscoped discovery
/// toward it.
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
            ServiceId::NodeStop.name(),
        )
        .await
        .expect("foreign node_stop listener should start");
        let foreign_hits = Arc::clone(&foreign_hits);
        AbortOnDrop(peppylib::runtime::spawn(async move {
            endpoint
                .handle_requests(move |_context| {
                    foreign_hits.fetch_add(1, Ordering::SeqCst);
                    async move {
                        Err(Failure::new(
                            PeppyFailureKind::Rejected,
                            "foreign core node must never answer a scoped node_stop",
                        ))
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
            ServiceId::NodeStop.name(),
        )
        .await
        .expect("local node_stop listener should start");
        AbortOnDrop(peppylib::runtime::spawn(async move {
            endpoint
                .handle_requests(|_context| async move {
                    Ok(NodeStopResponse::graceful()
                        .encode()
                        .expect("a node_stop response encodes"))
                })
                .await
        }))
    };

    // Both listeners must be reachable, or the foreign one could not win an
    // unscoped discovery and the test would pass vacuously. Reachability
    // probes are answered by the transport and never reach the handlers.
    let caller_handle = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    for (core_node, listener_instance_id) in [
        (LOCAL_CORE_NODE, LOCAL_LISTENER_INSTANCE_ID),
        (FOREIGN_CORE_NODE, FOREIGN_LISTENER_INSTANCE_ID),
    ] {
        common::wait_until_service_reachable(
            &caller_handle,
            LOCAL_CORE_NODE,
            NODE_NAME,
            ServiceId::NodeStop.name(),
            core_node,
            listener_instance_id,
            Duration::from_secs(5),
        )
        .await;
    }

    // Each iteration runs a fresh discover-then-pin sequence; without the
    // bound-core-node scope the foreign listener could win any of them.
    for i in 0..10 {
        poll_node_stop(
            &NodeStopRequest::new(TARGET_INSTANCE_ID),
            &caller_handle,
            LOCAL_CORE_NODE,
            CALLER_INSTANCE_ID,
            common::test_node_target(NODE_NAME),
            LOCAL_CORE_NODE,
            Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|error| {
            panic!("scoped node_stop #{i} was answered by the foreign core node: {error}")
        });
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

    send_node_add_then_build(
        &started.caller_handle,
        &started.core_node_name,
        source_dir.path(),
        Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .await
    .expect("node_add should succeed");

    // Mocks satisfy the readiness gate so the real node_run path commits the
    // instance and spawns its exit watcher (the bare `sleep` cannot speak the
    // protocol itself).
    let _responders = answer_readiness(&started, TARGET_NODE_NAME, TARGET_INSTANCE_ID).await;

    let instance_plan = common::default_instance_plan(TARGET_INSTANCE_ID);
    let start_response = send_node_run_and_wait(
        &started.caller_handle,
        &started.core_node_name,
        instance_plan,
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
    let pid = start_response
        .end
        .completed()
        .pid
        .expect("should have a PID on success");
    let instance_id = Name::new(TARGET_INSTANCE_ID).expect("valid instance id");

    // Bridge the cooperative shutdown to a real kill so the stop completes fast
    // instead of waiting out the whole force-kill deadline. Crucially the kill
    // lands strictly AFTER the stop path's `mark_stopping` claim, so the watcher
    // sees an intentional stop, not a crash.
    let _kill_on_shutdown =
        install_kill_on_shutdown_listener(&started, TARGET_NODE_NAME, &instance_id, pid).await;

    poll_node_stop(
        &NodeStopRequest::new(TARGET_INSTANCE_ID),
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        common::core_node_target(&started.core_node_name),
        &started.core_node_name,
        Duration::from_secs(30),
    )
    .await
    .expect("node_stop request should complete");

    wait_until_process_exits(
        pid,
        Duration::from_secs(5),
        "node process should be gone after stop",
    )
    .await;

    // The instance is removed from EVERY state, not left as a terminal record.
    // (The Running-only `find_by_instance_id` would also return None for a
    // lingering `Failed`, so this must check across all states to be meaningful.)
    poll_until(
        Duration::from_secs(5),
        "instance should be fully removed from the stack after stop, not lingering as terminal",
        || {
            started
                .node_stack
                .instance_state(instance_id.as_str())
                .is_none()
                .then_some(())
        },
    )
    .await;

    // The claim made the watcher leave the kill alone, so no crash was recorded
    // for this instance. On correct code this holds regardless of watcher/stop
    // ordering; a regression that dropped the claim would let the watcher relabel
    // the force-kill as `failed` in the stack log.
    let log_content = common::stack_log(&started);
    assert!(
        !log_content
            .lines()
            .any(|line| line.contains(TARGET_INSTANCE_ID) && line.contains("failed")),
        "an explicit stop must never be recorded as a crash, got:
{}",
        log_content
    );
}
