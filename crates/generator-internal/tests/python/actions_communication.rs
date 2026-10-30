use crate::helpers::{
    CapturedChild, ConsumerExposerOutput, DEFAULT_WAIT_TIMEOUT, EXPOSED_ACTION_EXAMPLE,
    JOINTS_CONTRACT_ACTION, JOINTS_CONTRACT_NAME, JOINTS_REFINEMENT, PYTHON_STUB_EXECUTION,
    SHUTDOWN_SENDER_INSTANCE_ID, STUB_PYTHON_NODE_CONFIG, TEST_CORE_NODE,
    assert_exited_successfully, bind_slot, consumed_move_arm_action_messages,
    contract_consumer_stub_node_config, contract_dep, contract_dep_with_kinds, failure_kinds,
    generate_node_project, init_python_project_venv, init_python_user_node, native_dep,
    python_consumer_stub_node_config, save_runtime_config, shut_down_test_node, spawn_python_run,
    test_contract_target, test_node_runtime_config, test_wait_context, try_send_shutdown,
    wait_for_action_reachable_via_target_or_exit, wait_for_action_service_reachable_or_exit,
    wait_for_child, wait_for_health_service_reachable_or_exit, wait_for_service_reachable_or_exit,
};
use config::consts::RUNTIME_CONFIG_VAR_NAME;
use config::failure_kind::MemberSection;
use config::node::{ConsumedAction, NativeExposedAction, NativeExposedService};
use generator::{ConsumedActionMessage, ContractOrigin, LanguageGenerator};
use std::{fs, time::Duration};
use tempfile::TempDir;

// --- Common test constants
const CONSUMER_NODE_NAME: &str = "consumer_node";
const CONSUMER_INSTANCE_ID: &str = "consumer_instance";
const EXPOSER_INSTANCE_ID: &str = "exposer_instance";
const BRAIN_NODE_NAME: &str = "brain";
const ACTION_FLOW_DONE_SERVICE: &str = "move_arm_flow_done";
const ACTION_CANCEL_FLOW_DONE_SERVICE: &str = "move_arm_cancel_flow_done";
const ACTION_IN_HANDLER_FLOW_DONE_SERVICE: &str = "move_arm_in_handler_flow_done";
const ACTION_CANCEL_ACCEPT_FLOW_DONE_SERVICE: &str = "move_arm_cancel_accept_flow_done";
const ACTION_CANCEL_REJECT_FLOW_DONE_SERVICE: &str = "move_arm_cancel_reject_flow_done";
const ACTION_DRAIN_LOOP_FLOW_DONE_SERVICE: &str = "move_arm_drain_loop_flow_done";

const CONSUMED_ACTION_EXAMPLE: &str = r#"
{
  link_id: "brain",
  name: "move_arm",
}
"#;

