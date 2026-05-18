use crate::helpers::{
    STUB_NODE_CONFIG, WaitContext, compile_project, copy_config_to_output, init_cargo_user_node,
    init_test_env, send_shutdown, spawn_cargo_run, test_peppy_dirs,
    wait_for_action_service_reachable_or_exit, wait_for_child,
    wait_for_health_service_reachable_or_exit,
};
use config::consts::{PEPPYGEN_OUTPUT_PATH, RUNTIME_CONFIG_VAR_NAME};
use config::runtime::NodeInstanceConfig;
use config::{
    launcher::Name,
    node::{ConsumedAction, ExposedAction, MessageFormat},
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
  local_node_id: "brain",
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
        init_test_env::<generator::RustGenerator>(&temp_dir_consumer, STUB_NODE_CONFIG);
    generator
        .add_consumed_action(
            &consumed_action,
            &action_messages,
            &generator::DependencyContext::native("brain", "v1"),
        )
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
        NodeInstanceConfig {
            instance_id: Name::new(consumer_instance_id).unwrap(),
            arguments: Default::default(),
            framework: Default::default(),
        },
        CONSUMER_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let consumer_runtime_config_path = temp_dir_consumer.path().join("peppy_runtime.json5");
    consumer_runtime_config
        .save_json5_launch_config(&consumer_runtime_config_path)
        .unwrap();

    init_cargo_user_node(&user_node_consumer);
    let consumer_main = r#"
use peppygen::consumed_actions::brain_move_arm;
use peppygen::NodeBuilder;
use peppygen::Result;
use std::time::Duration;

async fn consume_action(node_runner: &peppygen::NodeRunner) -> Result<()> {
    let request = brain_move_arm::GoalRequest {
        arm_id: 7,
        desired_position: [10, 20, 30],
    };
    let mut action_handle = brain_move_arm::ActionHandle::fire_goal(
        &node_runner,
        Duration::from_secs(5),
        None,
        None,
        request,
        peppygen::QoSProfile::SensorData,
    ).await?;
    println!("goal accepted={}", action_handle.data.accepted);

    let feedback = action_handle.on_next_feedback_message().await?;
    assert_eq!(feedback.new_position, [7, 31, 43], "unexpected feedback message");
    println!("feedback message received new_position={:?}", feedback.new_position);

    let result = action_handle.get_result(Duration::from_secs(5)).await?;
    println!(
        "result success={} error={:?} final_position={:?}",
        result.data.success,
        result.data.error_msg.as_deref(),
        result.data.final_position
    );

    Ok(())
}

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        consume_action(&node_runner).await
    })
}
"#;
    let main_file = user_node_consumer.join("src").join("main.rs");
    fs::write(main_file, consumer_main).expect("failed to write consumer main");

    // --- Exposer (server) project
    let exposer_instance_id = EXPOSER_INSTANCE_ID;
    let temp_dir_exposer = TempDir::new().unwrap();
    let exposed_action: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE).unwrap();
    let (mut generator, output_dir_exposer, user_node_exposer, peppy_node_config_path) =
        init_test_env::<generator::RustGenerator>(&temp_dir_exposer, STUB_NODE_CONFIG);
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
        NodeInstanceConfig {
            instance_id: Name::new(exposer_instance_id).unwrap(),
            arguments: Default::default(),
            framework: Default::default(),
        },
        BRAIN_NODE_NAME, // Must match the node name expected by the consumer
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let exposer_runtime_config_path = temp_dir_exposer.path().join("peppy_runtime.json5");
    exposer_runtime_config
        .save_json5_launch_config(&exposer_runtime_config_path)
        .unwrap();

    init_cargo_user_node(&user_node_exposer);
    let exposer_main = r#"
use peppygen::exposed_actions::move_arm;
use peppygen::NodeBuilder;
use peppygen::Result;

