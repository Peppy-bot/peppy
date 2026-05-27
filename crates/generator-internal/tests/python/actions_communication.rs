use crate::helpers::{
    DEFAULT_WAIT_TIMEOUT, STUB_PYTHON_NODE_CONFIG, WaitContext, copy_config_to_output,
    init_python_project_venv, init_python_user_node, init_test_env, send_shutdown,
    spawn_python_run, test_peppy_dirs, wait_for_action_service_reachable_or_exit, wait_for_child,
    wait_for_health_service_reachable_or_exit, wait_for_service_reachable_or_exit,
};
use config::consts::{PEPPYGEN_OUTPUT_PATH, RUNTIME_CONFIG_VAR_NAME};
use config::runtime::NodeInstanceConfig;
use config::{
    launcher::Name,
    node::{ConsumedAction, ExposedAction, ExposedService, MessageFormat},
    runtime::RuntimeConfig,
};
use generator::{ConsumedActionMessage, LanguageGenerator};
use std::path::Path;
use std::{fs, time::Duration};
use tempfile::TempDir;

// --- Common test constants
const TEST_CORE_NODE: &str = "test_core";
const CONSUMER_NODE_NAME: &str = "consumer_node";
const CONSUMER_INSTANCE_ID: &str = "consumer_instance";
const EXPOSER_INSTANCE_ID: &str = "exposer_instance";
const SHUTDOWN_SENDER_INSTANCE_ID: &str = "test_shutdown_sender";
const BRAIN_NODE_NAME: &str = "brain";
const ACTION_FLOW_DONE_SERVICE: &str = "move_arm_flow_done";
const ACTION_CANCEL_FLOW_DONE_SERVICE: &str = "move_arm_cancel_flow_done";
const ACTION_CANCEL_ACCEPT_FLOW_DONE_SERVICE: &str = "move_arm_cancel_accept_flow_done";
const ACTION_CANCEL_REJECT_FLOW_DONE_SERVICE: &str = "move_arm_cancel_reject_flow_done";
const ACTION_DRAIN_LOOP_FLOW_DONE_SERVICE: &str = "move_arm_drain_loop_flow_done";
const EXPOSED_ACTION_FLOW_DONE_SERVICE_EXAMPLE: &str = r#"
{
  name: "move_arm_flow_done"
}
"#;
const EXPOSED_ACTION_CANCEL_ACCEPT_FLOW_DONE_SERVICE_EXAMPLE: &str = r#"
{
  name: "move_arm_cancel_accept_flow_done"
}
"#;
const EXPOSED_ACTION_CANCEL_REJECT_FLOW_DONE_SERVICE_EXAMPLE: &str = r#"
{
  name: "move_arm_cancel_reject_flow_done"
}
"#;
const EXPOSED_ACTION_DRAIN_LOOP_FLOW_DONE_SERVICE_EXAMPLE: &str = r#"
{
  name: "move_arm_drain_loop_flow_done"
}
"#;
const EXPOSED_ACTION_CANCEL_FLOW_DONE_SERVICE_EXAMPLE: &str = r#"
{
  name: "move_arm_cancel_flow_done"
}
"#;

const EXPOSED_ACTION_EXAMPLE: &str = r#"
{
  name: "move_arm",
  goal_service: {
    request_message_format: {
      arm_id: "u16",
      desired_position: {
        $type: "array",
        $items: "i32",
        $length: 3
      }
    },
    response_message_format: {
      accepted: "bool"
    }
  },
  feedback_topic: {
    qos_profile: "sensor_data",
    message_format: {
      new_position: {
        $type: "array",
        $items: "i32",
        $length: 3
      }
    }
  },
  result_service: {
    response_message_format: {
      success: "bool",
      error_msg: {
        $type: "string",
        $optional: true
      },
      final_position: {
        $type: "array",
        $items: "i32",
        $length: 3
      }
    }
  }
}
"#;

const CONSUMED_ACTION_EXAMPLE: &str = r#"
{
  link_id: "brain",
  name: "move_arm",
}
"#;

const CONSUMED_ACTION_FEEDBACK_FORMAT: &str = r#"
{
  new_position: {
    $type: "array",
    $items: "i32",
    $length: 3
  }
}
"#;

const CONSUMED_ACTION_RESULT_FORMAT: &str = r#"
{
  success: "bool",
  error_msg: {
    $type: "string",
    $optional: true
  },
  final_position: {
    $type: "array",
    $items: "i32",
    $length: 3
  }
}
"#;

const CONSUMED_ACTION_GOAL_FORMAT: &str = r#"
{
  arm_id: "u16",
  desired_position: {
    $type: "array",
    $items: "i32",
    $length: 3
  }
}
"#;