/// Runs one native move_arm action flow. The consumer fires goals at the
/// exposer of [`EXPOSED_ACTION_EXAMPLE`] through its `brain` slot, bound to
/// that exposer, and exposes `flow_done_service`, which its script answers
/// once it has run the flow. The exposer starts first, and both nodes are
/// shut down once the consumer's flow-done service is reachable. Returns
/// what the two nodes printed; both exited successfully.
async fn run_move_arm_flow(
    topology: crate::helpers::LocalNodesTopology,
    flow_done_service: &str,
    consumer_main: &str,
    exposer_main: &str,
) -> ConsumerExposerOutput {
    let instance = pmi::ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    // --- Consumer (client) project
    let temp_dir_consumer = TempDir::new_in(config_test_support::test_tmp_root())
        .expect("failed to create temp dir for consumer project");
    let consumed_action: ConsumedAction = serde_json5::from_str(CONSUMED_ACTION_EXAMPLE).unwrap();
    let action_messages = consumed_move_arm_action_messages();
    let flow_done: NativeExposedService = serde_json5::from_str(&format!(
        r#"{{ name: "{flow_done_service}", description: "A test member." }}"#
    ))
    .unwrap();
    let user_node_consumer = generate_node_project::<generator::PythonGenerator>(
        &temp_dir_consumer,
        &python_consumer_stub_node_config("brain", "v1", "brain"),
        |generator| {
            generator
                .add_consumed_action(
                    &consumed_action,
                    &action_messages,
                    &native_dep("brain", "v1", "brain"),
                )
                .unwrap();
            generator.add_exposed_service(&flow_done, None).unwrap();
        },
    );
    let consumer_runtime_config = test_node_runtime_config(
        &router_host,
        router_port,
        CONSUMER_NODE_NAME,
        CONSUMER_INSTANCE_ID,
    );
    let consumer_runtime_config = bind_slot(
        consumer_runtime_config,
        "brain",
        TEST_CORE_NODE,
        EXPOSER_INSTANCE_ID,
    );
    let consumer_runtime_config = crate::helpers::apply_topology(consumer_runtime_config, topology);
    let consumer_runtime_config_str =
        save_runtime_config(&consumer_runtime_config, temp_dir_consumer.path());
    init_python_user_node(&user_node_consumer);
    fs::write(user_node_consumer.join("main.py"), consumer_main)
        .expect("failed to write consumer main.py");

    // --- Exposer (server) project
    let temp_dir_exposer = TempDir::new_in(config_test_support::test_tmp_root())
        .expect("failed to create temp dir for exposer project");
    let exposed_action: NativeExposedAction =
        serde_json5::from_str(EXPOSED_ACTION_EXAMPLE).unwrap();
    let user_node_exposer = generate_node_project::<generator::PythonGenerator>(
        &temp_dir_exposer,
        STUB_PYTHON_NODE_CONFIG,
        |generator| {
            generator.add_exposed_action(&exposed_action, None).unwrap();
        },
    );
    // The exposer runs as the `brain` node the consumer's slot depends on.
    let exposer_runtime_config = test_node_runtime_config(
        &router_host,
        router_port,
        BRAIN_NODE_NAME,
        EXPOSER_INSTANCE_ID,
    );
    let exposer_runtime_config = crate::helpers::apply_topology(exposer_runtime_config, topology);
    let exposer_runtime_config_str =
        save_runtime_config(&exposer_runtime_config, temp_dir_exposer.path());
    init_python_user_node(&user_node_exposer);
    fs::write(user_node_exposer.join("main.py"), exposer_main)
        .expect("failed to write exposer main.py");

    init_python_project_venv(&user_node_consumer);
    init_python_project_venv(&user_node_exposer);

    let messenger = peppylib::MessengerHandle::connect(&router_host, router_port)
        .await
        .expect("failed to create messenger for test control");
    let ctx = test_wait_context(&messenger);

    // Spawn exposer first so it's ready to handle requests
    let mut exposer_child = spawn_python_run(
        &user_node_exposer,
        &[(RUNTIME_CONFIG_VAR_NAME, &exposer_runtime_config_str)],
    );
    wait_for_action_service_reachable_or_exit(
        &ctx,
        BRAIN_NODE_NAME,
        "move_arm",
        None,
        &mut exposer_child,
        &user_node_exposer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;

    let mut consumer_child = spawn_python_run(
        &user_node_consumer,
        &[(RUNTIME_CONFIG_VAR_NAME, &consumer_runtime_config_str)],
    );
    wait_for_health_service_reachable_or_exit(
        &ctx,
        CONSUMER_NODE_NAME,
        CONSUMER_INSTANCE_ID,
        &mut consumer_child,
        &user_node_consumer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;
    wait_for_health_service_reachable_or_exit(
        &ctx,
        BRAIN_NODE_NAME,
        EXPOSER_INSTANCE_ID,
        &mut exposer_child,
        &user_node_exposer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;
    wait_for_service_reachable_or_exit(
        &ctx,
        CONSUMER_NODE_NAME,
        flow_done_service,
        Some(CONSUMER_INSTANCE_ID),
        &mut consumer_child,
        &user_node_consumer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;

    shut_down_test_node(&messenger, CONSUMER_NODE_NAME, CONSUMER_INSTANCE_ID).await;
    shut_down_test_node(&messenger, BRAIN_NODE_NAME, EXPOSER_INSTANCE_ID).await;

    let consumer_output = wait_for_child(
        &mut consumer_child,
        Some(Duration::from_secs(10)),
        &user_node_consumer,
    );
    let exposer_output = wait_for_child(
        &mut exposer_child,
        Some(Duration::from_secs(10)),
        &user_node_exposer,
    );
    let (consumer_stdout, consumer_stderr) =
        assert_exited_successfully("consumer process", &consumer_output);
    let (exposer_stdout, exposer_stderr) =
        assert_exited_successfully("exposer process", &exposer_output);
    ConsumerExposerOutput {
        consumer_stdout,
        consumer_stderr,
        exposer_stdout,
        exposer_stderr,
    }
}

#[rstest::rstest]
#[case::peer(crate::helpers::LocalNodesTopology::Peer)]
#[case::router(crate::helpers::LocalNodesTopology::Router)]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn actions_communication(#[case] topology: crate::helpers::LocalNodesTopology) {
    let consumer_main = r#"
import asyncio
from peppygen import NodeBuilder
from peppygen.exposed_services import move_arm_flow_done
from peppygen.consumed_actions.brain import move_arm as brain_move_arm

async def run_consumer(node_runner):
    arm = brain_move_arm.bound_producer(node_runner)

    # A goal for the reserved arm is refused through the framework
    # admission ack, with a kind of peppy (a native action declares no kind
    # of its own); no declared response payload rides along.
    reserved = brain_move_arm.GoalRequest(arm_id=99, desired_position=[0, 0, 0])
    refused = await brain_move_arm.ActionHandle.fire_goal(
        node_runner, arm, reserved, 5.0
    )
    assert refused.refusal.kind is brain_move_arm.FailureKind.PRECONDITION_FAILED
    print(
        f"refused goal accepted={refused.accepted} refusal={refused.refusal} data_present={refused.data is not None}",
        flush=True,
    )

    request = brain_move_arm.GoalRequest(arm_id=7, desired_position=[10, 20, 30])
    goal = await brain_move_arm.ActionHandle.fire_goal(
        node_runner, arm, request, 5.0
    )
    print(f"goal accepted={goal.accepted} data_accepted={goal.data.accepted}", flush=True)

    feedback = await goal.on_next_feedback_message()
    assert feedback.new_position == [7, 31, 43], "unexpected feedback message"
    print(f"feedback message received new_position={feedback.new_position}", flush=True)

    result = await goal.get_result(5.0)
    assert result.status == brain_move_arm.ResultStatus.COMPLETED, f"unexpected status {result.status}"
    print(
        f"result success={result.data.success} error={result.data.error_msg} final_position={result.data.final_position}",
        flush=True,
    )
    await move_arm_flow_done.handle_next_request(node_runner, lambda _request: None)

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    return [asyncio.create_task(run_consumer(node_runner))]

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;
    let exposer_main = r#"
import asyncio
from peppygen import NodeBuilder
from peppygen.exposed_actions import move_arm

async def run_exposer(node_runner):
    action = await move_arm.ActionHandle.expose(node_runner)

    def goal_handler(request):
        print(
            f"server received goal arm_id={request.data.arm_id} desired={request.data.desired_position}",
            flush=True,
        )
        if request.data.arm_id == 99:
            return move_arm.GoalDecision.refuse(
                move_arm.FailureKind.PRECONDITION_FAILED, "arm 99 is reserved"
            )
        return move_arm.GoalDecision.accept(move_arm.GoalResponse(True))

    while True:
        ctx = await action.handle_goal_next_request(goal_handler)
        if ctx is None:
            break

        feedback_message = [7, 31, 43]
        await ctx.publish_feedback(feedback_message)
        print(f"server emitted feedback message {feedback_message}", flush=True)

        final_position = [98, 4, 26]
        await ctx.complete(True, None, final_position)
        print(f"server handled result request. Final position sent: {final_position}", flush=True)

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    return [asyncio.create_task(run_exposer(node_runner))]

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;
    let ConsumerExposerOutput {
        consumer_stdout,
        consumer_stderr,
        exposer_stdout,
        exposer_stderr,
    } = run_move_arm_flow(
        topology,
        ACTION_FLOW_DONE_SERVICE,
        consumer_main,
        exposer_main,
    )
    .await;
    assert!(
        consumer_stdout
            .contains("refused goal accepted=False refusal=precondition_failed: arm 99 is reserved data_present=False")
            && consumer_stdout.contains("goal accepted=True data_accepted=True")
            && consumer_stdout.contains("feedback message received new_position=[7, 31, 43]")
            && consumer_stdout
                .contains("result success=True error=None final_position=[98, 4, 26]"),
        "consumer did not complete the action flow.\nstdout:\n{}\nstderr:\n{}",
        consumer_stdout,
        consumer_stderr
    );

    assert!(
        exposer_stdout.contains("server received goal arm_id=7 desired=[10, 20, 30]")
            && exposer_stdout.contains("server emitted feedback message [7, 31, 43]")
            && exposer_stdout
                .contains("server handled result request. Final position sent: [98, 4, 26]"),
        "exposer did not process the action endpoints as expected.\nstdout:\n{}\nstderr:\n{}",
        exposer_stdout,
        exposer_stderr
    );
}

#[rstest::rstest]
#[case::peer(crate::helpers::LocalNodesTopology::Peer)]
#[case::router(crate::helpers::LocalNodesTopology::Router)]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn actions_communication_cancel_goal(#[case] topology: crate::helpers::LocalNodesTopology) {
    let consumer_main = r#"
import asyncio
from peppygen import NodeBuilder
from peppygen.exposed_services import move_arm_cancel_flow_done
from peppygen.consumed_actions.brain import move_arm as brain_move_arm

async def run_consumer(node_runner):
    request = brain_move_arm.GoalRequest(arm_id=7, desired_position=[10, 20, 30])
    arm = brain_move_arm.bound_producer(node_runner)
    goal = await brain_move_arm.ActionHandle.fire_goal(
        node_runner, arm, request, 5.0
    )
    print(f"goal accepted={goal.data.accepted}", flush=True)

    cancel_response = await goal.cancel_goal(5.0)
    accepted = cancel_response.state == brain_move_arm.CancelState.SIGNALLED
    print(
        f"cancel accepted={accepted} error=<none>",
        flush=True,
    )
    await move_arm_cancel_flow_done.handle_next_request(node_runner, lambda _request: None)

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    return [asyncio.create_task(run_consumer(node_runner))]

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;
    let exposer_main = r#"
import asyncio
from peppygen import NodeBuilder
from peppygen.exposed_actions import move_arm

async def run_exposer(node_runner):
    action = await move_arm.ActionHandle.expose(node_runner)

    def goal_handler(request):
        print(
            f"server received goal arm_id={request.data.arm_id} desired={request.data.desired_position}",
            flush=True,
        )
        return move_arm.GoalDecision.accept(move_arm.GoalResponse(True))

    while True:
        ctx = await action.handle_goal_next_request(goal_handler)
        if ctx is None:
            break
        # Wait for a cancel for this goal, then report the cancelled result.
        await ctx.cancel_signal()
        print("server observed cancel for goal", flush=True)
        await ctx.complete_cancelled("goal cancelled by server", False, "goal cancelled by server", [0, 0, 0])

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    return [asyncio.create_task(run_exposer(node_runner))]

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;
    let ConsumerExposerOutput {
        consumer_stdout,
        consumer_stderr,
        exposer_stdout,
        exposer_stderr,
    } = run_move_arm_flow(
        topology,
        ACTION_CANCEL_FLOW_DONE_SERVICE,
        consumer_main,
        exposer_main,
    )
    .await;
    assert!(
        consumer_stdout.contains("goal accepted=True")
            && consumer_stdout.contains("cancel accepted=True error=<none>"),
        "consumer did not complete the cancel flow.\nstdout:\n{}\nstderr:\n{}",
        consumer_stdout,
        consumer_stderr
    );

    assert!(
        exposer_stdout.contains("server received goal arm_id=7 desired=[10, 20, 30]")
            && exposer_stdout.contains("server observed cancel for goal"),
        "exposer did not handle cancel endpoint as expected.\nstdout:\n{}\nstderr:\n{}",
        exposer_stdout,
        exposer_stderr
    );
}

/// Verifies that an **async goal decider** works end-to-end through the
/// Python codegen: `handle_goal_next_request` awaits the handler when it
/// returns an awaitable, so a server can `await` inside its accept or refuse
/// decision. After accepting, the worker publishes feedback through the
/// `GoalContext` and completes the goal.
#[rstest::rstest]
#[case::peer(crate::helpers::LocalNodesTopology::Peer)]
#[case::router(crate::helpers::LocalNodesTopology::Router)]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn actions_communication_async_goal_decider(
    #[case] topology: crate::helpers::LocalNodesTopology,
) {
    let consumer_main = r#"
import asyncio
from peppygen import NodeBuilder
from peppygen.exposed_services import move_arm_in_handler_flow_done
from peppygen.consumed_actions.brain import move_arm as brain_move_arm

async def run_consumer(node_runner):
    request = brain_move_arm.GoalRequest(arm_id=7, desired_position=[10, 20, 30])
    arm = brain_move_arm.bound_producer(node_runner)
    goal = await brain_move_arm.ActionHandle.fire_goal(
        node_runner, arm, request, 5.0
    )
    print(f"goal accepted={goal.data.accepted}", flush=True)

    feedback = await goal.on_next_feedback_message()
    print(f"feedback message received new_position={feedback.new_position}", flush=True)
    assert feedback.new_position == [7, 100, 200], "unexpected feedback"

    result = await goal.get_result(5.0)
    assert result.status == brain_move_arm.ResultStatus.COMPLETED, f"unexpected status {result.status}"
    print(
        f"result success={result.data.success} final_position={result.data.final_position}",
        flush=True,
    )
    await move_arm_in_handler_flow_done.handle_next_request(node_runner, lambda _request: None)

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    return [asyncio.create_task(run_consumer(node_runner))]

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;
    // The goal decider is `async` (codegen awaits it); after accepting, the
    // worker publishes feedback through the `GoalContext` and completes. See
    // the test docstring above.
    let exposer_main = r#"
import asyncio
from peppygen import NodeBuilder
from peppygen.exposed_actions import move_arm

async def run_exposer(node_runner):
    action = await move_arm.ActionHandle.expose(node_runner)

    # The goal decider may be a coroutine: the codegen awaits it when it
    # returns an awaitable. This verifies async deciders work end-to-end.
    async def goal_handler(request):
        print(
            f"server received goal arm_id={request.data.arm_id} desired={request.data.desired_position}",
            flush=True,
        )
        return move_arm.GoalDecision.accept(move_arm.GoalResponse(True))

    while True:
        ctx = await action.handle_goal_next_request(goal_handler)
        if ctx is None:
            break
        print("server accepted goal via async decider", flush=True)

        await ctx.publish_feedback([ctx.request().data.arm_id, 100, 200])
        print("server emitted feedback", flush=True)

        final_position = [42, 42, 42]
        await ctx.complete(True, None, final_position)
        print(f"server handled result request final_position={final_position}", flush=True)

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    return [asyncio.create_task(run_exposer(node_runner))]

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;
    let ConsumerExposerOutput {
        consumer_stdout,
        consumer_stderr,
        exposer_stdout,
        exposer_stderr,
    } = run_move_arm_flow(
        topology,
        ACTION_IN_HANDLER_FLOW_DONE_SERVICE,
        consumer_main,
        exposer_main,
    )
    .await;
    assert!(
        consumer_stdout.contains("goal accepted=True")
            && consumer_stdout.contains("feedback message received new_position=[7, 100, 200]")
            && consumer_stdout.contains("result success=True final_position=[42, 42, 42]"),
        "consumer did not complete the in-handler feedback flow.\nstdout:\n{}\nstderr:\n{}",
        consumer_stdout,
        consumer_stderr
    );

    assert!(
        exposer_stdout.contains("server accepted goal via async decider")
            && exposer_stdout.contains("server emitted feedback")
            && exposer_stdout.contains("server handled result request"),
        "exposer did not exercise the async-decider path.\nstdout:\n{}\nstderr:\n{}",
        exposer_stdout,
        exposer_stderr
    );
}

/// Verifies the cancel-honored side of the action lifecycle contract: when
/// the server's worker observes `ctx.cancel_signal()` and reacts with
/// `ctx.complete_cancelled(...)`, completing the goal publishes an
/// end-of-stream sentinel on the per-goal feedback publisher so the client
/// knows no further feedback will arrive.
///
/// End-to-end flow exercised here:
///   1. Client `fire_goal`, server accepts.
///   2. Server emits one warmup feedback message; client receives it.
///   3. Client calls `cancel_goal`; the framework auto-acks `accepted=True`
///      and the worker's `cancel_signal()` resolves.
///   4. The worker reacts with `complete_cancelled`, closing this goal's
///      feedback stream.
///   5. The client's next `await goal.on_next_feedback_message()` raises
///      (instead of blocking forever waiting for feedback that will never
///      come). That raise is what this test asserts.
///
/// The ignore-cancel branch is covered by
/// `actions_communication_cancel_reject_keeps_feedback_open`.
#[rstest::rstest]
#[case::peer(crate::helpers::LocalNodesTopology::Peer)]
#[case::router(crate::helpers::LocalNodesTopology::Router)]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn actions_communication_cancel_accept_closes_feedback_stream(
    #[case] topology: crate::helpers::LocalNodesTopology,
) {
    let consumer_main = r#"
import asyncio
from peppygen import NodeBuilder
from peppygen.exposed_services import move_arm_cancel_accept_flow_done
from peppygen.consumed_actions.brain import move_arm as brain_move_arm

async def run_consumer(node_runner):
    request = brain_move_arm.GoalRequest(arm_id=7, desired_position=[10, 20, 30])
    arm = brain_move_arm.bound_producer(node_runner)
    goal = await brain_move_arm.ActionHandle.fire_goal(
        node_runner, arm, request, 5.0
    )
    print(f"goal accepted={goal.data.accepted}", flush=True)

    warmup = await goal.on_next_feedback_message()
    print(f"warmup feedback new_position={warmup.new_position}", flush=True)

    cancel_response = await goal.cancel_goal(5.0)
    accepted = cancel_response.state == brain_move_arm.CancelState.SIGNALLED
    print(f"cancel accepted={accepted}", flush=True)

    # Cancel was accepted: codegen publishes end-of-stream sentinel.
    try:
        msg = await goal.on_next_feedback_message()
        print(f"UNEXPECTED feedback after cancel-accept new_position={msg.new_position}", flush=True)
    except Exception as e:
        print(f"feedback closed after cancel-accept err={type(e).__name__}", flush=True)

    await move_arm_cancel_accept_flow_done.handle_next_request(node_runner, lambda _request: None)

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    return [asyncio.create_task(run_consumer(node_runner))]

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;
    let exposer_main = r#"
import asyncio
from peppygen import NodeBuilder
from peppygen.exposed_actions import move_arm

async def run_exposer(node_runner):
    action = await move_arm.ActionHandle.expose(node_runner)

    def goal_handler(_request):
        return move_arm.GoalDecision.accept(move_arm.GoalResponse(True))

    while True:
        ctx = await action.handle_goal_next_request(goal_handler)
        if ctx is None:
            break
        print("server accepted goal", flush=True)

        await ctx.publish_feedback([1, 2, 3])
        print("server emitted warmup feedback", flush=True)

        # Honor the cancel: completing-cancelled closes the feedback stream.
        await ctx.cancel_signal()
        await ctx.complete_cancelled("cancelled", False, "cancelled", [0, 0, 0])
        print("server observed cancel: completing cancelled closes the feedback stream", flush=True)

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    return [asyncio.create_task(run_exposer(node_runner))]

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;
    let ConsumerExposerOutput {
        consumer_stdout,
        exposer_stdout,
        ..
    } = run_move_arm_flow(
        topology,
        ACTION_CANCEL_ACCEPT_FLOW_DONE_SERVICE,
        consumer_main,
        exposer_main,
    )
    .await;
    assert!(
        consumer_stdout.contains("warmup feedback new_position=[1, 2, 3]"),
        "consumer did not receive warmup feedback before cancel.\nstdout:\n{}",
        consumer_stdout
    );
    assert!(
        consumer_stdout.contains("cancel accepted=True"),
        "consumer did not receive accepted cancel response.\nstdout:\n{}",
        consumer_stdout
    );
    assert!(
        consumer_stdout.contains("feedback closed after cancel-accept"),
        "consumer did not receive close signal after cancel-accept.\nstdout:\n{}",
        consumer_stdout
    );
    assert!(
        !consumer_stdout.contains("UNEXPECTED feedback after cancel-accept"),
        "consumer received unexpected feedback after cancel-accept.\nstdout:\n{}",
        consumer_stdout
    );

    assert!(
        exposer_stdout.contains("server observed cancel"),
        "exposer did not exercise the cancel-observed path.\nstdout:\n{}",
        exposer_stdout
    );
}