async fn expose_action(node_runner: &peppygen::NodeRunner) -> Result<()> {
    let mut action = move_arm::ActionHandle::expose(&node_runner).await?;

    action.handle_goal_next_request(|request| -> Result<move_arm::GoalResponse> {
        println!(
            "server received goal arm_id={} desired={:?}",
            request.data.arm_id,
            request.data.desired_position
        );
        Ok(move_arm::GoalResponse::new(true))
    })
    .await?;

    let feedback_message = [7, 31, 43];
    action.emit_feedback(feedback_message).await?;
    println!("server emitted feedback message {:?}", feedback_message);

    let final_position = [98, 4, 26];
    action.handle_result_next_request(|_request| -> Result<move_arm::ResultResponse> {
        println!("server preparing action result");
        let final_pos = final_position.clone();
        Ok(move_arm::ResultResponse::new(
            true,
            None,
            final_pos,
        ))
    })
    .await?;

    println!("server handled result request. Final position sent: {:?}", &final_position);

    Ok(())
}

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        expose_action(&node_runner).await
    })
}
"#;
    let main_file = user_node_exposer.join("src").join("main.rs");
    fs::write(main_file, exposer_main).expect("failed to write exposer main");

    compile_project(&user_node_consumer);
    compile_project(&user_node_exposer);

    let exposer_runtime_config_str = exposer_runtime_config_path.to_str().unwrap().to_owned();
    let consumer_runtime_config_str = consumer_runtime_config_path.to_str().unwrap().to_owned();

    let messenger = peppylib::MessengerHandle::from_host_port(&router_host, router_port)
        .await
        .expect("failed to create messenger for test control");

    // Spawn exposer first so it's ready to handle requests
    let mut exposer_child = spawn_cargo_run(
        &user_node_exposer,
        &[(RUNTIME_CONFIG_VAR_NAME, &exposer_runtime_config_str)],
    );

    let action_ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        to_core_node: None,
    };
    wait_for_action_service_reachable_or_exit(
        &action_ctx,
        BRAIN_NODE_NAME,
        "move_arm",
        None,
        &mut exposer_child,
        &user_node_exposer,
    )
    .await;

    let mut consumer_child = spawn_cargo_run(
        &user_node_consumer,
        &[(RUNTIME_CONFIG_VAR_NAME, &consumer_runtime_config_str)],
    );

    let ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        to_core_node: Some(TEST_CORE_NODE),
    };
    wait_for_health_service_reachable_or_exit(
        &ctx,
        CONSUMER_NODE_NAME,
        consumer_instance_id,
        &mut consumer_child,
        &user_node_consumer,
    )
    .await;
    wait_for_health_service_reachable_or_exit(
        &ctx,
        BRAIN_NODE_NAME,
        exposer_instance_id,
        &mut exposer_child,
        &user_node_exposer,
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

    // Wait for both processes to exit
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
        "consumer cargo run failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        consumer_output.status.code(),
        consumer_stdout,
        consumer_stderr
    );
    assert!(
        consumer_stdout.contains("goal accepted=true")
            && consumer_stdout.contains("feedback message received new_position=[7, 31, 43]")
            && consumer_stdout
                .contains("result success=true error=None final_position=[98, 4, 26]"),
        "consumer did not complete the action flow.\nstdout:\n{}\nstderr:\n{}",
        consumer_stdout,
        consumer_stderr
    );

    let exposer_stdout = String::from_utf8_lossy(&exposer_output.stdout).into_owned();
    let exposer_stderr = String::from_utf8_lossy(&exposer_output.stderr).into_owned();
    assert!(
        exposer_output.status.success(),
        "exposer cargo run failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
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
        init_test_env::<generator::RustGenerator>(&temp_dir_consumer, STUB_NODE_CONFIG);
    generator
        .add_consumed_action(
            &consumed_action,
            &action_messages,
            &generator::DependencyContext::native("brain", "v1"),
        )
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
        NodeInstanceConfig {
            instance_id: Name::new(consumer_instance_id).unwrap(),
            arguments: Default::default(),
            framework: Default::default(),
        },
        CONSUMER_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let consumer_runtime_config_path = temp_dir_consumer.path().join("peppy_runtime.json5");
    consumer_runtime_config
        .save_json5_launch_config(&consumer_runtime_config_path)
        .unwrap();

    init_cargo_user_node(&user_node_consumer);
    let consumer_main = r#"
use peppygen::consumed_actions::brain_move_arm;
use peppygen::NodeBuilder;
use peppygen::Result;
use std::time::Duration;

async fn consume_action(node_runner: &peppygen::NodeRunner) -> Result<()> {
    let request = brain_move_arm::GoalRequest {
        arm_id: 7,
        desired_position: [10, 20, 30],
    };
    let action_handle = brain_move_arm::ActionHandle::fire_goal(
        &node_runner,
        Duration::from_secs(5),
        None,
        None,
        request,
        peppygen::QoSProfile::SensorData,
    ).await?;
    println!("goal accepted={}", action_handle.data.accepted);

    let cancel_response = action_handle.cancel_goal(Duration::from_secs(5)).await?;
    let error_msg = cancel_response.data.error_message.as_deref().unwrap_or("<none>");
    println!(
        "cancel accepted={} error={}",
        cancel_response.data.accepted,
        error_msg
    );

    Ok(())
}

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        consume_action(&node_runner).await
    })
}
"#;
    let main_file = user_node_consumer.join("src").join("main.rs");
    fs::write(main_file, consumer_main).expect("failed to write consumer main");

    // --- Exposer (server) project
    let exposer_instance_id = EXPOSER_INSTANCE_ID;
    let temp_dir_exposer = TempDir::new().unwrap();
    let exposed_action: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE).unwrap();
    let (mut generator, output_dir_exposer, user_node_exposer, peppy_node_config_path) =
        init_test_env::<generator::RustGenerator>(&temp_dir_exposer, STUB_NODE_CONFIG);
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
        NodeInstanceConfig {
            instance_id: Name::new(exposer_instance_id).unwrap(),
            arguments: Default::default(),
            framework: Default::default(),
        },
        BRAIN_NODE_NAME, // Must match the node name expected by the consumer
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let exposer_runtime_config_path = temp_dir_exposer.path().join("peppy_runtime.json5");
    exposer_runtime_config
        .save_json5_launch_config(&exposer_runtime_config_path)
        .unwrap();

    init_cargo_user_node(&user_node_exposer);
    let exposer_main = r#"