const CONSUMED_ACTION_GOAL_RESPONSE_FORMAT: &str = r#"
{
  accepted: "bool"
}
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn actions_communication() {
    let instance = pmi::ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    // --- Consumer (client) project
    let consumer_instance_id = CONSUMER_INSTANCE_ID;
    let temp_dir_consumer = TempDir::new().unwrap();
    let consumed_action: ConsumedAction = serde_json5::from_str(CONSUMED_ACTION_EXAMPLE).unwrap();
    let goal_request_format: MessageFormat =
        serde_json5::from_str(CONSUMED_ACTION_GOAL_FORMAT).unwrap();
    let goal_response_format: MessageFormat =
        serde_json5::from_str(CONSUMED_ACTION_GOAL_RESPONSE_FORMAT).unwrap();
    let feedback_format: MessageFormat =
        serde_json5::from_str(CONSUMED_ACTION_FEEDBACK_FORMAT).unwrap();
    let result_response_format: MessageFormat =
        serde_json5::from_str(CONSUMED_ACTION_RESULT_FORMAT).unwrap();
    let action_messages = ConsumedActionMessage {
        goal_request: Some(goal_request_format),
        goal_response: Some(goal_response_format),
        feedback: Some(feedback_format),
        result_request: None,
        result_response: Some(result_response_format),
    };
    let (mut generator, output_dir_consumer, user_node_consumer, peppy_node_config_path) =
        init_test_env::<generator::PythonGenerator>(&temp_dir_consumer, STUB_PYTHON_NODE_CONFIG);
    let flow_done_service: ExposedService =
        serde_json5::from_str(EXPOSED_ACTION_FLOW_DONE_SERVICE_EXAMPLE).unwrap();
    generator
        .add_consumed_action(
            &consumed_action,
            &action_messages,
            &generator::DependencyContext::native("brain", "v1"),
        )
        .unwrap();
    generator
        .add_exposed_service(&flow_done_service, None)
        .unwrap();
    let output_config = copy_config_to_output(&user_node_consumer, &output_dir_consumer);
    generator
        .build(&output_dir_consumer, &test_peppy_dirs(), Default::default())
        .unwrap();
    fs::remove_file(output_config).unwrap();
    config::fingerprint::create_codegen_fingerprint(
        &peppy_node_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let consumer_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstanceConfig::new(Name::new(consumer_instance_id).unwrap()),
        CONSUMER_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let consumer_runtime_config_path = temp_dir_consumer.path().join("peppy_runtime.json5");
    consumer_runtime_config
        .save_json5_launch_config(&consumer_runtime_config_path)
        .unwrap();

    init_python_user_node(&user_node_consumer);
    let consumer_main = r#"
import asyncio
from peppygen import NodeBuilder, QoSProfile
from peppygen.exposed_services import move_arm_flow_done
from peppygen.consumed_actions import brain_move_arm

async def run_consumer(node_runner):
    request = brain_move_arm.GoalRequest(arm_id=7, desired_position=[10, 20, 30])
    goal = await brain_move_arm.ActionHandle.fire_goal(
        node_runner, request, 5.0, QoSProfile.SensorData
    )
    print(f"goal accepted={goal.data.accepted}", flush=True)

    feedback = await goal.on_next_feedback_message()
    assert feedback.new_position == [7, 31, 43], "unexpected feedback message"
    print(f"feedback message received new_position={feedback.new_position}", flush=True)

    result = await goal.get_result(5.0)
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
    let main_file = user_node_consumer.join("main.py");
    fs::write(main_file, consumer_main).expect("failed to write consumer main.py");

    // --- Exposer (server) project
    let exposer_instance_id = EXPOSER_INSTANCE_ID;
    let temp_dir_exposer = TempDir::new().unwrap();
    let exposed_action: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE).unwrap();
    let (mut generator, output_dir_exposer, user_node_exposer, peppy_node_config_path) =
        init_test_env::<generator::PythonGenerator>(&temp_dir_exposer, STUB_PYTHON_NODE_CONFIG);
    generator.add_exposed_action(&exposed_action, None).unwrap();
    let output_config = copy_config_to_output(&user_node_exposer, &output_dir_exposer);
    generator
        .build(&output_dir_exposer, &test_peppy_dirs(), Default::default())
        .unwrap();
    fs::remove_file(output_config).unwrap();
    config::fingerprint::create_codegen_fingerprint(
        &peppy_node_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let exposer_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstanceConfig::new(Name::new(exposer_instance_id).unwrap()),
        BRAIN_NODE_NAME, // Must match the node name expected by the consumer
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let exposer_runtime_config_path = temp_dir_exposer.path().join("peppy_runtime.json5");
    exposer_runtime_config
        .save_json5_launch_config(&exposer_runtime_config_path)
        .unwrap();

    init_python_user_node(&user_node_exposer);
    let exposer_main = r#"
import asyncio
from peppygen import NodeBuilder
from peppygen.exposed_actions import move_arm

async def run_exposer(node_runner):
    action = await move_arm.ActionHandle.expose(node_runner)

    def decide(request):
        print(
            f"server received goal arm_id={request.data.arm_id} desired={request.data.desired_position}",
            flush=True,
        )
        return move_arm.GoalResponse(accepted=True)

    # Accept loop: a real server keeps running so its results stay fetchable.
    while True:
        ctx = await action.handle_goal_next_request(decide)
        if ctx is None:
            break

        feedback_message = [7, 31, 43]
        await ctx.publish_feedback(feedback_message)
        print(f"server emitted feedback message {feedback_message}", flush=True)

        final_position = [98, 4, 26]
        print("server preparing action result", flush=True)
        await ctx.complete(success=True, error_msg=None, final_position=final_position)
        print(f"server handled result request. Final position sent: {final_position}", flush=True)

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    return [asyncio.create_task(run_exposer(node_runner))]

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;
    let main_file = user_node_exposer.join("main.py");
    fs::write(main_file, exposer_main).expect("failed to write exposer main.py");

    println!(
        "User node consumer PEPPY_RUNTIME_CONFIG=\"{}\"",
        &consumer_runtime_config_path.display()
    );
    println!("User node consumer = {}", user_node_consumer.display());
    println!(
        "User node exposer PEPPY_RUNTIME_CONFIG=\"{}\"",
        &exposer_runtime_config_path.display()
    );
    println!("User node exposer = {}", user_node_exposer.display());

    init_python_project_venv(&user_node_consumer);
    init_python_project_venv(&user_node_exposer);

    let exposer_runtime_config_str = exposer_runtime_config_path.to_str().unwrap().to_owned();
    let consumer_runtime_config_str = consumer_runtime_config_path.to_str().unwrap().to_owned();

    let messenger = peppylib::MessengerHandle::from_host_port(&router_host, router_port)
        .await
        .expect("failed to create messenger for test control");

    // Spawn exposer first so it's ready to handle requests
    let mut exposer_child = spawn_python_run(
        &user_node_exposer,
        &[(RUNTIME_CONFIG_VAR_NAME, &exposer_runtime_config_str)],
    );

    let action_ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_core_node: None,
    };
    wait_for_action_service_reachable_or_exit(
        &action_ctx,
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

    let health_ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_core_node: Some(TEST_CORE_NODE),
    };
    wait_for_health_service_reachable_or_exit(
        &health_ctx,
        CONSUMER_NODE_NAME,
        consumer_instance_id,
        &mut consumer_child,
        &user_node_consumer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;
    wait_for_health_service_reachable_or_exit(
        &health_ctx,
        BRAIN_NODE_NAME,
        exposer_instance_id,
        &mut exposer_child,
        &user_node_exposer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;
    wait_for_service_reachable_or_exit(
        &health_ctx,
        CONSUMER_NODE_NAME,
        ACTION_FLOW_DONE_SERVICE,
        Some(consumer_instance_id),
        &mut consumer_child,
        &user_node_consumer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;

    send_shutdown(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        CONSUMER_NODE_NAME,
        Some(TEST_CORE_NODE),
        consumer_instance_id,
        Duration::from_secs(5),
    )
    .await;
    send_shutdown(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        BRAIN_NODE_NAME,
        Some(TEST_CORE_NODE),
        exposer_instance_id,
        Duration::from_secs(5),
    )
    .await;

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

    let consumer_stdout = String::from_utf8_lossy(&consumer_output.stdout).into_owned();
    let consumer_stderr = String::from_utf8_lossy(&consumer_output.stderr).into_owned();
    assert!(
        consumer_output.status.success(),
        "consumer process failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        consumer_output.status.code(),
        consumer_stdout,
        consumer_stderr
    );
    assert!(
        consumer_stdout.contains("goal accepted=True")
            && consumer_stdout.contains("feedback message received new_position=[7, 31, 43]")
            && consumer_stdout
                .contains("result success=True error=None final_position=[98, 4, 26]"),
        "consumer did not complete the action flow.\nstdout:\n{}\nstderr:\n{}",
        consumer_stdout,
        consumer_stderr
    );

    let exposer_stdout = String::from_utf8_lossy(&exposer_output.stdout).into_owned();
    let exposer_stderr = String::from_utf8_lossy(&exposer_output.stderr).into_owned();
    assert!(
        exposer_output.status.success(),
        "exposer process failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        exposer_output.status.code(),
        exposer_stdout,
        exposer_stderr
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

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn actions_communication_cancel_goal() {
    let instance = pmi::ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    // --- Consumer (client) project
    let consumer_instance_id = CONSUMER_INSTANCE_ID;
    let temp_dir_consumer = TempDir::new().unwrap();
    let consumed_action: ConsumedAction = serde_json5::from_str(CONSUMED_ACTION_EXAMPLE).unwrap();
    let goal_request_format: MessageFormat =
        serde_json5::from_str(CONSUMED_ACTION_GOAL_FORMAT).unwrap();
    let goal_response_format: MessageFormat =
        serde_json5::from_str(CONSUMED_ACTION_GOAL_RESPONSE_FORMAT).unwrap();
    let feedback_format: MessageFormat =
        serde_json5::from_str(CONSUMED_ACTION_FEEDBACK_FORMAT).unwrap();
    let result_response_format: MessageFormat =
        serde_json5::from_str(CONSUMED_ACTION_RESULT_FORMAT).unwrap();
    let action_messages = ConsumedActionMessage {
        goal_request: Some(goal_request_format),
        goal_response: Some(goal_response_format),
        feedback: Some(feedback_format),
        result_request: None,
        result_response: Some(result_response_format),
    };
    let (mut generator, output_dir_consumer, user_node_consumer, peppy_node_config_path) =
        init_test_env::<generator::PythonGenerator>(&temp_dir_consumer, STUB_PYTHON_NODE_CONFIG);
    let flow_done_service: ExposedService =
        serde_json5::from_str(EXPOSED_ACTION_CANCEL_FLOW_DONE_SERVICE_EXAMPLE).unwrap();
    generator
        .add_consumed_action(
            &consumed_action,
            &action_messages,
            &generator::DependencyContext::native("brain", "v1"),
        )
        .unwrap();
    generator
        .add_exposed_service(&flow_done_service, None)
        .unwrap();
    let output_config = copy_config_to_output(&user_node_consumer, &output_dir_consumer);
    generator
        .build(&output_dir_consumer, &test_peppy_dirs(), Default::default())
        .unwrap();
    fs::remove_file(output_config).unwrap();
    config::fingerprint::create_codegen_fingerprint(
        &peppy_node_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let consumer_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstanceConfig::new(Name::new(consumer_instance_id).unwrap()),
        CONSUMER_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let consumer_runtime_config_path = temp_dir_consumer.path().join("peppy_runtime.json5");
    consumer_runtime_config
        .save_json5_launch_config(&consumer_runtime_config_path)
        .unwrap();

    init_python_user_node(&user_node_consumer);
    let consumer_main = r#"
import asyncio
from peppygen import NodeBuilder, QoSProfile
from peppygen.exposed_services import move_arm_cancel_flow_done
from peppygen.consumed_actions import brain_move_arm

async def run_consumer(node_runner):
    request = brain_move_arm.GoalRequest(arm_id=7, desired_position=[10, 20, 30])
    goal = await brain_move_arm.ActionHandle.fire_goal(
        node_runner, request, 5.0, QoSProfile.SensorData
    )
    print(f"goal accepted={goal.data.accepted}", flush=True)

    cancel_response = await goal.cancel_goal(5.0)
    error_msg = cancel_response.data.error_message if cancel_response.data.error_message is not None else "<none>"
    print(
        f"cancel accepted={cancel_response.data.accepted} error={error_msg}",
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
    let main_file = user_node_consumer.join("main.py");
    fs::write(main_file, consumer_main).expect("failed to write consumer main.py");

    // --- Exposer (server) project
    let exposer_instance_id = EXPOSER_INSTANCE_ID;
    let temp_dir_exposer = TempDir::new().unwrap();
    let exposed_action: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE).unwrap();
    let (mut generator, output_dir_exposer, user_node_exposer, peppy_node_config_path) =
        init_test_env::<generator::PythonGenerator>(&temp_dir_exposer, STUB_PYTHON_NODE_CONFIG);
    generator.add_exposed_action(&exposed_action, None).unwrap();
    let output_config = copy_config_to_output(&user_node_exposer, &output_dir_exposer);
    generator
        .build(&output_dir_exposer, &test_peppy_dirs(), Default::default())
        .unwrap();
    fs::remove_file(output_config).unwrap();
    config::fingerprint::create_codegen_fingerprint(
        &peppy_node_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let exposer_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstanceConfig::new(Name::new(exposer_instance_id).unwrap()),
        BRAIN_NODE_NAME, // Must match the node name expected by the consumer
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let exposer_runtime_config_path = temp_dir_exposer.path().join("peppy_runtime.json5");
    exposer_runtime_config
        .save_json5_launch_config(&exposer_runtime_config_path)
        .unwrap();

    init_python_user_node(&user_node_exposer);
    let exposer_main = r#"
import asyncio
from peppygen import NodeBuilder
from peppygen.exposed_actions import move_arm

async def run_exposer(node_runner):
    action = await move_arm.ActionHandle.expose(node_runner)

    def decide(request):
        print(
            f"server received goal arm_id={request.data.arm_id} desired={request.data.desired_position}",
            flush=True,
        )
        return move_arm.GoalResponse(accepted=True)

    ctx = await action.handle_goal_next_request(decide)
    if ctx is None:
        return
    print("server handled goal request", flush=True)

    # Cancellation is now a signal the worker observes, not a handler.
    await ctx.cancel_signal()
    print("server saw cancel for goal", flush=True)
    await ctx.complete(success=False, error_msg="cancelled by client", final_position=[0, 0, 0])

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

    let exposer_runtime_config_str = exposer_runtime_config_path.to_str().unwrap().to_owned();
    let consumer_runtime_config_str = consumer_runtime_config_path.to_str().unwrap().to_owned();

    let messenger = peppylib::MessengerHandle::from_host_port(&router_host, router_port)
        .await
        .expect("failed to create messenger for test control");

    // Spawn exposer first so it's ready to handle requests
    let mut exposer_child = spawn_python_run(
        &user_node_exposer,
        &[(RUNTIME_CONFIG_VAR_NAME, &exposer_runtime_config_str)],
    );

    let action_ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_core_node: None,
    };
    wait_for_action_service_reachable_or_exit(
        &action_ctx,
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

    let health_ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_core_node: Some(TEST_CORE_NODE),
    };
    wait_for_health_service_reachable_or_exit(
        &health_ctx,
        CONSUMER_NODE_NAME,
        consumer_instance_id,
        &mut consumer_child,
        &user_node_consumer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;
    wait_for_health_service_reachable_or_exit(
        &health_ctx,
        BRAIN_NODE_NAME,
        exposer_instance_id,
        &mut exposer_child,
        &user_node_exposer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;
    wait_for_service_reachable_or_exit(
        &health_ctx,
        CONSUMER_NODE_NAME,
        ACTION_CANCEL_FLOW_DONE_SERVICE,
        Some(consumer_instance_id),
        &mut consumer_child,
        &user_node_consumer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;

    send_shutdown(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        CONSUMER_NODE_NAME,
        Some(TEST_CORE_NODE),
        consumer_instance_id,
        Duration::from_secs(5),
    )
    .await;
    send_shutdown(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        BRAIN_NODE_NAME,
        Some(TEST_CORE_NODE),
        exposer_instance_id,
        Duration::from_secs(5),
    )
    .await;

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

    let consumer_stdout = String::from_utf8_lossy(&consumer_output.stdout).into_owned();
    let consumer_stderr = String::from_utf8_lossy(&consumer_output.stderr).into_owned();
    assert!(
        consumer_output.status.success(),
        "consumer process failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        consumer_output.status.code(),
        consumer_stdout,
        consumer_stderr
    );
    assert!(
        consumer_stdout.contains("goal accepted=True")
            && consumer_stdout.contains("cancel accepted=True error=<none>"),
        "consumer did not complete the cancel flow.\nstdout:\n{}\nstderr:\n{}",
        consumer_stdout,
        consumer_stderr
    );

    let exposer_stdout = String::from_utf8_lossy(&exposer_output.stdout).into_owned();
    let exposer_stderr = String::from_utf8_lossy(&exposer_output.stderr).into_owned();
    assert!(
        exposer_output.status.success(),
        "exposer process failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        exposer_output.status.code(),
        exposer_stdout,
        exposer_stderr
    );
    assert!(
        exposer_stdout.contains("server handled goal request")
            && exposer_stdout.contains("server saw cancel for goal"),
        "exposer did not observe the cancel signal as expected.\nstdout:\n{}\nstderr:\n{}",
        exposer_stdout,
        exposer_stderr
    );
}

/// Verifies a worker that reacts to the cancel signal by completing, which
/// closes the feedback stream. Cancellation is an SDK-driven signal now, not a
/// user handler: the framework acks the cancel (`accepted=True` while the goal
/// is in flight) and fires `ctx.cancel_signal()`. Closing the feedback stream
/// is the worker's decision, made here by calling `ctx.complete(...)`.
///
/// End-to-end flow exercised here:
///   1. Client `fire_goal`, server accepts.
///   2. Server emits one warmup feedback message; client receives it.
///   3. Client calls `cancel_goal`; the SDK acks `accepted=True` and fires
///      the worker's `ctx.cancel_signal()`.
///   4. The worker reacts by calling `ctx.complete(...)`, which publishes the
///      end-of-stream sentinel and closes the feedback stream for this goal.
///   5. The client's next `await goal.on_next_feedback_message()` raises
///      (instead of blocking forever waiting for feedback that will never
///      come). That raise is what this test asserts.
///
/// The "keep streaming after cancel" variant is covered by
/// `actions_communication_cancel_reject_keeps_feedback_open`.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn actions_communication_cancel_accept_closes_feedback_stream() {
    let instance = pmi::ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    // --- Consumer (client) project
    let consumer_instance_id = CONSUMER_INSTANCE_ID;
    let temp_dir_consumer = TempDir::new().unwrap();
    let consumed_action: ConsumedAction = serde_json5::from_str(CONSUMED_ACTION_EXAMPLE).unwrap();
    let goal_request_format: MessageFormat =
        serde_json5::from_str(CONSUMED_ACTION_GOAL_FORMAT).unwrap();
    let goal_response_format: MessageFormat =
        serde_json5::from_str(CONSUMED_ACTION_GOAL_RESPONSE_FORMAT).unwrap();
    let feedback_format: MessageFormat =
        serde_json5::from_str(CONSUMED_ACTION_FEEDBACK_FORMAT).unwrap();
    let result_response_format: MessageFormat =
        serde_json5::from_str(CONSUMED_ACTION_RESULT_FORMAT).unwrap();
    let action_messages = ConsumedActionMessage {
        goal_request: Some(goal_request_format),
        goal_response: Some(goal_response_format),
        feedback: Some(feedback_format),
        result_request: None,
        result_response: Some(result_response_format),
    };
    let (mut generator, output_dir_consumer, user_node_consumer, peppy_node_config_path) =
        init_test_env::<generator::PythonGenerator>(&temp_dir_consumer, STUB_PYTHON_NODE_CONFIG);
    let flow_done_service: ExposedService =
        serde_json5::from_str(EXPOSED_ACTION_CANCEL_ACCEPT_FLOW_DONE_SERVICE_EXAMPLE).unwrap();
    generator
        .add_consumed_action(
            &consumed_action,
            &action_messages,
            &generator::DependencyContext::native("brain", "v1"),
        )
        .unwrap();
    generator
        .add_exposed_service(&flow_done_service, None)
        .unwrap();
    let output_config = copy_config_to_output(&user_node_consumer, &output_dir_consumer);
    generator
        .build(&output_dir_consumer, &test_peppy_dirs(), Default::default())
        .unwrap();
    fs::remove_file(output_config).unwrap();
    config::fingerprint::create_codegen_fingerprint(
        &peppy_node_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let consumer_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstanceConfig::new(Name::new(consumer_instance_id).unwrap()),
        CONSUMER_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let consumer_runtime_config_path = temp_dir_consumer.path().join("peppy_runtime.json5");
    consumer_runtime_config
        .save_json5_launch_config(&consumer_runtime_config_path)
        .unwrap();

    init_python_user_node(&user_node_consumer);
    let consumer_main = r#"
import asyncio
from peppygen import NodeBuilder, QoSProfile
from peppygen.exposed_services import move_arm_cancel_accept_flow_done
from peppygen.consumed_actions import brain_move_arm

async def run_consumer(node_runner):
    request = brain_move_arm.GoalRequest(arm_id=7, desired_position=[10, 20, 30])
    goal = await brain_move_arm.ActionHandle.fire_goal(
        node_runner, request, 5.0, QoSProfile.SensorData
    )
    print(f"goal accepted={goal.data.accepted}", flush=True)

    warmup = await goal.on_next_feedback_message()
    print(f"warmup feedback new_position={warmup.new_position}", flush=True)

    cancel_response = await goal.cancel_goal(5.0)
    print(f"cancel accepted={cancel_response.data.accepted}", flush=True)

    # Cancel was accepted — codegen publishes end-of-stream sentinel.
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
    let main_file = user_node_consumer.join("main.py");
    fs::write(main_file, consumer_main).expect("failed to write consumer main.py");

    // --- Exposer (server) project
    let exposer_instance_id = EXPOSER_INSTANCE_ID;
    let temp_dir_exposer = TempDir::new().unwrap();
    let exposed_action: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE).unwrap();
    let (mut generator, output_dir_exposer, user_node_exposer, peppy_node_config_path) =
        init_test_env::<generator::PythonGenerator>(&temp_dir_exposer, STUB_PYTHON_NODE_CONFIG);
    generator.add_exposed_action(&exposed_action, None).unwrap();
    let output_config = copy_config_to_output(&user_node_exposer, &output_dir_exposer);
    generator
        .build(&output_dir_exposer, &test_peppy_dirs(), Default::default())
        .unwrap();
    fs::remove_file(output_config).unwrap();
    config::fingerprint::create_codegen_fingerprint(
        &peppy_node_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let exposer_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstanceConfig::new(Name::new(exposer_instance_id).unwrap()),
        BRAIN_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let exposer_runtime_config_path = temp_dir_exposer.path().join("peppy_runtime.json5");
    exposer_runtime_config
        .save_json5_launch_config(&exposer_runtime_config_path)
        .unwrap();

    init_python_user_node(&user_node_exposer);
    let exposer_main = r#"
import asyncio
from peppygen import NodeBuilder
from peppygen.exposed_actions import move_arm

async def run_exposer(node_runner):
    action = await move_arm.ActionHandle.expose(node_runner)

    def decide(_request):
        return move_arm.GoalResponse(accepted=True)

    ctx = await action.handle_goal_next_request(decide)
    if ctx is None:
        return
    print("server accepted goal", flush=True)

    await ctx.publish_feedback([1, 2, 3])
    print("server emitted warmup feedback", flush=True)

    # The worker reacts to the cancel signal by completing, which closes the
    # feedback stream. Cancellation no longer auto-closes it.
    await ctx.cancel_signal()
    print("server saw cancel - completing closes the feedback stream", flush=True)
    await ctx.complete(success=False, error_msg="cancelled", final_position=[0, 0, 0])

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

    let exposer_runtime_config_str = exposer_runtime_config_path.to_str().unwrap().to_owned();
    let consumer_runtime_config_str = consumer_runtime_config_path.to_str().unwrap().to_owned();

    let messenger = peppylib::MessengerHandle::from_host_port(&router_host, router_port)
        .await
        .expect("failed to create messenger for test control");

    let mut exposer_child = spawn_python_run(
        &user_node_exposer,
        &[(RUNTIME_CONFIG_VAR_NAME, &exposer_runtime_config_str)],
    );

    let action_ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_core_node: None,
    };
    wait_for_action_service_reachable_or_exit(
        &action_ctx,
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

    let health_ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_core_node: Some(TEST_CORE_NODE),
    };
    wait_for_health_service_reachable_or_exit(
        &health_ctx,
        CONSUMER_NODE_NAME,
        consumer_instance_id,
        &mut consumer_child,
        &user_node_consumer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;
    wait_for_health_service_reachable_or_exit(
        &health_ctx,
        BRAIN_NODE_NAME,
        exposer_instance_id,
        &mut exposer_child,
        &user_node_exposer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;
    wait_for_service_reachable_or_exit(
        &health_ctx,
        CONSUMER_NODE_NAME,
        ACTION_CANCEL_ACCEPT_FLOW_DONE_SERVICE,
        Some(consumer_instance_id),
        &mut consumer_child,
        &user_node_consumer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;

    send_shutdown(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        CONSUMER_NODE_NAME,
        Some(TEST_CORE_NODE),
        consumer_instance_id,
        Duration::from_secs(5),
    )
    .await;
    send_shutdown(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        BRAIN_NODE_NAME,
        Some(TEST_CORE_NODE),
        exposer_instance_id,
        Duration::from_secs(5),
    )
    .await;

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

    let consumer_stdout = String::from_utf8_lossy(&consumer_output.stdout).into_owned();
    let consumer_stderr = String::from_utf8_lossy(&consumer_output.stderr).into_owned();
    assert!(
        consumer_output.status.success(),
        "consumer process failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        consumer_output.status.code(),
        consumer_stdout,
        consumer_stderr
    );
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

    let exposer_stdout = String::from_utf8_lossy(&exposer_output.stdout).into_owned();
    let exposer_stderr = String::from_utf8_lossy(&exposer_output.stderr).into_owned();
    assert!(
        exposer_output.status.success(),
        "exposer process failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        exposer_output.status.code(),
        exposer_stdout,
        exposer_stderr
    );
    assert!(
        exposer_stdout.contains("server saw cancel - completing closes the feedback stream"),
        "exposer did not exercise the cancel-accept path.\nstdout:\n{}",
        exposer_stdout
    );
}

/// Verifies that the cancel signal does NOT close the feedback stream: the
/// worker is free to keep publishing feedback after observing the cancel and
/// to close the stream only when it calls `ctx.complete(...)`. This proves the
/// framework no longer auto-closes the stream on cancel (a cancel for one goal
/// must never close another goal's stream).
///
/// End-to-end flow exercised here:
///   1. Client `fire_goal`, server accepts.
///   2. Server emits pre-cancel feedback; client receives it.
///   3. Client calls `cancel_goal`; the SDK acks `accepted=True` and fires
///      the worker's `ctx.cancel_signal()`.
///   4. The worker chooses to keep going and emits post-cancel feedback; the
///      client still receives it. This is what proves the cancel signal did
///      not close the stream.
///   5. The worker calls `ctx.complete(...)`, which publishes the
///      end-of-stream sentinel as part of normal goal completion.
///   6. The client's next `await goal.on_next_feedback_message()` raises,
///      confirming the stream is closed by `complete` (not by the cancel).
///
/// The "complete immediately on cancel" variant is covered by
/// `actions_communication_cancel_accept_closes_feedback_stream`.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn actions_communication_cancel_reject_keeps_feedback_open() {
    let instance = pmi::ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    // --- Consumer (client) project
    let consumer_instance_id = CONSUMER_INSTANCE_ID;
    let temp_dir_consumer = TempDir::new().unwrap();
    let consumed_action: ConsumedAction = serde_json5::from_str(CONSUMED_ACTION_EXAMPLE).unwrap();
    let goal_request_format: MessageFormat =
        serde_json5::from_str(CONSUMED_ACTION_GOAL_FORMAT).unwrap();
    let goal_response_format: MessageFormat =
        serde_json5::from_str(CONSUMED_ACTION_GOAL_RESPONSE_FORMAT).unwrap();
    let feedback_format: MessageFormat =
        serde_json5::from_str(CONSUMED_ACTION_FEEDBACK_FORMAT).unwrap();
    let result_response_format: MessageFormat =
        serde_json5::from_str(CONSUMED_ACTION_RESULT_FORMAT).unwrap();
    let action_messages = ConsumedActionMessage {
        goal_request: Some(goal_request_format),
        goal_response: Some(goal_response_format),
        feedback: Some(feedback_format),
        result_request: None,
        result_response: Some(result_response_format),
    };
    let (mut generator, output_dir_consumer, user_node_consumer, peppy_node_config_path) =
        init_test_env::<generator::PythonGenerator>(&temp_dir_consumer, STUB_PYTHON_NODE_CONFIG);
    let flow_done_service: ExposedService =
        serde_json5::from_str(EXPOSED_ACTION_CANCEL_REJECT_FLOW_DONE_SERVICE_EXAMPLE).unwrap();
    generator
        .add_consumed_action(
            &consumed_action,
            &action_messages,
            &generator::DependencyContext::native("brain", "v1"),
        )
        .unwrap();
    generator
        .add_exposed_service(&flow_done_service, None)
        .unwrap();
    let output_config = copy_config_to_output(&user_node_consumer, &output_dir_consumer);
    generator
        .build(&output_dir_consumer, &test_peppy_dirs(), Default::default())
        .unwrap();
    fs::remove_file(output_config).unwrap();
    config::fingerprint::create_codegen_fingerprint(
        &peppy_node_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let consumer_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstanceConfig::new(Name::new(consumer_instance_id).unwrap()),
        CONSUMER_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let consumer_runtime_config_path = temp_dir_consumer.path().join("peppy_runtime.json5");
    consumer_runtime_config
        .save_json5_launch_config(&consumer_runtime_config_path)
        .unwrap();

    init_python_user_node(&user_node_consumer);
    let consumer_main = r#"
import asyncio
from peppygen import NodeBuilder, QoSProfile
from peppygen.exposed_services import move_arm_cancel_reject_flow_done
from peppygen.consumed_actions import brain_move_arm

async def run_consumer(node_runner):
    request = brain_move_arm.GoalRequest(arm_id=7, desired_position=[10, 20, 30])
    goal = await brain_move_arm.ActionHandle.fire_goal(
        node_runner, request, 5.0, QoSProfile.SensorData
    )
    print(f"goal accepted={goal.data.accepted}", flush=True)

    pre_cancel = await goal.on_next_feedback_message()
    print(f"pre_cancel feedback new_position={pre_cancel.new_position}", flush=True)

    cancel_response = await goal.cancel_goal(5.0)
    err = cancel_response.data.error_message if cancel_response.data.error_message else "<none>"
    print(
        f"cancel accepted={cancel_response.data.accepted} error={err}",
        flush=True,
    )

    # CRITICAL: feedback after cancel-reject must still arrive.
    post_cancel = await goal.on_next_feedback_message()
    print(f"post_cancel feedback new_position={post_cancel.new_position}", flush=True)

    result = await goal.get_result(5.0)
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
    let main_file = user_node_consumer.join("main.py");
    fs::write(main_file, consumer_main).expect("failed to write consumer main.py");

    // --- Exposer (server) project
    let exposer_instance_id = EXPOSER_INSTANCE_ID;
    let temp_dir_exposer = TempDir::new().unwrap();
    let exposed_action: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE).unwrap();
    let (mut generator, output_dir_exposer, user_node_exposer, peppy_node_config_path) =
        init_test_env::<generator::PythonGenerator>(&temp_dir_exposer, STUB_PYTHON_NODE_CONFIG);
    generator.add_exposed_action(&exposed_action, None).unwrap();
    let output_config = copy_config_to_output(&user_node_exposer, &output_dir_exposer);
    generator
        .build(&output_dir_exposer, &test_peppy_dirs(), Default::default())
        .unwrap();
    fs::remove_file(output_config).unwrap();
    config::fingerprint::create_codegen_fingerprint(
        &peppy_node_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let exposer_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstanceConfig::new(Name::new(exposer_instance_id).unwrap()),
        BRAIN_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let exposer_runtime_config_path = temp_dir_exposer.path().join("peppy_runtime.json5");
    exposer_runtime_config
        .save_json5_launch_config(&exposer_runtime_config_path)
        .unwrap();

    init_python_user_node(&user_node_exposer);
    let exposer_main = r#"
import asyncio
from peppygen import NodeBuilder
from peppygen.exposed_actions import move_arm

async def run_exposer(node_runner):
    action = await move_arm.ActionHandle.expose(node_runner)

    def decide(_request):
        return move_arm.GoalResponse(accepted=True)

    # Accept loop: a real server keeps running so its results stay fetchable.
    while True:
        ctx = await action.handle_goal_next_request(decide)
        if ctx is None:
            break

        await ctx.publish_feedback([1, 1, 1])
        print("server emitted pre-cancel feedback", flush=True)

        # The cancel signal is advisory: the worker chooses to keep streaming
        # feedback rather than complete immediately. This proves the cancel does
        # not auto-close the feedback stream.
        await ctx.cancel_signal()
        print("server saw cancel but keeps streaming", flush=True)

        await ctx.publish_feedback([2, 2, 2])
        print("server emitted post-cancel feedback", flush=True)

        await ctx.complete(success=True, error_msg=None, final_position=[9, 9, 9])
        print("server handled result request", flush=True)

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

    let exposer_runtime_config_str = exposer_runtime_config_path.to_str().unwrap().to_owned();
    let consumer_runtime_config_str = consumer_runtime_config_path.to_str().unwrap().to_owned();

    let messenger = peppylib::MessengerHandle::from_host_port(&router_host, router_port)
        .await
        .expect("failed to create messenger for test control");

    let mut exposer_child = spawn_python_run(
        &user_node_exposer,
        &[(RUNTIME_CONFIG_VAR_NAME, &exposer_runtime_config_str)],
    );

    let action_ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_core_node: None,
    };
    wait_for_action_service_reachable_or_exit(
        &action_ctx,
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

    let health_ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_core_node: Some(TEST_CORE_NODE),
    };
    wait_for_health_service_reachable_or_exit(
        &health_ctx,
        CONSUMER_NODE_NAME,
        consumer_instance_id,
        &mut consumer_child,
        &user_node_consumer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;
    wait_for_health_service_reachable_or_exit(
        &health_ctx,
        BRAIN_NODE_NAME,
        exposer_instance_id,
        &mut exposer_child,
        &user_node_exposer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;
    wait_for_service_reachable_or_exit(
        &health_ctx,
        CONSUMER_NODE_NAME,
        ACTION_CANCEL_REJECT_FLOW_DONE_SERVICE,
        Some(consumer_instance_id),
        &mut consumer_child,
        &user_node_consumer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;

    send_shutdown(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        CONSUMER_NODE_NAME,
        Some(TEST_CORE_NODE),
        consumer_instance_id,
        Duration::from_secs(5),
    )
    .await;
    send_shutdown(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        BRAIN_NODE_NAME,
        Some(TEST_CORE_NODE),
        exposer_instance_id,
        Duration::from_secs(5),
    )
    .await;

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

    let consumer_stdout = String::from_utf8_lossy(&consumer_output.stdout).into_owned();
    let consumer_stderr = String::from_utf8_lossy(&consumer_output.stderr).into_owned();
    assert!(
        consumer_output.status.success(),
        "consumer process failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        consumer_output.status.code(),
        consumer_stdout,
        consumer_stderr
    );
    assert!(
        consumer_stdout.contains("pre_cancel feedback new_position=[1, 1, 1]"),
        "consumer did not receive pre-cancel feedback.\nstdout:\n{}",
        consumer_stdout
    );
    assert!(
        consumer_stdout.contains("cancel accepted=True"),
        "consumer did not see the SDK-acked cancel.\nstdout:\n{}",
        consumer_stdout
    );
    assert!(
        consumer_stdout.contains("post_cancel feedback new_position=[2, 2, 2]"),
        "consumer did not receive post-cancel feedback — the cancel signal must NOT close the stream.\nstdout:\n{}",
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

    let exposer_stdout = String::from_utf8_lossy(&exposer_output.stdout).into_owned();
    let exposer_stderr = String::from_utf8_lossy(&exposer_output.stderr).into_owned();
    assert!(
        exposer_output.status.success(),
        "exposer process failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        exposer_output.status.code(),
        exposer_stdout,
        exposer_stderr
    );
    assert!(
        exposer_stdout.contains("server emitted pre-cancel feedback")
            && exposer_stdout.contains("server saw cancel but keeps streaming")
            && exposer_stdout.contains("server emitted post-cancel feedback")
            && exposer_stdout.contains("server handled result request"),
        "exposer did not exercise the cancel-keeps-streaming path.\nstdout:\n{}",
        exposer_stdout
    );
}

/// Verifies the goal-completion side of the action lifecycle contract: a
/// client can use a drain-loop pattern (`while True: await
/// on_next_feedback_message()`) to consume every feedback message and
/// reliably exit once the goal is complete. This works because the Python
/// codegen for `handle_result_next_request` publishes an end-of-stream
/// sentinel (an empty payload on the per-goal feedback publisher) before
/// invoking the user's result handler. Without that sentinel the loop
/// would hang forever, because the underlying mpsc receiver never
/// surfaces an end-of-stream condition on its own.
///
/// End-to-end flow exercised here:
///   1. Client `fire_goal`, server accepts.
///   2. Server emits 3 feedback messages.
///   3. Client's drain-loop receives all 3 in order.
///   4. Server calls `handle_result_next_request`. Before invoking the
///      user's result handler, codegen publishes the end-of-stream
///      sentinel on the per-goal feedback publisher (closing the
///      feedback stream); then it runs the handler and returns the
///      result.
///   5. Client's next `on_next_feedback_message()` raises (sentinel
///      observed). The drain-loop catches the exception and exits.
///   6. Client calls `get_result` and receives the final response.
///
/// Rust parity is `actions_communication_drain_loop_until_end_signal` in
/// the rust/ test module.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn actions_communication_drain_loop_until_end_signal() {
    let instance = pmi::ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    // --- Consumer (client) project
    let consumer_instance_id = CONSUMER_INSTANCE_ID;
    let temp_dir_consumer = TempDir::new().unwrap();
    let consumed_action: ConsumedAction = serde_json5::from_str(CONSUMED_ACTION_EXAMPLE).unwrap();
    let goal_request_format: MessageFormat =
        serde_json5::from_str(CONSUMED_ACTION_GOAL_FORMAT).unwrap();
    let goal_response_format: MessageFormat =
        serde_json5::from_str(CONSUMED_ACTION_GOAL_RESPONSE_FORMAT).unwrap();
    let feedback_format: MessageFormat =
        serde_json5::from_str(CONSUMED_ACTION_FEEDBACK_FORMAT).unwrap();
    let result_response_format: MessageFormat =
        serde_json5::from_str(CONSUMED_ACTION_RESULT_FORMAT).unwrap();
    let action_messages = ConsumedActionMessage {
        goal_request: Some(goal_request_format),
        goal_response: Some(goal_response_format),
        feedback: Some(feedback_format),
        result_request: None,
        result_response: Some(result_response_format),
    };
    let (mut generator, output_dir_consumer, user_node_consumer, peppy_node_config_path) =
        init_test_env::<generator::PythonGenerator>(&temp_dir_consumer, STUB_PYTHON_NODE_CONFIG);
    let flow_done_service: ExposedService =
        serde_json5::from_str(EXPOSED_ACTION_DRAIN_LOOP_FLOW_DONE_SERVICE_EXAMPLE).unwrap();
    generator
        .add_consumed_action(
            &consumed_action,
            &action_messages,
            &generator::DependencyContext::native("brain", "v1"),
        )
        .unwrap();
    generator
        .add_exposed_service(&flow_done_service, None)
        .unwrap();
    let output_config = copy_config_to_output(&user_node_consumer, &output_dir_consumer);
    generator
        .build(&output_dir_consumer, &test_peppy_dirs(), Default::default())
        .unwrap();
    fs::remove_file(output_config).unwrap();
    config::fingerprint::create_codegen_fingerprint(
        &peppy_node_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let consumer_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstanceConfig::new(Name::new(consumer_instance_id).unwrap()),
        CONSUMER_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let consumer_runtime_config_path = temp_dir_consumer.path().join("peppy_runtime.json5");
    consumer_runtime_config
        .save_json5_launch_config(&consumer_runtime_config_path)
        .unwrap();

    init_python_user_node(&user_node_consumer);
    let consumer_main = r#"
import asyncio
from peppygen import NodeBuilder, QoSProfile
from peppygen.exposed_services import move_arm_drain_loop_flow_done
from peppygen.consumed_actions import brain_move_arm

async def run_consumer(node_runner):
    request = brain_move_arm.GoalRequest(arm_id=7, desired_position=[10, 20, 30])
    goal = await brain_move_arm.ActionHandle.fire_goal(
        node_runner, request, 5.0, QoSProfile.SensorData
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
    let main_file = user_node_consumer.join("main.py");
    fs::write(main_file, consumer_main).expect("failed to write consumer main.py");

    // --- Exposer (server) project
    let exposer_instance_id = EXPOSER_INSTANCE_ID;
    let temp_dir_exposer = TempDir::new().unwrap();
    let exposed_action: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE).unwrap();
    let (mut generator, output_dir_exposer, user_node_exposer, peppy_node_config_path) =
        init_test_env::<generator::PythonGenerator>(&temp_dir_exposer, STUB_PYTHON_NODE_CONFIG);
    generator.add_exposed_action(&exposed_action, None).unwrap();
    let output_config = copy_config_to_output(&user_node_exposer, &output_dir_exposer);
    generator
        .build(&output_dir_exposer, &test_peppy_dirs(), Default::default())
        .unwrap();
    fs::remove_file(output_config).unwrap();
    config::fingerprint::create_codegen_fingerprint(
        &peppy_node_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let exposer_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstanceConfig::new(Name::new(exposer_instance_id).unwrap()),
        BRAIN_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let exposer_runtime_config_path = temp_dir_exposer.path().join("peppy_runtime.json5");
    exposer_runtime_config
        .save_json5_launch_config(&exposer_runtime_config_path)
        .unwrap();

    init_python_user_node(&user_node_exposer);
    let exposer_main = r#"
import asyncio
from peppygen import NodeBuilder
from peppygen.exposed_actions import move_arm

async def run_exposer(node_runner):
    action = await move_arm.ActionHandle.expose(node_runner)

    def decide(_request):
        return move_arm.GoalResponse(accepted=True)

    # Accept loop: a real server keeps running so its results stay fetchable.
    while True:
        ctx = await action.handle_goal_next_request(decide)
        if ctx is None:
            break
        print("server accepted goal", flush=True)

        for i in range(3):
            pos = [i, i + 1, i + 2]
            await ctx.publish_feedback(pos)
            print(f"server emitted feedback #{i + 1} position={pos}", flush=True)

        final_position = [99, 99, 99]
        # complete closes the feedback stream (end-of-stream sentinel) before
        # delivering the result, so the client's drain loop exits.
        await ctx.complete(success=True, error_msg=None, final_position=final_position)
        print(f"server handled result request final_position={final_position}", flush=True)

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

    let exposer_runtime_config_str = exposer_runtime_config_path.to_str().unwrap().to_owned();
    let consumer_runtime_config_str = consumer_runtime_config_path.to_str().unwrap().to_owned();

    let messenger = peppylib::MessengerHandle::from_host_port(&router_host, router_port)
        .await
        .expect("failed to create messenger for test control");

    let mut exposer_child = spawn_python_run(
        &user_node_exposer,
        &[(RUNTIME_CONFIG_VAR_NAME, &exposer_runtime_config_str)],
    );

    let action_ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_core_node: None,
    };
    wait_for_action_service_reachable_or_exit(
        &action_ctx,
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

    let health_ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_core_node: Some(TEST_CORE_NODE),
    };
    wait_for_health_service_reachable_or_exit(
        &health_ctx,
        CONSUMER_NODE_NAME,
        consumer_instance_id,
        &mut consumer_child,
        &user_node_consumer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;
    wait_for_health_service_reachable_or_exit(
        &health_ctx,
        BRAIN_NODE_NAME,
        exposer_instance_id,
        &mut exposer_child,
        &user_node_exposer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;
    wait_for_service_reachable_or_exit(
        &health_ctx,
        CONSUMER_NODE_NAME,
        ACTION_DRAIN_LOOP_FLOW_DONE_SERVICE,
        Some(consumer_instance_id),
        &mut consumer_child,
        &user_node_consumer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;

    send_shutdown(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        CONSUMER_NODE_NAME,
        Some(TEST_CORE_NODE),
        consumer_instance_id,
        Duration::from_secs(5),
    )
    .await;
    send_shutdown(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        BRAIN_NODE_NAME,
        Some(TEST_CORE_NODE),
        exposer_instance_id,
        Duration::from_secs(5),
    )
    .await;

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

    let consumer_stdout = String::from_utf8_lossy(&consumer_output.stdout).into_owned();
    let consumer_stderr = String::from_utf8_lossy(&consumer_output.stderr).into_owned();
    assert!(
        consumer_output.status.success(),
        "consumer process failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        consumer_output.status.code(),
        consumer_stdout,
        consumer_stderr
    );
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

    let exposer_stdout = String::from_utf8_lossy(&exposer_output.stdout).into_owned();
    let exposer_stderr = String::from_utf8_lossy(&exposer_output.stderr).into_owned();
    assert!(
        exposer_output.status.success(),
        "exposer process failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        exposer_output.status.code(),
        exposer_stdout,
        exposer_stderr
    );
    assert!(
        exposer_stdout.contains("server emitted feedback #3 position=[2, 3, 4]")
            && exposer_stdout.contains("server handled result request"),
        "exposer did not complete the action lifecycle.\nstdout:\n{}",
        exposer_stdout
    );
}