/// Verifies the cancel-ignored side of the action lifecycle contract: a
/// worker is free to observe `ctx.cancel_signal()` and keep going. Ignoring
/// the cancel does NOT close the feedback stream; the goal stays alive,
/// feedback keeps flowing, and the stream is closed only when the worker
/// finally calls `ctx.complete(...)` as part of normal goal completion.
///
/// End-to-end flow exercised here:
///   1. Client `fire_goal`, server accepts.
///   2. Server emits pre-cancel feedback; client receives it.
///   3. Client calls `cancel_goal`; the framework auto-acks `accepted=True`
///      and the worker's `cancel_signal()` resolves.
///   4. The worker chooses to keep going (does not complete), so the feedback
///      stream stays open.
///   5. Server emits post-cancel feedback; the client still receives it.
///      This is what proves step 4: ignoring the cancel did not close the stream.
///   6. The worker calls `ctx.complete(...)`; this is the step that
///      publishes the end-of-stream sentinel, as part of normal goal
///      completion.
///   7. The client's next `await goal.on_next_feedback_message()` raises,
///      confirming the stream is closed by completion (not by the earlier
///      cancel).
///
/// The honor-cancel branch is covered by
/// `actions_communication_cancel_accept_closes_feedback_stream`.
#[rstest::rstest]
#[case::peer(crate::helpers::LocalNodesTopology::Peer)]
#[case::router(crate::helpers::LocalNodesTopology::Router)]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn actions_communication_cancel_reject_keeps_feedback_open(
    #[case] topology: crate::helpers::LocalNodesTopology,
) {
    let consumer_main = r#"
import asyncio
from peppygen import NodeBuilder
from peppygen.exposed_services import move_arm_cancel_reject_flow_done
from peppygen.consumed_actions.brain import move_arm as brain_move_arm

async def run_consumer(node_runner):
    request = brain_move_arm.GoalRequest(arm_id=7, desired_position=[10, 20, 30])
    arm = brain_move_arm.bound_producer(node_runner)
    goal = await brain_move_arm.ActionHandle.fire_goal(
        node_runner, arm, request, 5.0
    )
    print(f"goal accepted={goal.data.accepted}", flush=True)

    pre_cancel = await goal.on_next_feedback_message()
    print(f"pre_cancel feedback new_position={pre_cancel.new_position}", flush=True)

    cancel_response = await goal.cancel_goal(5.0)
    accepted = cancel_response.state == brain_move_arm.CancelState.SIGNALLED
    print(
        f"cancel accepted={accepted} error=<none>",
        flush=True,
    )

    # CRITICAL: feedback after cancel-reject must still arrive.
    post_cancel = await goal.on_next_feedback_message()
    print(f"post_cancel feedback new_position={post_cancel.new_position}", flush=True)

    result = await goal.get_result(5.0)
    assert result.status == brain_move_arm.ResultStatus.COMPLETED, f"unexpected status {result.status}"
    print(f"result success={result.data.success}", flush=True)

    try:
        msg = await goal.on_next_feedback_message()
        print(f"UNEXPECTED feedback after result new_position={msg.new_position}", flush=True)
    except Exception as e:
        print(f"feedback closed after result err={type(e).__name__}", flush=True)

    await move_arm_cancel_reject_flow_done.handle_next_request(node_runner, lambda _request: None)

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    return [asyncio.create_task(run_consumer(node_runner))]

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;
    let exposer_main = r#"
import asyncio
from peppygen import NodeBuilder
from peppygen.exposed_actions import move_arm

async def run_exposer(node_runner):
    action = await move_arm.ActionHandle.expose(node_runner)

    def goal_handler(_request):
        return move_arm.GoalDecision.accept(move_arm.GoalResponse(True))

    while True:
        ctx = await action.handle_goal_next_request(goal_handler)
        if ctx is None:
            break

        await ctx.publish_feedback([1, 1, 1])
        print("server emitted pre-cancel feedback", flush=True)

        # Observe the cancel but choose to keep going (ignore it). Feedback
        # must keep flowing since the goal hasn't completed.
        await ctx.cancel_signal()
        print("server observed cancel but keeps going", flush=True)

        await ctx.publish_feedback([2, 2, 2])
        print("server emitted post-cancel feedback", flush=True)

        await ctx.complete(True, None, [9, 9, 9])
        print("server handled result request", flush=True)

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    return [asyncio.create_task(run_exposer(node_runner))]

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;
    let ConsumerExposerOutput {
        consumer_stdout,
        exposer_stdout,
        ..
    } = run_move_arm_flow(
        topology,
        ACTION_CANCEL_REJECT_FLOW_DONE_SERVICE,
        consumer_main,
        exposer_main,
    )
    .await;
    assert!(
        consumer_stdout.contains("pre_cancel feedback new_position=[1, 1, 1]"),
        "consumer did not receive pre-cancel feedback.\nstdout:\n{}",
        consumer_stdout
    );
    assert!(
        consumer_stdout.contains("cancel accepted=True error=<none>"),
        "consumer did not see the auto-acked cancel.\nstdout:\n{}",
        consumer_stdout
    );
    assert!(
        consumer_stdout.contains("post_cancel feedback new_position=[2, 2, 2]"),
        "consumer did not receive post-cancel feedback: a worker that ignores the cancel keeps the stream open.\nstdout:\n{}",
        consumer_stdout
    );
    assert!(
        consumer_stdout.contains("result success=True"),
        "consumer did not receive result.\nstdout:\n{}",
        consumer_stdout
    );
    assert!(
        consumer_stdout.contains("feedback closed after result"),
        "consumer did not receive close signal after result.\nstdout:\n{}",
        consumer_stdout
    );

    assert!(
        exposer_stdout.contains("server emitted pre-cancel feedback")
            && exposer_stdout.contains("server observed cancel but keeps going")
            && exposer_stdout.contains("server emitted post-cancel feedback")
            && exposer_stdout.contains("server handled result request"),
        "exposer did not exercise the cancel-ignored path.\nstdout:\n{}",
        exposer_stdout
    );
}