use peppygen::exposed_actions::move_arm;
use peppygen::NodeBuilder;
use peppygen::Result;

async fn expose_action(node_runner: &peppygen::NodeRunner) -> Result<()> {
    let mut action = move_arm::ActionHandle::expose(&node_runner).await?;

    action.handle_goal_next_request(|request| -> Result<move_arm::GoalResponse> {
        println!(
            "server received goal arm_id={} desired={:?}",
            request.data.arm_id,
            request.data.desired_position
        );
        Ok(move_arm::GoalResponse::new(true))
    })
    .await?;
    println!("server handled goal request");

    let cancel_error = "goal cancelled by server";

    action.handle_cancel_next_request(|_request| -> Result<move_arm::CancelResponse> {
        println!("server received cancel request");
        Ok(move_arm::CancelResponse::new(
            false,
            Some(cancel_error.to_owned()),
        ))
    })
    .await?;

    println!("server responded to cancel request error={}", cancel_error);

    Ok(())
}

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        expose_action(&node_runner).await
    })
}
"#;
    let main_file = user_node_exposer.join("src").join("main.rs");
    fs::write(main_file, exposer_main).expect("failed to write exposer main");

    compile_project(&user_node_consumer);
    compile_project(&user_node_exposer);

    let exposer_runtime_config_str = exposer_runtime_config_path.to_str().unwrap().to_owned();
    let consumer_runtime_config_str = consumer_runtime_config_path.to_str().unwrap().to_owned();

    let messenger = peppylib::MessengerHandle::from_host_port(&router_host, router_port)
        .await
        .expect("failed to create messenger for test control");

    // Spawn exposer first so it's ready to handle requests
    let mut exposer_child = spawn_cargo_run(
        &user_node_exposer,
        &[(RUNTIME_CONFIG_VAR_NAME, &exposer_runtime_config_str)],
    );

    let action_ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        to_core_node: None,
    };
    wait_for_action_service_reachable_or_exit(
        &action_ctx,
        BRAIN_NODE_NAME,
        "move_arm",
        None,
        &mut exposer_child,
        &user_node_exposer,
    )
    .await;

    let mut consumer_child = spawn_cargo_run(
        &user_node_consumer,
        &[(RUNTIME_CONFIG_VAR_NAME, &consumer_runtime_config_str)],
    );

    let ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        to_core_node: Some(TEST_CORE_NODE),
    };
    wait_for_health_service_reachable_or_exit(
        &ctx,
        CONSUMER_NODE_NAME,
        consumer_instance_id,
        &mut consumer_child,
        &user_node_consumer,
    )
    .await;
    wait_for_health_service_reachable_or_exit(
        &ctx,
        BRAIN_NODE_NAME,
        exposer_instance_id,
        &mut exposer_child,
        &user_node_exposer,
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

    // Wait for both processes to exit
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
        "consumer cargo run failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        consumer_output.status.code(),
        consumer_stdout,
        consumer_stderr
    );
    assert!(
        consumer_stdout.contains("goal accepted=true")
            && consumer_stdout.contains("cancel accepted=false error=goal cancelled by server"),
        "consumer did not complete the cancel flow.\nstdout:\n{}\nstderr:\n{}",
        consumer_stdout,
        consumer_stderr
    );

    let exposer_stdout = String::from_utf8_lossy(&exposer_output.stdout).into_owned();
    let exposer_stderr = String::from_utf8_lossy(&exposer_output.stderr).into_owned();
    assert!(
        exposer_output.status.success(),
        "exposer cargo run failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        exposer_output.status.code(),
        exposer_stdout,
        exposer_stderr
    );
    assert!(
        exposer_stdout.contains("server handled goal request")
            && exposer_stdout.contains("server received cancel request")
            && exposer_stdout
                .contains("server responded to cancel request error=goal cancelled by server"),
        "exposer did not handle cancel endpoint as expected.\nstdout:\n{}\nstderr:\n{}",
        exposer_stdout,
        exposer_stderr
    );
}

/// Verifies the goal-completion side of the action lifecycle contract: a
/// client can use a drain-loop pattern (`loop { match
/// on_next_feedback_message().await { Ok(_) => ..., Err(_) => break } }`)
/// to consume every feedback message and reliably exit once the goal is
/// complete. This works because the Rust codegen for
/// `handle_result_next_request` publishes an end-of-stream sentinel (an
/// empty payload on the per-goal feedback publisher) before invoking the
/// user's result handler. Without that sentinel the loop would hang
/// forever, because `mpsc::Receiver::recv()` never returns `None` on its
/// own.
///
/// This is the regression test for the original "stuck on draining
/// feedback" hang seen in `openarm01_nodes/{action_server,action_client}`,
/// which is what motivated adding the per-goal feedback closure signal in
/// the first place.
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
///   5. Client's next `on_next_feedback_message().await` returns `Err`
///      (sentinel observed). The drain-loop matches the `Err` arm and
///      breaks.
///   6. Client calls `get_result` and receives the final response.
///
/// Python parity is `actions_communication_drain_loop_until_end_signal`
/// in the python/ test module.
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
        init_test_env::<generator::RustGenerator>(&temp_dir_consumer, STUB_NODE_CONFIG);
    generator
        .add_consumed_action(
            &consumed_action,
            &action_messages,
            &generator::DependencyContext::native("brain", "v1"),
        )
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
        NodeInstanceConfig {
            instance_id: Name::new(consumer_instance_id).unwrap(),
            arguments: Default::default(),
            framework: Default::default(),
        },
        CONSUMER_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let consumer_runtime_config_path = temp_dir_consumer.path().join("peppy_runtime.json5");
    consumer_runtime_config
        .save_json5_launch_config(&consumer_runtime_config_path)
        .unwrap();

    init_cargo_user_node(&user_node_consumer);
    let consumer_main = r#"