/// Verifies the goal-completion side of the action lifecycle contract: a
/// client can use a drain-loop pattern (`while True: await
/// on_next_feedback_message()`) to consume every feedback message and
/// reliably exit once the goal is complete. This works because
/// `GoalContext.complete()` publishes an end-of-stream sentinel (an empty
/// payload on the per-goal feedback publisher) before delivering the
/// result. Without that sentinel the loop would hang forever, because the
/// underlying mpsc receiver never surfaces an end-of-stream condition on
/// its own.
///
/// End-to-end flow exercised here:
///   1. Client `fire_goal`, server accepts.
///   2. Server emits 3 feedback messages.
///   3. Client's drain-loop receives all 3 in order.
///   4. Server calls `ctx.complete(...)`, which publishes the end-of-stream
///      sentinel on the per-goal feedback publisher (closing the feedback
///      stream) and then delivers the result.
///   5. Client's next `on_next_feedback_message()` raises (sentinel
///      observed). The drain-loop catches the exception and exits.
///   6. Client calls `get_result` and receives the final response.
///
/// Rust parity is `actions_communication_drain_loop_until_end_signal` in
/// the rust/ test module.
#[rstest::rstest]
#[case::peer(crate::helpers::LocalNodesTopology::Peer)]
#[case::router(crate::helpers::LocalNodesTopology::Router)]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn actions_communication_drain_loop_until_end_signal(
    #[case] topology: crate::helpers::LocalNodesTopology,
) {
    let consumer_main = r#"
import asyncio
from peppygen import NodeBuilder
from peppygen.exposed_services import move_arm_drain_loop_flow_done
from peppygen.consumed_actions.brain import move_arm as brain_move_arm

async def run_consumer(node_runner):
    request = brain_move_arm.GoalRequest(arm_id=7, desired_position=[10, 20, 30])
    arm = brain_move_arm.bound_producer(node_runner)
    goal = await brain_move_arm.ActionHandle.fire_goal(
        node_runner, arm, request, 5.0
    )
    print(f"goal accepted={goal.data.accepted}", flush=True)

    # Drain-loop pattern: keep reading feedback until the server signals
    # end-of-stream. Without the empty-payload sentinel this loop would hang
    # forever because the underlying mpsc receiver never returns None.
    feedback_count = 0
    while True:
        try:
            feedback = await goal.on_next_feedback_message()
            feedback_count += 1
            print(f"feedback #{feedback_count} new_position={feedback.new_position}", flush=True)
        except Exception:
            print(f"feedback channel closed after {feedback_count} messages", flush=True)
            break

    result = await goal.get_result(5.0)
    assert result.status == brain_move_arm.ResultStatus.COMPLETED, f"unexpected status {result.status}"
    print(
        f"result success={result.data.success} final_position={result.data.final_position}",
        flush=True,
    )

    await move_arm_drain_loop_flow_done.handle_next_request(node_runner, lambda _request: None)

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    return [asyncio.create_task(run_consumer(node_runner))]

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;
    let exposer_main = r#"
import asyncio
from peppygen import NodeBuilder
from peppygen.exposed_actions import move_arm

async def run_exposer(node_runner):
    action = await move_arm.ActionHandle.expose(node_runner)

    def goal_handler(_request):
        return move_arm.GoalDecision.accept(move_arm.GoalResponse(True))

    while True:
        ctx = await action.handle_goal_next_request(goal_handler)
        if ctx is None:
            break
        print("server accepted goal", flush=True)

        for i in range(3):
            pos = [i, i + 1, i + 2]
            await ctx.publish_feedback(pos)
            print(f"server emitted feedback #{i + 1} position={pos}", flush=True)

        final_position = [99, 99, 99]
        # complete publishes the end-of-stream sentinel, then delivers the result.
        await ctx.complete(True, None, final_position)
        print(f"server handled result request final_position={final_position}", flush=True)

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    return [asyncio.create_task(run_exposer(node_runner))]

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;
    let ConsumerExposerOutput {
        consumer_stdout,
        exposer_stdout,
        ..
    } = run_move_arm_flow(
        topology,
        ACTION_DRAIN_LOOP_FLOW_DONE_SERVICE,
        consumer_main,
        exposer_main,
    )
    .await;
    assert!(
        consumer_stdout.contains("feedback #1 new_position=[0, 1, 2]")
            && consumer_stdout.contains("feedback #2 new_position=[1, 2, 3]")
            && consumer_stdout.contains("feedback #3 new_position=[2, 3, 4]"),
        "consumer did not receive all 3 feedback messages.\nstdout:\n{}",
        consumer_stdout
    );
    assert!(
        consumer_stdout.contains("feedback channel closed after 3 messages"),
        "consumer drain loop did not exit via the close signal.\nstdout:\n{}",
        consumer_stdout
    );
    assert!(
        consumer_stdout.contains("result success=True final_position=[99, 99, 99]"),
        "consumer did not receive result.\nstdout:\n{}",
        consumer_stdout
    );

    assert!(
        exposer_stdout.contains("server emitted feedback #3 position=[2, 3, 4]")
            && exposer_stdout.contains("server handled result request"),
        "exposer did not complete the action lifecycle.\nstdout:\n{}",
        exposer_stdout
    );
}

/// Verifies producer-disappearance detection end-to-end through the Python
/// codegen: the exposer process is SIGKILLed mid-goal, so the end-of-stream
/// sentinel is never published. The consumer's feedback drain must then
/// raise `ConnectionError` (the producer-gone signal, distinct from the
/// clean-close `RuntimeError` pinned by
/// `actions_communication_drain_loop_until_end_signal`) once the liveliness
/// watcher confirms the producer instance is gone, and `get_result` must
/// resolve to `ResultStatus.ABANDONED` instead of timing out.
///
/// Rust parity is
/// `concurrent_action_producer_death_unblocks_feedback_and_yields_abandoned`
/// in the peppylib actions tests.
#[rstest::rstest]
#[case::peer(crate::helpers::LocalNodesTopology::Peer)]
#[case::router(crate::helpers::LocalNodesTopology::Router)]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn actions_communication_producer_killed_yields_connection_error_and_abandoned(
    #[case] topology: crate::helpers::LocalNodesTopology,
) {
    let instance = pmi::ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    // --- Consumer (client) project
    let consumer_instance_id = CONSUMER_INSTANCE_ID;
    let temp_dir_consumer = TempDir::new_in(config_test_support::test_tmp_root())
        .expect("failed to create temp dir for consumer project");
    let consumed_action: ConsumedAction = serde_json5::from_str(CONSUMED_ACTION_EXAMPLE).unwrap();
    let action_messages = consumed_move_arm_action_messages();
    let user_node_consumer = generate_node_project::<generator::PythonGenerator>(
        &temp_dir_consumer,
        &python_consumer_stub_node_config("brain", "v1", "brain"),
        |generator| {
            generator
                .add_consumed_action(
                    &consumed_action,
                    &action_messages,
                    &native_dep("brain", "v1", "brain"),
                )
                .unwrap();
        },
    );

    let consumer_runtime_config = test_node_runtime_config(
        &router_host,
        router_port,
        CONSUMER_NODE_NAME,
        consumer_instance_id,
    );
    let consumer_runtime_config = bind_slot(
        consumer_runtime_config,
        "brain",
        TEST_CORE_NODE,
        EXPOSER_INSTANCE_ID,
    );
    let consumer_runtime_config = crate::helpers::apply_topology(consumer_runtime_config, topology);
    let consumer_runtime_config_str =
        save_runtime_config(&consumer_runtime_config, temp_dir_consumer.path());

    init_python_user_node(&user_node_consumer);
    let consumer_main = r#"
import asyncio
import time
from peppygen import NodeBuilder
from peppygen.consumed_actions.brain import move_arm as brain_move_arm

async def run_consumer(node_runner):
    request = brain_move_arm.GoalRequest(arm_id=7, desired_position=[10, 20, 30])
    arm = brain_move_arm.bound_producer(node_runner)
    goal = await brain_move_arm.ActionHandle.fire_goal(
        node_runner, arm, request, 5.0
    )
    print(f"goal accepted={goal.data.accepted} t={time.monotonic():.3f}", flush=True)

    feedback = await goal.on_next_feedback_message()
    print(f"FIRST_FEEDBACK new_position={feedback.new_position} t={time.monotonic():.3f}", flush=True)

    # Drain until the stream ends. ConnectionError must be caught FIRST: it
    # is the producer-disappeared signal, while any other exception means the
    # clean end-of-stream sentinel was observed (which a SIGKILLed producer
    # can never publish). Buffered feedback may still arrive before the
    # liveliness watcher confirms the producer is gone.
    while True:
        try:
            beat = await goal.on_next_feedback_message()
            print(f"BEAT new_position={beat.new_position} t={time.monotonic():.3f}", flush=True)
        except ConnectionError as e:
            print(f"FEEDBACK_PRODUCER_GONE err={e!r} t={time.monotonic():.3f}", flush=True)
            break
        except Exception as e:
            print(f"FEEDBACK_CLEAN_CLOSE err={e!r} t={time.monotonic():.3f}", flush=True)
            break

    print(f"CALLING_GET_RESULT t={time.monotonic():.3f}", flush=True)
    try:
        result = await goal.get_result(5.0)
        print(f"RESULT_STATUS={result.status.name} t={time.monotonic():.3f}", flush=True)
    except Exception as e:
        print(f"RESULT_ERROR={e!r} t={time.monotonic():.3f}", flush=True)

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    return [asyncio.create_task(run_consumer(node_runner))]

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;
    let main_file = user_node_consumer.join("main.py");
    fs::write(main_file, consumer_main).expect("failed to write consumer main.py");

    // --- Exposer (server) project
    let temp_dir_exposer = TempDir::new_in(config_test_support::test_tmp_root())
        .expect("failed to create temp dir for exposer project");
    let exposed_action: NativeExposedAction =
        serde_json5::from_str(EXPOSED_ACTION_EXAMPLE).unwrap();
    let user_node_exposer = generate_node_project::<generator::PythonGenerator>(
        &temp_dir_exposer,
        STUB_PYTHON_NODE_CONFIG,
        |generator| {
            generator.add_exposed_action(&exposed_action, None).unwrap();
        },
    );

    let exposer_runtime_config = test_node_runtime_config(
        &router_host,
        router_port,
        BRAIN_NODE_NAME,
        EXPOSER_INSTANCE_ID,
    );
    let exposer_runtime_config = crate::helpers::apply_topology(exposer_runtime_config, topology);
    let exposer_runtime_config_str =
        save_runtime_config(&exposer_runtime_config, temp_dir_exposer.path());

    init_python_user_node(&user_node_exposer);
    // The worker never calls `ctx.complete(...)`: it streams feedback forever
    // so the only way the consumer's drain can end is the producer-gone path.
    let exposer_main = r#"
import asyncio
from peppygen import NodeBuilder
from peppygen.exposed_actions import move_arm

async def run_exposer(node_runner):
    action = await move_arm.ActionHandle.expose(node_runner)

    def goal_handler(_request):
        return move_arm.GoalDecision.accept(move_arm.GoalResponse(True))

    while True:
        ctx = await action.handle_goal_next_request(goal_handler)
        if ctx is None:
            break
        print("server accepted goal", flush=True)

        i = 0
        while True:
            i += 1
            await ctx.publish_feedback([i, i, i])
            await asyncio.sleep(0.2)

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    return [asyncio.create_task(run_exposer(node_runner))]

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;
    let main_file = user_node_exposer.join("main.py");
    fs::write(main_file, exposer_main).expect("failed to write exposer main.py");

    init_python_project_venv(&user_node_consumer);
    init_python_project_venv(&user_node_exposer);

    let messenger = peppylib::MessengerHandle::connect(&router_host, router_port)
        .await
        .expect("failed to create messenger for test control");

    let mut exposer_child = spawn_python_run(
        &user_node_exposer,
        &[(RUNTIME_CONFIG_VAR_NAME, &exposer_runtime_config_str)],
    );

    let ctx = test_wait_context(&messenger);
    wait_for_action_service_reachable_or_exit(
        &ctx,
        BRAIN_NODE_NAME,
        "move_arm",
        None,
        &mut exposer_child,
        &user_node_exposer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;

    let mut consumer = CapturedChild::new(spawn_python_run(
        &user_node_consumer,
        &[(RUNTIME_CONFIG_VAR_NAME, &consumer_runtime_config_str)],
    ));

    // The first feedback message proves the goal is established and the
    // per-goal feedback stream is live; only then is killing the producer
    // meaningful (killing earlier would test goal-request failure instead).
    consumer.wait_for_stdout_contains(
        "FIRST_FEEDBACK new_position=",
        DEFAULT_WAIT_TIMEOUT,
        &user_node_consumer,
    );

    // SIGKILL the exposer mid-goal: no sentinel, no graceful teardown. Its
    // exit status is deliberately not asserted: being killed is the point.
    // `spawn_python_run` wraps the node in `uv run`, and SIGKILLing only the
    // wrapper orphans the python node (it keeps running and publishing), so
    // the node's own process must be killed first: kill the wrapper's direct
    // children (`pkill -P`), the node among them, then the wrapper.
    std::process::Command::new("pkill")
        .args(["-KILL", "-P", &exposer_child.id().to_string()])
        .status()
        .expect("failed to SIGKILL exposer descendants");
    exposer_child
        .kill()
        .expect("failed to SIGKILL exposer process");
    let _ = exposer_child.wait();

    // Producer-gone confirmation takes ~600ms-1.5s (liveliness DELETE plus
    // confirmation probes); the default wait leaves ample slack for slow CI.
    consumer.wait_for_stdout_contains("RESULT_STATUS=", DEFAULT_WAIT_TIMEOUT, &user_node_consumer);

    // The consumer node may have already exited once its setup task finished.
    try_send_shutdown(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        CONSUMER_NODE_NAME,
        TEST_CORE_NODE,
        consumer_instance_id,
        Duration::from_secs(5),
    )
    .await;

    let consumer_output = consumer.wait(Some(Duration::from_secs(10)), &user_node_consumer);

    let (consumer_stdout, consumer_stderr) =
        assert_exited_successfully("consumer process", &consumer_output);
    assert!(
        consumer_stdout.contains("FEEDBACK_PRODUCER_GONE"),
        "consumer drain did not raise ConnectionError for the killed producer.\nstdout:\n{}\nstderr:\n{}",
        consumer_stdout,
        consumer_stderr
    );
    assert!(
        !consumer_stdout.contains("FEEDBACK_CLEAN_CLOSE"),
        "consumer saw a clean end-of-stream close: a SIGKILLed producer must surface as ConnectionError, not the sentinel path.\nstdout:\n{}",
        consumer_stdout
    );
    assert!(
        consumer_stdout.contains("RESULT_STATUS=ABANDONED"),
        "get_result did not resolve to ResultStatus.ABANDONED for the killed producer.\nstdout:\n{}\nstderr:\n{}",
        consumer_stdout,
        consumer_stderr
    );
}

const ARM_CONTRACT_NAME: &str = "arm_actions";
const ARM_SERVER_NODE_NAME: &str = "arm_server";
const ACTION_CONTRACT_FLOW_DONE_SERVICE: &str = "move_arm_contract_flow_done";
const EXPOSED_ACTION_CONTRACT_FLOW_DONE_SERVICE_EXAMPLE: &str = r#"
{
  name: "move_arm_contract_flow_done",
  description: "A test member.",
}
"#;
/// The `failure_kinds` block of the `arm_actions` contract: both sides of the
/// contract-slot test generate from it.
const ARM_CONTRACT_FAILURE_KINDS: &[(&str, &str)] = &[
    ("arm_reserved", "The arm of the goal is reserved"),
    ("unreachable", "No plan to the target"),
];
const CONSUMED_ACTION_CONTRACT_EXAMPLE: &str = r#"
{
  link_id: "arm",
  name: "move_arm",
}
"#;

/// Consumer-side contract-slot capstone, Python twin of the Rust
/// `actions_communication_via_contract_slot`: the consumer's manifest
/// declares the action's slot under `depends_on.contracts`, the producer
/// exposes the same action through a `manifest.implements` slot, and both
/// sides address the wire via `SenderTarget.contract`. The exposer node is
/// deliberately named `arm_server`, a name the consumer never mentions.
/// Covers the full lifecycle over a live router: framework rejection,
/// accept with declared response, feedback, completed result, then cancel
/// driving a cancelled result on a second goal.
#[rstest::rstest]
#[case::peer(crate::helpers::LocalNodesTopology::Peer)]
#[case::router(crate::helpers::LocalNodesTopology::Router)]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn actions_communication_via_contract_slot(
    #[case] topology: crate::helpers::LocalNodesTopology,
) {
    let instance = pmi::ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    // --- Consumer (client) project: one contract slot, bound to the exposer.
    let consumer_instance_id = CONSUMER_INSTANCE_ID;
    let temp_dir_consumer = TempDir::new_in(config_test_support::test_tmp_root())
        .expect("failed to create temp dir for consumer project");
    let consumed_action: ConsumedAction =
        serde_json5::from_str(CONSUMED_ACTION_CONTRACT_EXAMPLE).unwrap();
    let action_messages = consumed_move_arm_action_messages();
    let flow_done_service: NativeExposedService =
        serde_json5::from_str(EXPOSED_ACTION_CONTRACT_FLOW_DONE_SERVICE_EXAMPLE).unwrap();
    let user_node_consumer = generate_node_project::<generator::PythonGenerator>(
        &temp_dir_consumer,
        &contract_consumer_stub_node_config(
            ARM_CONTRACT_NAME,
            "v1",
            "arm",
            "one",
            PYTHON_STUB_EXECUTION,
        ),
        |generator| {
            generator
                .add_consumed_action(
                    &consumed_action,
                    &action_messages,
                    &contract_dep_with_kinds(
                        ARM_CONTRACT_NAME,
                        "v1",
                        "arm",
                        ARM_CONTRACT_FAILURE_KINDS,
                        &[(MemberSection::Action, "move_arm")],
                    ),
                )
                .unwrap();
            generator
                .add_exposed_service(&flow_done_service, None)
                .unwrap();
        },
    );

    let consumer_runtime_config = test_node_runtime_config(
        &router_host,
        router_port,
        CONSUMER_NODE_NAME,
        consumer_instance_id,
    );
    let consumer_runtime_config = bind_slot(
        consumer_runtime_config,
        "arm",
        TEST_CORE_NODE,
        EXPOSER_INSTANCE_ID,
    );
    let consumer_runtime_config = crate::helpers::apply_topology(consumer_runtime_config, topology);
    let consumer_runtime_config_str =
        save_runtime_config(&consumer_runtime_config, temp_dir_consumer.path());

    init_python_user_node(&user_node_consumer);
    let consumer_main = r#"
import asyncio
from peppygen import NodeBuilder
from peppygen.exposed_services import move_arm_contract_flow_done
from peppygen.consumed_actions.arm import move_arm as arm_move_arm

async def run_consumer(node_runner):
    arm = arm_move_arm.bound_producer(node_runner)

    # A refusal with a kind of the contract rides the admission ack and
    # reaches the consumer as its typed `Failure`.
    reserved = arm_move_arm.GoalRequest(arm_id=99, desired_position=[0, 0, 0])
    refused = await arm_move_arm.ActionHandle.fire_goal(
        node_runner, arm, reserved, 5.0
    )
    assert refused.refusal.kind is arm_move_arm.FailureKind.ARM_RESERVED
    print(
        f"refused goal accepted={refused.accepted} kind={refused.refusal.kind.value} "
        f"message={refused.refusal.message} data_present={refused.data is not None}",
        flush=True,
    )

    # Accepted goal: feedback, then a completed result.
    request = arm_move_arm.GoalRequest(arm_id=7, desired_position=[10, 20, 30])
    goal = await arm_move_arm.ActionHandle.fire_goal(
        node_runner, arm, request, 5.0
    )
    print(f"goal accepted={goal.accepted} data_accepted={goal.data.accepted}", flush=True)

    feedback = await goal.on_next_feedback_message()
    print(f"feedback message received new_position={feedback.new_position}", flush=True)

    result = await goal.get_result(5.0)
    assert result.status == arm_move_arm.ResultStatus.COMPLETED, f"unexpected status {result.status}"
    print(
        f"result success={result.data.success} final_position={result.data.final_position}",
        flush=True,
    )

    # Second goal: cancel drives the cancelled result through the same
    # contract-addressed endpoints.
    cancel_request = arm_move_arm.GoalRequest(arm_id=8, desired_position=[1, 2, 3])
    cancel_goal = await arm_move_arm.ActionHandle.fire_goal(
        node_runner, arm, cancel_request, 5.0
    )
    cancel_response = await cancel_goal.cancel_goal(5.0)
    print(
        f"cancel signalled={cancel_response.state == arm_move_arm.CancelState.SIGNALLED}",
        flush=True,
    )

    result = await cancel_goal.get_result(5.0)
    assert result.status == arm_move_arm.ResultStatus.CANCELLED, f"unexpected status {result.status}"
    print(
        f"cancelled result reason={result.cancel_reason} success={result.data.success} error={result.data.error_msg}",
        flush=True,
    )

    # Third goal: the provider fails it with a kind of the contract, and no
    # result.
    failed_request = arm_move_arm.GoalRequest(arm_id=9, desired_position=[500, 500, 500])
    failed_goal = await arm_move_arm.ActionHandle.fire_goal(
        node_runner, arm, failed_request, 5.0
    )
    result = await failed_goal.get_result(5.0)
    assert result.status == arm_move_arm.ResultStatus.FAILED, f"unexpected status {result.status}"
    assert result.failure.kind is arm_move_arm.FailureKind.UNREACHABLE
    print(
        f"failed result kind={result.failure.kind.value} message={result.failure.message} data_present={result.data is not None}",
        flush=True,
    )

    await move_arm_contract_flow_done.handle_next_request(node_runner, lambda _request: None)

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    return [asyncio.create_task(run_consumer(node_runner))]

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;
    let main_file = user_node_consumer.join("main.py");
    fs::write(main_file, consumer_main).expect("failed to write consumer main.py");

    // --- Exposer (server) project: exposes the action through a
    // `manifest.implements`-shaped contract origin.
    let exposer_instance_id = EXPOSER_INSTANCE_ID;
    let temp_dir_exposer = TempDir::new_in(config_test_support::test_tmp_root())
        .expect("failed to create temp dir for exposer project");
    let exposed_action: NativeExposedAction =
        serde_json5::from_str(EXPOSED_ACTION_EXAMPLE).unwrap();
    let user_node_exposer = generate_node_project::<generator::PythonGenerator>(
        &temp_dir_exposer,
        STUB_PYTHON_NODE_CONFIG,
        |generator| {
            generator
                .add_exposed_action(
                    &exposed_action,
                    Some(&ContractOrigin {
                        link_id: "arm_impl".to_string(),
                        contract_name: ARM_CONTRACT_NAME.to_string(),
                        contract_tag: "v1".to_string(),
                        failure_kinds: failure_kinds(
                            ARM_CONTRACT_FAILURE_KINDS,
                            &[(MemberSection::Action, "move_arm")],
                        ),
                    }),
                )
                .unwrap();
        },
    );

    let exposer_runtime_config = test_node_runtime_config(
        &router_host,
        router_port,
        ARM_SERVER_NODE_NAME,
        exposer_instance_id,
    );
    let exposer_runtime_config = crate::helpers::apply_topology(exposer_runtime_config, topology);
    let exposer_runtime_config_str =
        save_runtime_config(&exposer_runtime_config, temp_dir_exposer.path());

    init_python_user_node(&user_node_exposer);
    let exposer_main = r#"
import asyncio
from peppygen import NodeBuilder
from peppygen.exposed_actions.arm_impl import move_arm

async def run_exposer(node_runner):
    action = await move_arm.ActionHandle.expose(node_runner)

    def goal_handler(request):
        print(
            f"server received goal arm_id={request.data.arm_id} desired={request.data.desired_position}",
            flush=True,
        )
        if request.data.arm_id == 99:
            return move_arm.GoalDecision.refuse(
                move_arm.FailureKind.ARM_RESERVED, "arm 99 is reserved"
            )
        return move_arm.GoalDecision.accept(move_arm.GoalResponse(True))

    while True:
        ctx = await action.handle_goal_next_request(goal_handler)
        if ctx is None:
            break
        if ctx.request().data.arm_id == 8:
            # Wait for the cancel, then end the goal cancelled with the
            # reason of its cause.
            await ctx.cancel_signal()
            print(f"server observed cancel for goal cause={ctx.cancel_cause()}", flush=True)
            await ctx.complete_cancelled(ctx.cancel_reason(), False, "goal cancelled by server", [0, 0, 0])
        elif ctx.request().data.arm_id == 9:
            await ctx.fail(move_arm.FailureKind.UNREACHABLE, "no plan to [500, 500, 500]")
            print("server failed goal", flush=True)
        else:
            feedback_message = [7, 31, 43]
            await ctx.publish_feedback(feedback_message)
            print(f"server emitted feedback message {feedback_message}", flush=True)
            await ctx.complete(True, None, [98, 4, 26])
            print("server completed goal", flush=True)

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    return [asyncio.create_task(run_exposer(node_runner))]

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;
    let main_file = user_node_exposer.join("main.py");
    fs::write(main_file, exposer_main).expect("failed to write exposer main.py");

    init_python_project_venv(&user_node_consumer);
    init_python_project_venv(&user_node_exposer);

    let messenger = peppylib::MessengerHandle::connect(&router_host, router_port)
        .await
        .expect("failed to create messenger for test control");

    // Spawn exposer first so it's ready to handle requests. The probe uses
    // the contract-shaped target, exactly what the consumer fires at.
    let mut exposer_child = spawn_python_run(
        &user_node_exposer,
        &[(RUNTIME_CONFIG_VAR_NAME, &exposer_runtime_config_str)],
    );

    let ctx = test_wait_context(&messenger);
    wait_for_action_reachable_via_target_or_exit(
        &ctx,
        test_contract_target(ARM_CONTRACT_NAME),
        "move_arm",
        None,
        &mut exposer_child,
        &user_node_exposer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;

    let mut consumer_child = spawn_python_run(
        &user_node_consumer,
        &[(RUNTIME_CONFIG_VAR_NAME, &consumer_runtime_config_str)],
    );

    wait_for_health_service_reachable_or_exit(
        &ctx,
        CONSUMER_NODE_NAME,
        consumer_instance_id,
        &mut consumer_child,
        &user_node_consumer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;
    wait_for_health_service_reachable_or_exit(
        &ctx,
        ARM_SERVER_NODE_NAME,
        exposer_instance_id,
        &mut exposer_child,
        &user_node_exposer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;
    wait_for_service_reachable_or_exit(
        &ctx,
        CONSUMER_NODE_NAME,
        ACTION_CONTRACT_FLOW_DONE_SERVICE,
        Some(consumer_instance_id),
        &mut consumer_child,
        &user_node_consumer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;

    shut_down_test_node(&messenger, CONSUMER_NODE_NAME, consumer_instance_id).await;
    shut_down_test_node(&messenger, ARM_SERVER_NODE_NAME, exposer_instance_id).await;

    let consumer_output = wait_for_child(
        &mut consumer_child,
        Some(Duration::from_secs(10)),
        &user_node_consumer,
    );
    let exposer_output = wait_for_child(
        &mut exposer_child,
        Some(Duration::from_secs(10)),
        &user_node_exposer,
    );

    let (consumer_stdout, consumer_stderr) =
        assert_exited_successfully("consumer process", &consumer_output);
    assert!(
        consumer_stdout.contains(
            "refused goal accepted=False kind=arm_reserved message=arm 99 is reserved data_present=False"
        ) && consumer_stdout.contains("goal accepted=True data_accepted=True")
            && consumer_stdout.contains("feedback message received new_position=[7, 31, 43]")
            && consumer_stdout.contains("result success=True final_position=[98, 4, 26]")
            && consumer_stdout.contains("cancel signalled=True")
            && consumer_stdout.contains(
                "cancelled result reason=cancelled by its caller success=False error=goal cancelled by server"
            )
            && consumer_stdout.contains(
                "failed result kind=unreachable message=no plan to [500, 500, 500] data_present=False"
            ),
        "consumer did not complete the contract-addressed action lifecycle.\nstdout:\n{}\nstderr:\n{}",
        consumer_stdout,
        consumer_stderr
    );

    let (exposer_stdout, exposer_stderr) =
        assert_exited_successfully("exposer process", &exposer_output);
    assert!(
        exposer_stdout.contains("server received goal arm_id=99 desired=[0, 0, 0]")
            && exposer_stdout.contains("server received goal arm_id=7 desired=[10, 20, 30]")
            && exposer_stdout.contains("server emitted feedback message [7, 31, 43]")
            && exposer_stdout.contains("server completed goal")
            && exposer_stdout.contains("server received goal arm_id=8 desired=[1, 2, 3]")
            && exposer_stdout.contains("server observed cancel for goal cause=caller")
            && exposer_stdout.contains("server failed goal"),
        "exposer did not process the contract-addressed action endpoints as expected.\nstdout:\n{}\nstderr:\n{}",
        exposer_stdout,
        exposer_stderr
    );
}

const JOINTS_FLOW_DONE_SERVICE: &str = "joints_flow_done";

/// Wire-compatibility capstone for `refine`, Python twin of the Rust
/// `actions_refined_producer_interoperates_with_generic_consumer`: the
/// producer generates from the contract member with its `refine` block
/// applied (length-checked three-element lists on the goal and the result),
/// the consumer from the member as the contract declares it, and the two
/// talk over a live router through the contract slot. A three-joint goal
/// decodes on the producer and completes; a two-joint goal is valid for the
/// contract but not for the producer's pinned shape, so the generated server
/// answers it with a rejection carrying the decode error, never hands it to
/// the node's goal handler, and keeps serving, which a third goal proves.
/// Runs the peer topology only; the transport topology is orthogonal to
/// what is under test here.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn actions_refined_producer_interoperates_with_generic_consumer() {
    let topology = crate::helpers::LocalNodesTopology::Peer;
    let instance = pmi::ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    let contract_action: NativeExposedAction =
        serde_json5::from_str(JOINTS_CONTRACT_ACTION).unwrap();

    // --- Consumer (client) project: the contract member as written, plus a
    // service whose first call tells the test the flow has run.
    let consumer_instance_id = CONSUMER_INSTANCE_ID;
    let temp_dir_consumer = TempDir::new_in(config_test_support::test_tmp_root())
        .expect("failed to create temp dir for consumer project");
    let consumed_action: ConsumedAction =
        serde_json5::from_str(r#"{ link_id: "arm", name: "move_arm_joints" }"#).unwrap();
    let action_messages = ConsumedActionMessage::from(&contract_action);
    let flow_done_service: NativeExposedService = serde_json5::from_str(&format!(
        r#"{{ name: "{JOINTS_FLOW_DONE_SERVICE}", description: "A test member." }}"#
    ))
    .unwrap();
    let user_node_consumer = generate_node_project::<generator::PythonGenerator>(
        &temp_dir_consumer,
        &contract_consumer_stub_node_config(
            JOINTS_CONTRACT_NAME,
            "v1",
            "arm",
            "one",
            PYTHON_STUB_EXECUTION,
        ),
        |generator| {
            generator
                .add_consumed_action(
                    &consumed_action,
                    &action_messages,
                    &contract_dep(JOINTS_CONTRACT_NAME, "v1", "arm"),
                )
                .unwrap();
            generator
                .add_exposed_service(&flow_done_service, None)
                .unwrap();
        },
    );

    let consumer_runtime_config = test_node_runtime_config(
        &router_host,
        router_port,
        CONSUMER_NODE_NAME,
        consumer_instance_id,
    );
    let consumer_runtime_config = bind_slot(
        consumer_runtime_config,
        "arm",
        TEST_CORE_NODE,
        EXPOSER_INSTANCE_ID,
    );
    let consumer_runtime_config = crate::helpers::apply_topology(consumer_runtime_config, topology);
    let consumer_runtime_config_str =
        save_runtime_config(&consumer_runtime_config, temp_dir_consumer.path());

    init_python_user_node(&user_node_consumer);
    let consumer_main = r#"
import asyncio
from peppygen import NodeBuilder
from peppygen.exposed_services import joints_flow_done
from peppygen.consumed_actions.arm import move_arm_joints as joints

async def fire(node_runner, arm, arm_id, joint_positions):
    # The consumer's generated request carries the contract's generic list;
    # the length is whatever the caller passes.
    request = joints.GoalRequest(arm_id=arm_id, joint_positions=joint_positions)
    return await joints.ActionHandle.fire_goal(node_runner, arm, request, 5.0)

async def run_consumer(node_runner):
    arm = joints.bound_producer(node_runner)

    # Three joints: the length the producer pins, so the goal decodes there.
    goal = await fire(node_runner, arm, 1, [0.1, 0.2, 0.3])
    print(f"goal 1 accepted={goal.accepted}", flush=True)
    result = await goal.get_result(5.0)
    assert result.status == joints.ResultStatus.COMPLETED, f"unexpected status {result.status}"
    print(f"result 1 success={result.data.success} final={result.data.final_joint_positions}", flush=True)

    # Two joints: the contract admits it, the producer's pinned shape does
    # not, and the answer is a rejection carrying the decode error.
    rejected = await fire(node_runner, arm, 2, [0.1, 0.2])
    print(
        f"goal 2 accepted={rejected.accepted} kind={rejected.refusal.kind.value} message={rejected.refusal.message}",
        flush=True,
    )

    # The producer kept serving after the rejection.
    goal = await fire(node_runner, arm, 3, [0.4, 0.5, 0.6])
    print(f"goal 3 accepted={goal.accepted}", flush=True)
    result = await goal.get_result(5.0)
    assert result.status == joints.ResultStatus.COMPLETED, f"unexpected status {result.status}"
    print(f"result 3 success={result.data.success}", flush=True)

    await joints_flow_done.handle_next_request(node_runner, lambda _request: None)

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    return [asyncio.create_task(run_consumer(node_runner))]

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;
    fs::write(user_node_consumer.join("main.py"), consumer_main)
        .expect("failed to write consumer main.py");

    // --- Exposer (server) project: the same member with the implementer's
    // `refine` block applied, under a `manifest.implements`-shaped origin.
    let exposer_instance_id = EXPOSER_INSTANCE_ID;
    let temp_dir_exposer = TempDir::new_in(config_test_support::test_tmp_root())
        .expect("failed to create temp dir for exposer project");
    use config::node::Refines;
    let refinement: config::node::ActionRefinement =
        serde_json5::from_str(JOINTS_REFINEMENT).unwrap();
    let exposed_action = refinement
        .apply(contract_action)
        .expect("the contract member admits both pins");
    let user_node_exposer = generate_node_project::<generator::PythonGenerator>(
        &temp_dir_exposer,
        STUB_PYTHON_NODE_CONFIG,
        |generator| {
            generator
                .add_exposed_action(
                    &exposed_action,
                    Some(&ContractOrigin {
                        link_id: "arm_impl".to_string(),
                        contract_name: JOINTS_CONTRACT_NAME.to_string(),
                        contract_tag: "v1".to_string(),
                        failure_kinds: Default::default(),
                    }),
                )
                .unwrap();
        },
    );

    let exposer_runtime_config = test_node_runtime_config(
        &router_host,
        router_port,
        ARM_SERVER_NODE_NAME,
        exposer_instance_id,
    );
    let exposer_runtime_config = crate::helpers::apply_topology(exposer_runtime_config, topology);
    let exposer_runtime_config_str =
        save_runtime_config(&exposer_runtime_config, temp_dir_exposer.path());

    init_python_user_node(&user_node_exposer);
    let exposer_main = r#"
import asyncio
from peppygen import NodeBuilder
from peppygen.exposed_actions.arm_impl import move_arm_joints

async def run_exposer(node_runner):
    action = await move_arm_joints.ActionHandle.expose(node_runner)

    def goal_handler(request):
        # A goal of any length other than the pinned three never reaches
        # this handler.
        print(
            f"server received goal arm_id={request.data.arm_id} joints={request.data.joint_positions}",
            flush=True,
        )
        return move_arm_joints.GoalDecision.accept(move_arm_joints.GoalResponse(True))

    while True:
        ctx = await action.handle_goal_next_request(goal_handler)
        if ctx is None:
            break
        await ctx.complete(True, [1.0, 2.0, 3.0])
        print("server completed goal", flush=True)

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    return [asyncio.create_task(run_exposer(node_runner))]

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;
    fs::write(user_node_exposer.join("main.py"), exposer_main)
        .expect("failed to write exposer main.py");

    init_python_project_venv(&user_node_consumer);
    init_python_project_venv(&user_node_exposer);

    let messenger = peppylib::MessengerHandle::connect(&router_host, router_port)
        .await
        .expect("failed to create messenger for test control");

    let mut exposer_child = spawn_python_run(
        &user_node_exposer,
        &[(RUNTIME_CONFIG_VAR_NAME, &exposer_runtime_config_str)],
    );

    let ctx = test_wait_context(&messenger);
    wait_for_action_reachable_via_target_or_exit(
        &ctx,
        test_contract_target(JOINTS_CONTRACT_NAME),
        "move_arm_joints",
        None,
        &mut exposer_child,
        &user_node_exposer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;

    let mut consumer_child = spawn_python_run(
        &user_node_consumer,
        &[(RUNTIME_CONFIG_VAR_NAME, &consumer_runtime_config_str)],
    );

    wait_for_health_service_reachable_or_exit(
        &ctx,
        CONSUMER_NODE_NAME,
        consumer_instance_id,
        &mut consumer_child,
        &user_node_consumer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;
    wait_for_health_service_reachable_or_exit(
        &ctx,
        ARM_SERVER_NODE_NAME,
        exposer_instance_id,
        &mut exposer_child,
        &user_node_exposer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;
    wait_for_service_reachable_or_exit(
        &ctx,
        CONSUMER_NODE_NAME,
        JOINTS_FLOW_DONE_SERVICE,
        Some(consumer_instance_id),
        &mut consumer_child,
        &user_node_consumer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;

    shut_down_test_node(&messenger, CONSUMER_NODE_NAME, consumer_instance_id).await;
    shut_down_test_node(&messenger, ARM_SERVER_NODE_NAME, exposer_instance_id).await;

    let consumer_output = wait_for_child(
        &mut consumer_child,
        Some(Duration::from_secs(10)),
        &user_node_consumer,
    );
    let exposer_output = wait_for_child(
        &mut exposer_child,
        Some(Duration::from_secs(10)),
        &user_node_exposer,
    );

    let (consumer_stdout, consumer_stderr) =
        assert_exited_successfully("consumer process", &consumer_output);
    for needle in [
        "goal 1 accepted=True",
        "result 1 success=True final=[1.0, 2.0, 3.0]",
        "goal 2 accepted=False kind=rejected message=goal request does not decode: ",
        "invalid fixed list length for field 'joint_positions': expected 3, got 2",
        "goal 3 accepted=True",
        "result 3 success=True",
    ] {
        assert!(
            consumer_stdout.contains(needle),
            "consumer stdout lacks `{needle}`.\nstdout:\n{consumer_stdout}\nstderr:\n{consumer_stderr}"
        );
    }

    let (exposer_stdout, exposer_stderr) =
        assert_exited_successfully("exposer process", &exposer_output);
    assert!(
        exposer_stdout.contains("server received goal arm_id=1 joints=[0.1, 0.2, 0.3]")
            && exposer_stdout.contains("server received goal arm_id=3 joints=[0.4, 0.5, 0.6]")
            && exposer_stdout.matches("server completed goal").count() == 2,
        "exposer did not run exactly the two decodable goals.\nstdout:\n{exposer_stdout}\nstderr:\n{exposer_stderr}"
    );
    assert!(
        !exposer_stdout.contains("arm_id=2"),
        "the undecodable goal must never reach the goal handler.\nstdout:\n{exposer_stdout}"
    );
}