use peppygen::consumed_actions::brain_move_arm;
use peppygen::NodeBuilder;
use peppygen::Result;
use std::time::Duration;

async fn consume_action(node_runner: &peppygen::NodeRunner) -> Result<()> {
    let request = brain_move_arm::GoalRequest {
        arm_id: 7,
        desired_position: [10, 20, 30],
    };
    let mut action_handle = brain_move_arm::ActionHandle::fire_goal(
        &node_runner,
        Duration::from_secs(5),
        None,
        None,
        request,
        peppygen::QoSProfile::SensorData,
    ).await?;
    println!("goal accepted={}", action_handle.data.accepted);
    println!("draining feedback...");

    // Drain-loop pattern: keep reading feedback until the server signals
    // end-of-stream. Without the empty-payload sentinel, this loop would
    // hang forever because mpsc::recv() never returns None.
    let mut feedback_count = 0;
    loop {
        match action_handle.on_next_feedback_message().await {
            Ok(feedback) => {
                feedback_count += 1;
                println!("feedback #{} new_position={:?}", feedback_count, feedback.new_position);
            }
            Err(_) => {
                println!("feedback channel closed after {} messages", feedback_count);
                break;
            }
        }
    }

    let result = action_handle.get_result(Duration::from_secs(5)).await?;
    println!(
        "result success={} final_position={:?}",
        result.data.success,
        result.data.final_position
    );

    Ok(())
}

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        consume_action(&node_runner).await
    })
}
"#;
    let main_file = user_node_consumer.join("src").join("main.rs");
    fs::write(main_file, consumer_main).expect("failed to write consumer main");

    // --- Exposer (server) project
    let exposer_instance_id = EXPOSER_INSTANCE_ID;
    let temp_dir_exposer = TempDir::new().unwrap();
    let exposed_action: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE).unwrap();
    let (mut generator, output_dir_exposer, user_node_exposer, peppy_node_config_path) =
        init_test_env::<generator::RustGenerator>(&temp_dir_exposer, STUB_NODE_CONFIG);
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
        NodeInstanceConfig {
            instance_id: Name::new(exposer_instance_id).unwrap(),
            arguments: Default::default(),
            framework: Default::default(),
        },
        BRAIN_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let exposer_runtime_config_path = temp_dir_exposer.path().join("peppy_runtime.json5");
    exposer_runtime_config
        .save_json5_launch_config(&exposer_runtime_config_path)
        .unwrap();

    init_cargo_user_node(&user_node_exposer);
    let exposer_main = r#"
use peppygen::exposed_actions::move_arm;
use peppygen::NodeBuilder;
use peppygen::Result;

async fn expose_action(node_runner: &peppygen::NodeRunner) -> Result<()> {
    let mut action = move_arm::ActionHandle::expose(&node_runner).await?;

    action.handle_goal_next_request(|request| -> Result<move_arm::GoalResponse> {
        println!(
            "server received goal arm_id={} desired={:?}",
            request.data.arm_id,
            request.data.desired_position
        );
        Ok(move_arm::GoalResponse::new(true))
    })
    .await?;
    println!("server accepted goal");

    // Emit 3 feedback messages; the client must drain all of them before
    // the end-of-stream signal closes the stream.
    for i in 0..3 {
        let pos = [i, i + 1, i + 2];
        action.emit_feedback(pos).await?;
        println!("server emitted feedback #{} position={:?}", i + 1, pos);
    }

    let final_position = [99, 99, 99];
    action.handle_result_next_request(|_request| -> Result<move_arm::ResultResponse> {
        Ok(move_arm::ResultResponse::new(true, None, final_position))
    })
    .await?;
    println!("server handled result request final_position={:?}", final_position);

    Ok(())
}

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        expose_action(&node_runner).await
    })
}
"#;
    let main_file = user_node_exposer.join("src").join("main.rs");
    fs::write(main_file, exposer_main).expect("failed to write exposer main");

    compile_project(&user_node_consumer);
    compile_project(&user_node_exposer);

    let exposer_runtime_config_str = exposer_runtime_config_path.to_str().unwrap().to_owned();
    let consumer_runtime_config_str = consumer_runtime_config_path.to_str().unwrap().to_owned();

    let messenger = peppylib::MessengerHandle::from_host_port(&router_host, router_port)
        .await
        .expect("failed to create messenger for test control");

    let mut exposer_child = spawn_cargo_run(
        &user_node_exposer,
        &[(RUNTIME_CONFIG_VAR_NAME, &exposer_runtime_config_str)],
    );

    let action_ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        to_core_node: None,
    };
    wait_for_action_service_reachable_or_exit(
        &action_ctx,
        BRAIN_NODE_NAME,
        "move_arm",
        None,
        &mut exposer_child,
        &user_node_exposer,
    )
    .await;

    let mut consumer_child = spawn_cargo_run(
        &user_node_consumer,
        &[(RUNTIME_CONFIG_VAR_NAME, &consumer_runtime_config_str)],
    );

    let ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        to_core_node: Some(TEST_CORE_NODE),
    };
    wait_for_health_service_reachable_or_exit(
        &ctx,
        CONSUMER_NODE_NAME,
        consumer_instance_id,
        &mut consumer_child,
        &user_node_consumer,
    )
    .await;
    wait_for_health_service_reachable_or_exit(
        &ctx,
        BRAIN_NODE_NAME,
        exposer_instance_id,
        &mut exposer_child,
        &user_node_exposer,
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
        "consumer cargo run failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        consumer_output.status.code(),
        consumer_stdout,
        consumer_stderr
    );
    // Critical assertions: client drained all 3 feedback messages, then
    // the End signal closed the loop, then get_result succeeded.
    assert!(
        consumer_stdout.contains("feedback #1 new_position=[0, 1, 2]"),
        "consumer missed first feedback message.\nstdout:\n{}",
        consumer_stdout
    );
    assert!(
        consumer_stdout.contains("feedback #2 new_position=[1, 2, 3]"),
        "consumer missed second feedback message.\nstdout:\n{}",
        consumer_stdout
    );
    assert!(
        consumer_stdout.contains("feedback #3 new_position=[2, 3, 4]"),
        "consumer missed third feedback message.\nstdout:\n{}",
        consumer_stdout
    );
    assert!(
        consumer_stdout.contains("feedback channel closed after 3 messages"),
        "consumer feedback loop did not exit via the close signal.\nstdout:\n{}",
        consumer_stdout
    );
    assert!(
        consumer_stdout.contains("result success=true final_position=[99, 99, 99]"),
        "consumer did not receive final result.\nstdout:\n{}",
        consumer_stdout
    );

    let exposer_stdout = String::from_utf8_lossy(&exposer_output.stdout).into_owned();
    let exposer_stderr = String::from_utf8_lossy(&exposer_output.stderr).into_owned();
    assert!(
        exposer_output.status.success(),
        "exposer cargo run failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        exposer_output.status.code(),
        exposer_stdout,
        exposer_stderr
    );
    assert!(
        exposer_stdout.contains("server emitted feedback #3 position=[2, 3, 4]")
            && exposer_stdout.contains("server handled result request"),
        "exposer did not complete the action lifecycle.\nstdout:\n{}\nstderr:\n{}",
        exposer_stdout,
        exposer_stderr
    );
}

/// Verifies the cancel-accept side of the action lifecycle contract: when
/// the server's cancel handler returns `CancelResponse::new(true, ...)`,
/// the Rust codegen for `handle_cancel_next_request` must publish an
/// end-of-stream sentinel (an empty payload on the per-goal feedback
/// publisher) so the client knows no further feedback will arrive.
///
/// End-to-end flow exercised here:
///   1. Client `fire_goal`, server accepts.
///   2. Server emits one warmup feedback message; client receives it.
///   3. Client calls `cancel_goal`; server's cancel handler returns
///      `accepted == true`.
///   4. As a direct consequence of accepting the cancel, the codegen
///      publishes the end-of-stream sentinel on the per-goal feedback
///      publisher, closing the feedback stream for this goal.
///   5. The client's next `on_next_feedback_message().await` returns
///      `Err` (instead of blocking forever waiting for feedback that will
///      never come). That `Err` is what this test asserts.
///
/// The reject branch is covered by
/// `actions_communication_cancel_reject_keeps_feedback_open`.
///
/// Python parity is
/// `actions_communication_cancel_accept_closes_feedback_stream` in the
/// python/ test module.
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
        init_test_env::<generator::RustGenerator>(&temp_dir_consumer, STUB_NODE_CONFIG);
    generator
        .add_consumed_action(
            &consumed_action,
            &action_messages,
            &generator::DependencyContext::native("brain", "v1"),
        )
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
        NodeInstanceConfig {
            instance_id: Name::new(consumer_instance_id).unwrap(),
            arguments: Default::default(),
            framework: Default::default(),
        },
        CONSUMER_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let consumer_runtime_config_path = temp_dir_consumer.path().join("peppy_runtime.json5");
    consumer_runtime_config
        .save_json5_launch_config(&consumer_runtime_config_path)
        .unwrap();

    init_cargo_user_node(&user_node_consumer);
    let consumer_main = r#"
use peppygen::consumed_actions::brain_move_arm;
use peppygen::NodeBuilder;
use peppygen::Result;
use std::time::Duration;

async fn consume_action(node_runner: &peppygen::NodeRunner) -> Result<()> {
    let request = brain_move_arm::GoalRequest {
        arm_id: 7,
        desired_position: [10, 20, 30],
    };
    let mut action_handle = brain_move_arm::ActionHandle::fire_goal(
        &node_runner,
        Duration::from_secs(5),
        None,
        None,
        request,
        peppygen::QoSProfile::SensorData,
    ).await?;
    println!("goal accepted={}", action_handle.data.accepted);

    // Server emits a single warmup feedback before the cancel arrives so
    // we can verify it's received before the close signal.
    let warmup = action_handle.on_next_feedback_message().await?;
    println!("warmup feedback new_position={:?}", warmup.new_position);

    let cancel_response = action_handle.cancel_goal(Duration::from_secs(5)).await?;
    println!("cancel accepted={}", cancel_response.data.accepted);

    // After cancel-accept, the server's codegen publishes the end-of-stream
    // sentinel. The next on_next_feedback_message must error.
    match action_handle.on_next_feedback_message().await {
        Ok(msg) => {
            println!("UNEXPECTED feedback after cancel-accept new_position={:?}", msg.new_position);
        }
        Err(err) => {
            println!("feedback closed after cancel-accept err={}", err);
        }
    }

    Ok(())
}

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        consume_action(&node_runner).await
    })
}
"#;
    let main_file = user_node_consumer.join("src").join("main.rs");
    fs::write(main_file, consumer_main).expect("failed to write consumer main");

    // --- Exposer (server) project
    let exposer_instance_id = EXPOSER_INSTANCE_ID;
    let temp_dir_exposer = TempDir::new().unwrap();
    let exposed_action: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE).unwrap();
    let (mut generator, output_dir_exposer, user_node_exposer, peppy_node_config_path) =
        init_test_env::<generator::RustGenerator>(&temp_dir_exposer, STUB_NODE_CONFIG);
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
        NodeInstanceConfig {
            instance_id: Name::new(exposer_instance_id).unwrap(),
            arguments: Default::default(),
            framework: Default::default(),
        },
        BRAIN_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let exposer_runtime_config_path = temp_dir_exposer.path().join("peppy_runtime.json5");
    exposer_runtime_config
        .save_json5_launch_config(&exposer_runtime_config_path)
        .unwrap();

    init_cargo_user_node(&user_node_exposer);
    let exposer_main = r#"
use peppygen::exposed_actions::move_arm;
use peppygen::NodeBuilder;
use peppygen::Result;

async fn expose_action(node_runner: &peppygen::NodeRunner) -> Result<()> {
    let mut action = move_arm::ActionHandle::expose(&node_runner).await?;

    action.handle_goal_next_request(|_request| -> Result<move_arm::GoalResponse> {
        Ok(move_arm::GoalResponse::new(true))
    })
    .await?;
    println!("server accepted goal");

    action.emit_feedback([1, 2, 3]).await?;
    println!("server emitted warmup feedback");

    action.handle_cancel_next_request(|_request| -> Result<move_arm::CancelResponse> {
        Ok(move_arm::CancelResponse::new(true, None))
    })
    .await?;
    println!("server accepted cancel — codegen publishes end-of-stream sentinel");

    Ok(())
}

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        expose_action(&node_runner).await
    })
}
"#;
    let main_file = user_node_exposer.join("src").join("main.rs");
    fs::write(main_file, exposer_main).expect("failed to write exposer main");

    compile_project(&user_node_consumer);
    compile_project(&user_node_exposer);

    let exposer_runtime_config_str = exposer_runtime_config_path.to_str().unwrap().to_owned();
    let consumer_runtime_config_str = consumer_runtime_config_path.to_str().unwrap().to_owned();

    let messenger = peppylib::MessengerHandle::from_host_port(&router_host, router_port)
        .await
        .expect("failed to create messenger for test control");

    let mut exposer_child = spawn_cargo_run(
        &user_node_exposer,
        &[(RUNTIME_CONFIG_VAR_NAME, &exposer_runtime_config_str)],
    );

    let action_ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        to_core_node: None,
    };
    wait_for_action_service_reachable_or_exit(
        &action_ctx,
        BRAIN_NODE_NAME,
        "move_arm",
        None,
        &mut exposer_child,
        &user_node_exposer,
    )
    .await;

    let mut consumer_child = spawn_cargo_run(
        &user_node_consumer,
        &[(RUNTIME_CONFIG_VAR_NAME, &consumer_runtime_config_str)],
    );

    let ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        to_core_node: Some(TEST_CORE_NODE),
    };
    wait_for_health_service_reachable_or_exit(
        &ctx,
        CONSUMER_NODE_NAME,
        consumer_instance_id,
        &mut consumer_child,
        &user_node_consumer,
    )
    .await;
    wait_for_health_service_reachable_or_exit(
        &ctx,
        BRAIN_NODE_NAME,
        exposer_instance_id,
        &mut exposer_child,
        &user_node_exposer,
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
        "consumer cargo run failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
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
        consumer_stdout.contains("cancel accepted=true"),
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
        "consumer received unexpected feedback after cancel-accept — close signal must come first.\nstdout:\n{}",
        consumer_stdout
    );

    let exposer_stdout = String::from_utf8_lossy(&exposer_output.stdout).into_owned();
    let exposer_stderr = String::from_utf8_lossy(&exposer_output.stderr).into_owned();
    assert!(
        exposer_output.status.success(),
        "exposer cargo run failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        exposer_output.status.code(),
        exposer_stdout,
        exposer_stderr
    );
    assert!(
        exposer_stdout.contains("server accepted cancel"),
        "exposer did not reach the cancel-accept path.\nstdout:\n{}\nstderr:\n{}",
        exposer_stdout,
        exposer_stderr
    );
}

/// Verifies the cancel-reject side of the action lifecycle contract: when
/// the server's cancel handler returns `CancelResponse::new(false, ...)`,
/// the Rust codegen for `handle_cancel_next_request` must NOT publish an
/// end-of-stream sentinel. The goal stays alive, feedback keeps flowing,
/// and the stream is closed only later by the result-handler step (which
/// publishes the sentinel as part of normal goal completion).
///
/// End-to-end flow exercised here:
///   1. Client `fire_goal`, server accepts.
///   2. Server emits pre-cancel feedback; client receives it.
///   3. Client calls `cancel_goal`; server's cancel handler returns
///      `accepted == false`.
///   4. Because the cancel was rejected, codegen does NOT publish the
///      end-of-stream sentinel on the per-goal feedback publisher; the
///      feedback stream stays open and the active-goal state stays set.
///   5. Server emits post-cancel feedback; the client still receives it.
///      This is what proves step 4: the stream was not closed by the
///      cancel-reject.
///   6. Server's result handler runs and returns; this is the step that
///      publishes the end-of-stream sentinel on the per-goal feedback
///      publisher, as part of normal goal completion.
///   7. The client's next `on_next_feedback_message().await` returns
///      `Err`, confirming the stream is closed by the result step (not by
///      the earlier cancel-reject).
///
/// The accept branch is covered by
/// `actions_communication_cancel_accept_closes_feedback_stream`.
///
/// Python parity is
/// `actions_communication_cancel_reject_keeps_feedback_open` in the
/// python/ test module.
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
        init_test_env::<generator::RustGenerator>(&temp_dir_consumer, STUB_NODE_CONFIG);
    generator
        .add_consumed_action(
            &consumed_action,
            &action_messages,
            &generator::DependencyContext::native("brain", "v1"),
        )
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
        NodeInstanceConfig {
            instance_id: Name::new(consumer_instance_id).unwrap(),
            arguments: Default::default(),
            framework: Default::default(),
        },
        CONSUMER_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let consumer_runtime_config_path = temp_dir_consumer.path().join("peppy_runtime.json5");
    consumer_runtime_config
        .save_json5_launch_config(&consumer_runtime_config_path)
        .unwrap();

    init_cargo_user_node(&user_node_consumer);
    let consumer_main = r#"
use peppygen::consumed_actions::brain_move_arm;
use peppygen::NodeBuilder;
use peppygen::Result;
use std::time::Duration;

async fn consume_action(node_runner: &peppygen::NodeRunner) -> Result<()> {
    let request = brain_move_arm::GoalRequest {
        arm_id: 7,
        desired_position: [10, 20, 30],
    };
    let mut action_handle = brain_move_arm::ActionHandle::fire_goal(
        &node_runner,
        Duration::from_secs(5),
        None,
        None,
        request,
        peppygen::QoSProfile::SensorData,
    ).await?;
    println!("goal accepted={}", action_handle.data.accepted);

    let pre_cancel = action_handle.on_next_feedback_message().await?;
    println!("pre_cancel feedback new_position={:?}", pre_cancel.new_position);

    let cancel_response = action_handle.cancel_goal(Duration::from_secs(5)).await?;
    let error_msg = cancel_response.data.error_message.as_deref().unwrap_or("<none>");
    println!(
        "cancel accepted={} error={}",
        cancel_response.data.accepted, error_msg
    );

    // CRITICAL: feedback after cancel-reject must still arrive — the goal
    // continues running and the stream stays open.
    let post_cancel = action_handle.on_next_feedback_message().await?;
    println!("post_cancel feedback new_position={:?}", post_cancel.new_position);

    let result = action_handle.get_result(Duration::from_secs(5)).await?;
    println!("result success={}", result.data.success);

    // Now the result-handler step has closed the stream.
    match action_handle.on_next_feedback_message().await {
        Ok(msg) => {
            println!("UNEXPECTED feedback after result new_position={:?}", msg.new_position);
        }
        Err(_) => {
            println!("feedback closed after result");
        }
    }

    Ok(())
}

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        consume_action(&node_runner).await
    })
}
"#;
    let main_file = user_node_consumer.join("src").join("main.rs");
    fs::write(main_file, consumer_main).expect("failed to write consumer main");

    // --- Exposer (server) project. Cancel handler returns accepted=false,
    // so codegen must NOT publish the end-of-stream sentinel; subsequent
    // emit_feedback calls must continue to reach the client.
    let exposer_instance_id = EXPOSER_INSTANCE_ID;
    let temp_dir_exposer = TempDir::new().unwrap();
    let exposed_action: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE).unwrap();
    let (mut generator, output_dir_exposer, user_node_exposer, peppy_node_config_path) =
        init_test_env::<generator::RustGenerator>(&temp_dir_exposer, STUB_NODE_CONFIG);
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
        NodeInstanceConfig {
            instance_id: Name::new(exposer_instance_id).unwrap(),
            arguments: Default::default(),
            framework: Default::default(),
        },
        BRAIN_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let exposer_runtime_config_path = temp_dir_exposer.path().join("peppy_runtime.json5");
    exposer_runtime_config
        .save_json5_launch_config(&exposer_runtime_config_path)
        .unwrap();

    init_cargo_user_node(&user_node_exposer);
    let exposer_main = r#"
use peppygen::exposed_actions::move_arm;
use peppygen::NodeBuilder;
use peppygen::Result;

async fn expose_action(node_runner: &peppygen::NodeRunner) -> Result<()> {
    let mut action = move_arm::ActionHandle::expose(&node_runner).await?;

    action.handle_goal_next_request(|_request| -> Result<move_arm::GoalResponse> {
        Ok(move_arm::GoalResponse::new(true))
    })
    .await?;

    action.emit_feedback([1, 1, 1]).await?;
    println!("server emitted pre-cancel feedback");

    action.handle_cancel_next_request(|_request| -> Result<move_arm::CancelResponse> {
        Ok(move_arm::CancelResponse::new(false, Some("not now".to_owned())))
    })
    .await?;
    println!("server rejected cancel");

    // Cancel was rejected — the codegen must keep the stream open. This
    // emit_feedback would silently no-op (or panic on no-active-goal) if
    // the codegen incorrectly cleared current_goal on a rejected cancel.
    action.emit_feedback([2, 2, 2]).await?;
    println!("server emitted post-cancel feedback");

    action.handle_result_next_request(|_request| -> Result<move_arm::ResultResponse> {
        Ok(move_arm::ResultResponse::new(true, None, [9, 9, 9]))
    })
    .await?;
    println!("server handled result request");

    Ok(())
}

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        expose_action(&node_runner).await
    })
}
"#;
    let main_file = user_node_exposer.join("src").join("main.rs");
    fs::write(main_file, exposer_main).expect("failed to write exposer main");

    compile_project(&user_node_consumer);
    compile_project(&user_node_exposer);

    let exposer_runtime_config_str = exposer_runtime_config_path.to_str().unwrap().to_owned();
    let consumer_runtime_config_str = consumer_runtime_config_path.to_str().unwrap().to_owned();

    let messenger = peppylib::MessengerHandle::from_host_port(&router_host, router_port)
        .await
        .expect("failed to create messenger for test control");

    let mut exposer_child = spawn_cargo_run(
        &user_node_exposer,
        &[(RUNTIME_CONFIG_VAR_NAME, &exposer_runtime_config_str)],
    );

    let action_ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        to_core_node: None,
    };
    wait_for_action_service_reachable_or_exit(
        &action_ctx,
        BRAIN_NODE_NAME,
        "move_arm",
        None,
        &mut exposer_child,
        &user_node_exposer,
    )
    .await;

    let mut consumer_child = spawn_cargo_run(
        &user_node_consumer,
        &[(RUNTIME_CONFIG_VAR_NAME, &consumer_runtime_config_str)],
    );

    let ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        to_core_node: Some(TEST_CORE_NODE),
    };
    wait_for_health_service_reachable_or_exit(
        &ctx,
        CONSUMER_NODE_NAME,
        consumer_instance_id,
        &mut consumer_child,
        &user_node_consumer,
    )
    .await;
    wait_for_health_service_reachable_or_exit(
        &ctx,
        BRAIN_NODE_NAME,
        exposer_instance_id,
        &mut exposer_child,
        &user_node_exposer,
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
        "consumer cargo run failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
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
        consumer_stdout.contains("cancel accepted=false error=not now"),
        "consumer did not see rejected cancel.\nstdout:\n{}",
        consumer_stdout
    );
    assert!(
        consumer_stdout.contains("post_cancel feedback new_position=[2, 2, 2]"),
        "consumer did not receive post-cancel feedback — the rejected cancel must NOT close the stream.\nstdout:\n{}",
        consumer_stdout
    );
    assert!(
        consumer_stdout.contains("result success=true"),
        "consumer did not receive result.\nstdout:\n{}",
        consumer_stdout
    );
    assert!(
        consumer_stdout.contains("feedback closed after result"),
        "consumer did not receive close signal after result handler.\nstdout:\n{}",
        consumer_stdout
    );

    let exposer_stdout = String::from_utf8_lossy(&exposer_output.stdout).into_owned();
    let exposer_stderr = String::from_utf8_lossy(&exposer_output.stderr).into_owned();
    assert!(
        exposer_output.status.success(),
        "exposer cargo run failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        exposer_output.status.code(),
        exposer_stdout,
        exposer_stderr
    );
    assert!(
        exposer_stdout.contains("server emitted pre-cancel feedback")
            && exposer_stdout.contains("server rejected cancel")
            && exposer_stdout.contains("server emitted post-cancel feedback")
            && exposer_stdout.contains("server handled result request"),
        "exposer did not exercise the cancel-reject path correctly.\nstdout:\n{}",
        exposer_stdout
    );
}
