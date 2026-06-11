use crate::helpers::{
    CapturedChild, DEFAULT_WAIT_TIMEOUT, STUB_NODE_CONFIG, WaitContext, compile_project,
    copy_config_to_output, init_cargo_user_node, init_test_env, send_shutdown, spawn_cargo_run,
    test_peppy_dirs, wait_for_action_service_reachable_or_exit, wait_for_child,
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

#[rstest::rstest]
#[case::peer(crate::helpers::Mode::Peer)]
#[case::router(crate::helpers::Mode::Router)]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn actions_communication(#[case] mode: crate::helpers::Mode) {
    let instance = pmi::ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    // --- Consumer (client) project
    let consumer_instance_id = CONSUMER_INSTANCE_ID;
    let temp_dir_consumer = TempDir::new_in(crate::helpers::test_tmp_root())
        .expect("failed to create temp dir for consumer project");
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
        NodeInstanceConfig::new(Name::new(consumer_instance_id).unwrap()),
        CONSUMER_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let consumer_runtime_config = crate::helpers::apply_mode(consumer_runtime_config, mode);
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
        request,
        peppygen::QoSProfile::SensorData,
    ).await?;
    println!("goal accepted={}", action_handle.data.accepted);

    let feedback = action_handle.on_next_feedback_message().await?;
    assert_eq!(feedback.new_position, [7, 31, 43], "unexpected feedback message");
    println!("feedback message received new_position={:?}", feedback.new_position);

    let result = action_handle.get_result(Duration::from_secs(5)).await?;
    match result.outcome {
        brain_move_arm::ResultOutcome::Completed(data) => println!(
            "result success={} error={:?} final_position={:?}",
            data.success,
            data.error_msg.as_deref(),
            data.final_position
        ),
        other => panic!("expected Completed outcome, got {other:?}"),
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
    let temp_dir_exposer = TempDir::new_in(crate::helpers::test_tmp_root())
        .expect("failed to create temp dir for exposer project");
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
        NodeInstanceConfig::new(Name::new(exposer_instance_id).unwrap()),
        BRAIN_NODE_NAME, // Must match the node name expected by the consumer
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let exposer_runtime_config = crate::helpers::apply_mode(exposer_runtime_config, mode);
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

    // Spawn the accept loop so this setup fn returns and the node starts
    // serving (health, etc.). The loop accepts goals and drives each one;
    // the engine routes cancel/result back to the matching goal by goal_id.
    tokio::spawn(async move {
        loop {
            let maybe_ctx = action
                .handle_goal_next_request(|request| -> Result<move_arm::GoalResponse> {
                    println!(
                        "server received goal arm_id={} desired={:?}",
                        request.data.arm_id, request.data.desired_position
                    );
                    Ok(move_arm::GoalResponse::accept())
                })
                .await;

            match maybe_ctx {
                Ok(Some(ctx)) => {
                    let feedback_message = [7, 31, 43];
                    let _ = ctx.publish_feedback(feedback_message).await;
                    println!("server emitted feedback message {:?}", feedback_message);

                    let final_position = [98, 4, 26];
                    let _ = ctx.complete(true, None, final_position).await;
                    println!(
                        "server handled result request. Final position sent: {:?}",
                        &final_position
                    );
                }
                _ => break, // None (stream closed / rejected) or Err
            }
        }
    });

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

    let mut consumer_child = spawn_cargo_run(
        &user_node_consumer,
        &[(RUNTIME_CONFIG_VAR_NAME, &consumer_runtime_config_str)],
    );

    let ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_core_node: Some(TEST_CORE_NODE),
    };
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
        BRAIN_NODE_NAME,
        exposer_instance_id,
        &mut exposer_child,
        &user_node_exposer,
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

#[rstest::rstest]
#[case::peer(crate::helpers::Mode::Peer)]
#[case::router(crate::helpers::Mode::Router)]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn actions_communication_cancel_goal(#[case] mode: crate::helpers::Mode) {
    let instance = pmi::ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    // --- Consumer (client) project
    let consumer_instance_id = CONSUMER_INSTANCE_ID;
    let temp_dir_consumer = TempDir::new_in(crate::helpers::test_tmp_root())
        .expect("failed to create temp dir for consumer project");
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
        NodeInstanceConfig::new(Name::new(consumer_instance_id).unwrap()),
        CONSUMER_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let consumer_runtime_config = crate::helpers::apply_mode(consumer_runtime_config, mode);
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
        request,
        peppygen::QoSProfile::SensorData,
    ).await?;
    println!("goal accepted={}", action_handle.data.accepted);

    let cancel_response = action_handle.cancel_goal(Duration::from_secs(5)).await?;
    let accepted = matches!(
        cancel_response.state,
        brain_move_arm::CancelState::Signalled
    );
    println!("cancel accepted={} error=<none>", accepted);

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
    let temp_dir_exposer = TempDir::new_in(crate::helpers::test_tmp_root())
        .expect("failed to create temp dir for exposer project");
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
        NodeInstanceConfig::new(Name::new(exposer_instance_id).unwrap()),
        BRAIN_NODE_NAME, // Must match the node name expected by the consumer
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let exposer_runtime_config = crate::helpers::apply_mode(exposer_runtime_config, mode);
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

    tokio::spawn(async move {
        loop {
            let maybe_ctx = action
                .handle_goal_next_request(|request| -> Result<move_arm::GoalResponse> {
                    println!(
                        "server received goal arm_id={} desired={:?}",
                        request.data.arm_id, request.data.desired_position
                    );
                    Ok(move_arm::GoalResponse::accept())
                })
                .await;

            match maybe_ctx {
                Ok(Some(ctx)) => {
                    // Wait for a cancel for this goal, then report the cancelled result.
                    ctx.cancel_signal().await;
                    println!("server observed cancel for goal");
                    let _ = ctx
                        .complete_cancelled(false, Some("goal cancelled by server".to_owned()), [0, 0, 0])
                        .await;
                }
                _ => break,
            }
        }
    });

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

    let mut consumer_child = spawn_cargo_run(
        &user_node_consumer,
        &[(RUNTIME_CONFIG_VAR_NAME, &consumer_runtime_config_str)],
    );

    let ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_core_node: Some(TEST_CORE_NODE),
    };
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
        BRAIN_NODE_NAME,
        exposer_instance_id,
        &mut exposer_child,
        &user_node_exposer,
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
            && consumer_stdout.contains("cancel accepted=true error=<none>"),
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
        exposer_stdout.contains("server received goal arm_id=7 desired=[10, 20, 30]")
            && exposer_stdout.contains("server observed cancel for goal"),
        "exposer did not handle cancel endpoint as expected.\nstdout:\n{}\nstderr:\n{}",
        exposer_stdout,
        exposer_stderr
    );
}

/// Verifies the goal-completion side of the action lifecycle contract: a
/// client can use a drain-loop pattern (`loop { match
/// on_next_feedback_message().await { Ok(_) => ..., Err(_) => break } }`)
/// to consume every feedback message and reliably exit once the goal is
/// complete. This works because `GoalContext::complete` publishes an
/// end-of-stream sentinel (an empty payload on the per-goal feedback
/// publisher) when it delivers the result. Without that sentinel the loop
/// would hang forever, because the feedback channel never closes on its own.
///
/// This is the regression test for the original "stuck on draining
/// feedback" hang seen in `openarm01_nodes/{action_server,action_client}`,
/// which is what motivated the per-goal feedback closure signal.
///
/// End-to-end flow exercised here:
///   1. Client `fire_goal`, server accepts.
///   2. Server emits 3 feedback messages.
///   3. Client's drain-loop receives all 3 in order.
///   4. Server calls `ctx.complete(...)`, which publishes the end-of-stream
///      sentinel on this goal's feedback publisher and stores the result.
///   5. Client's next `on_next_feedback_message().await` returns `Err`
///      (sentinel observed). The drain-loop matches the `Err` arm and
///      breaks.
///   6. Client calls `get_result` and receives the final response.
///
/// Python parity is `actions_communication_drain_loop_until_end_signal`
/// in the python/ test module.
#[rstest::rstest]
#[case::peer(crate::helpers::Mode::Peer)]
#[case::router(crate::helpers::Mode::Router)]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn actions_communication_drain_loop_until_end_signal(#[case] mode: crate::helpers::Mode) {
    let instance = pmi::ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    // --- Consumer (client) project
    let consumer_instance_id = CONSUMER_INSTANCE_ID;
    let temp_dir_consumer = TempDir::new_in(crate::helpers::test_tmp_root())
        .expect("failed to create temp dir for consumer project");
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
        NodeInstanceConfig::new(Name::new(consumer_instance_id).unwrap()),
        CONSUMER_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let consumer_runtime_config = crate::helpers::apply_mode(consumer_runtime_config, mode);
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
    match result.outcome {
        brain_move_arm::ResultOutcome::Completed(data) => println!(
            "result success={} final_position={:?}",
            data.success,
            data.final_position
        ),
        other => panic!("expected Completed outcome, got {other:?}"),
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
    let temp_dir_exposer = TempDir::new_in(crate::helpers::test_tmp_root())
        .expect("failed to create temp dir for exposer project");
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
        NodeInstanceConfig::new(Name::new(exposer_instance_id).unwrap()),
        BRAIN_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let exposer_runtime_config = crate::helpers::apply_mode(exposer_runtime_config, mode);
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

    tokio::spawn(async move {
        loop {
            let maybe_ctx = action
                .handle_goal_next_request(|request| -> Result<move_arm::GoalResponse> {
                    println!(
                        "server received goal arm_id={} desired={:?}",
                        request.data.arm_id, request.data.desired_position
                    );
                    Ok(move_arm::GoalResponse::accept())
                })
                .await;

            let ctx = match maybe_ctx {
                Ok(Some(ctx)) => ctx,
                _ => break,
            };
            println!("server accepted goal");

            // Emit 3 feedback messages; the client must drain all of them
            // before completion closes the stream.
            for i in 0..3 {
                let pos = [i, i + 1, i + 2];
                let _ = ctx.publish_feedback(pos).await;
                println!("server emitted feedback #{} position={:?}", i + 1, pos);
            }

            let final_position = [99, 99, 99];
            // complete publishes the end-of-stream sentinel, then delivers the result.
            let _ = ctx.complete(true, None, final_position).await;
            println!("server handled result request final_position={:?}", final_position);
        }
    });

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

    let mut consumer_child = spawn_cargo_run(
        &user_node_consumer,
        &[(RUNTIME_CONFIG_VAR_NAME, &consumer_runtime_config_str)],
    );

    let ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_core_node: Some(TEST_CORE_NODE),
    };
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
        BRAIN_NODE_NAME,
        exposer_instance_id,
        &mut exposer_child,
        &user_node_exposer,
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

/// Verifies the cancel-honored side of the action lifecycle contract: when
/// the server's worker observes `ctx.cancel_signal()` and reacts with
/// `ctx.complete_cancelled(...)`, completing the goal publishes an
/// end-of-stream sentinel on the per-goal feedback publisher so the client
/// knows no further feedback will arrive.
///
/// End-to-end flow exercised here:
///   1. Client `fire_goal`, server accepts.
///   2. Server emits one warmup feedback message; client receives it.
///   3. Client calls `cancel_goal`; the framework auto-acks `accepted == true`
///      (a live goal received the signal) and the worker's `cancel_signal()`
///      resolves.
///   4. The worker reacts with `complete_cancelled`, which closes this goal's
///      feedback stream.
///   5. The client's next `on_next_feedback_message().await` returns
///      `Err` (instead of blocking forever waiting for feedback that will
///      never come). That `Err` is what this test asserts.
///
/// The ignore-cancel branch is covered by
/// `actions_communication_cancel_reject_keeps_feedback_open`.
///
/// Python parity is
/// `actions_communication_cancel_accept_closes_feedback_stream` in the
/// python/ test module.
#[rstest::rstest]
#[case::peer(crate::helpers::Mode::Peer)]
#[case::router(crate::helpers::Mode::Router)]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn actions_communication_cancel_accept_closes_feedback_stream(
    #[case] mode: crate::helpers::Mode,
) {
    let instance = pmi::ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    // --- Consumer (client) project
    let consumer_instance_id = CONSUMER_INSTANCE_ID;
    let temp_dir_consumer = TempDir::new_in(crate::helpers::test_tmp_root())
        .expect("failed to create temp dir for consumer project");
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
        NodeInstanceConfig::new(Name::new(consumer_instance_id).unwrap()),
        CONSUMER_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let consumer_runtime_config = crate::helpers::apply_mode(consumer_runtime_config, mode);
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
        request,
        peppygen::QoSProfile::SensorData,
    ).await?;
    println!("goal accepted={}", action_handle.data.accepted);

    // Server emits a single warmup feedback before the cancel arrives so
    // we can verify it's received before the close signal.
    let warmup = action_handle.on_next_feedback_message().await?;
    println!("warmup feedback new_position={:?}", warmup.new_position);

    let cancel_response = action_handle.cancel_goal(Duration::from_secs(5)).await?;
    let accepted = matches!(
        cancel_response.state,
        brain_move_arm::CancelState::Signalled
    );
    println!("cancel accepted={}", accepted);

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
    let temp_dir_exposer = TempDir::new_in(crate::helpers::test_tmp_root())
        .expect("failed to create temp dir for exposer project");
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
        NodeInstanceConfig::new(Name::new(exposer_instance_id).unwrap()),
        BRAIN_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let exposer_runtime_config = crate::helpers::apply_mode(exposer_runtime_config, mode);
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

    tokio::spawn(async move {
        loop {
            let maybe_ctx = action
                .handle_goal_next_request(|_request| -> Result<move_arm::GoalResponse> {
                    Ok(move_arm::GoalResponse::accept())
                })
                .await;

            let ctx = match maybe_ctx {
                Ok(Some(ctx)) => ctx,
                _ => break,
            };
            println!("server accepted goal");

            let _ = ctx.publish_feedback([1, 2, 3]).await;
            println!("server emitted warmup feedback");

            // Honor the cancel: completing-cancelled closes the feedback stream.
            ctx.cancel_signal().await;
            let _ = ctx
                .complete_cancelled(false, Some("cancelled".to_owned()), [0, 0, 0])
                .await;
            println!("server observed cancel — completing cancelled closes the feedback stream");
        }
    });

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

    let mut consumer_child = spawn_cargo_run(
        &user_node_consumer,
        &[(RUNTIME_CONFIG_VAR_NAME, &consumer_runtime_config_str)],
    );

    let ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_core_node: Some(TEST_CORE_NODE),
    };
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
        BRAIN_NODE_NAME,
        exposer_instance_id,
        &mut exposer_child,
        &user_node_exposer,
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
        exposer_stdout.contains("server observed cancel"),
        "exposer did not reach the cancel-observed path.\nstdout:\n{}\nstderr:\n{}",
        exposer_stdout,
        exposer_stderr
    );
}

/// Verifies the cancel-ignored side of the action lifecycle contract: a
/// worker is free to observe `ctx.cancel_signal()` and keep going. Ignoring
/// the cancel does NOT close the feedback stream — the goal stays alive,
/// feedback keeps flowing, and the stream is closed only when the worker
/// finally calls `ctx.complete(...)` as part of normal goal completion.
///
/// End-to-end flow exercised here:
///   1. Client `fire_goal`, server accepts.
///   2. Server emits pre-cancel feedback; client receives it.
///   3. Client calls `cancel_goal`; the framework auto-acks `accepted == true`,
///      and the worker's `cancel_signal()` resolves.
///   4. The worker chooses to keep going (does not complete), so the feedback
///      stream stays open.
///   5. Server emits post-cancel feedback; the client still receives it.
///      This is what proves step 4: ignoring the cancel did not close the stream.
///   6. The worker calls `ctx.complete(...)`; this is the step that
///      publishes the end-of-stream sentinel on the per-goal feedback
///      publisher, as part of normal goal completion.
///   7. The client's next `on_next_feedback_message().await` returns
///      `Err`, confirming the stream is closed by completion (not by the
///      earlier cancel).
///
/// The honor-cancel branch is covered by
/// `actions_communication_cancel_accept_closes_feedback_stream`.
///
/// Python parity is
/// `actions_communication_cancel_reject_keeps_feedback_open` in the
/// python/ test module.
#[rstest::rstest]
#[case::peer(crate::helpers::Mode::Peer)]
#[case::router(crate::helpers::Mode::Router)]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn actions_communication_cancel_reject_keeps_feedback_open(
    #[case] mode: crate::helpers::Mode,
) {
    let instance = pmi::ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    // --- Consumer (client) project
    let consumer_instance_id = CONSUMER_INSTANCE_ID;
    let temp_dir_consumer = TempDir::new_in(crate::helpers::test_tmp_root())
        .expect("failed to create temp dir for consumer project");
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
        NodeInstanceConfig::new(Name::new(consumer_instance_id).unwrap()),
        CONSUMER_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let consumer_runtime_config = crate::helpers::apply_mode(consumer_runtime_config, mode);
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
        request,
        peppygen::QoSProfile::SensorData,
    ).await?;
    println!("goal accepted={}", action_handle.data.accepted);

    let pre_cancel = action_handle.on_next_feedback_message().await?;
    println!("pre_cancel feedback new_position={:?}", pre_cancel.new_position);

    let cancel_response = action_handle.cancel_goal(Duration::from_secs(5)).await?;
    let accepted = matches!(
        cancel_response.state,
        brain_move_arm::CancelState::Signalled
    );
    println!("cancel accepted={} error=<none>", accepted);

    // CRITICAL: feedback after cancel-reject must still arrive — the goal
    // continues running and the stream stays open.
    let post_cancel = action_handle.on_next_feedback_message().await?;
    println!("post_cancel feedback new_position={:?}", post_cancel.new_position);

    let result = action_handle.get_result(Duration::from_secs(5)).await?;
    match result.outcome {
        brain_move_arm::ResultOutcome::Completed(data) => {
            println!("result success={}", data.success)
        }
        other => panic!("expected Completed outcome, got {other:?}"),
    }

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
    // publish_feedback calls must continue to reach the client.
    let exposer_instance_id = EXPOSER_INSTANCE_ID;
    let temp_dir_exposer = TempDir::new_in(crate::helpers::test_tmp_root())
        .expect("failed to create temp dir for exposer project");
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
        NodeInstanceConfig::new(Name::new(exposer_instance_id).unwrap()),
        BRAIN_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let exposer_runtime_config = crate::helpers::apply_mode(exposer_runtime_config, mode);
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

    tokio::spawn(async move {
        loop {
            let maybe_ctx = action
                .handle_goal_next_request(|_request| -> Result<move_arm::GoalResponse> {
                    Ok(move_arm::GoalResponse::accept())
                })
                .await;

            let ctx = match maybe_ctx {
                Ok(Some(ctx)) => ctx,
                _ => break,
            };

            let _ = ctx.publish_feedback([1, 1, 1]).await;
            println!("server emitted pre-cancel feedback");

            // Observe the cancel but choose to keep going (ignore it).
            // Feedback must keep flowing since the goal hasn't completed.
            ctx.cancel_signal().await;
            println!("server observed cancel but keeps going");

            let _ = ctx.publish_feedback([2, 2, 2]).await;
            println!("server emitted post-cancel feedback");

            let _ = ctx.complete(true, None, [9, 9, 9]).await;
            println!("server handled result request");
        }
    });

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

    let mut consumer_child = spawn_cargo_run(
        &user_node_consumer,
        &[(RUNTIME_CONFIG_VAR_NAME, &consumer_runtime_config_str)],
    );

    let ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_core_node: Some(TEST_CORE_NODE),
    };
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
        BRAIN_NODE_NAME,
        exposer_instance_id,
        &mut exposer_child,
        &user_node_exposer,
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
        consumer_stdout.contains("cancel accepted=true error=<none>"),
        "consumer did not see the auto-acked cancel.\nstdout:\n{}",
        consumer_stdout
    );
    assert!(
        consumer_stdout.contains("post_cancel feedback new_position=[2, 2, 2]"),
        "consumer did not receive post-cancel feedback — a worker that ignores the cancel keeps the stream open.\nstdout:\n{}",
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
            && exposer_stdout.contains("server observed cancel but keeps going")
            && exposer_stdout.contains("server emitted post-cancel feedback")
            && exposer_stdout.contains("server handled result request"),
        "exposer did not exercise the cancel-ignored path correctly.\nstdout:\n{}",
        exposer_stdout
    );
}

/// Verifies the producer-disappearance side of the action lifecycle contract:
/// when the exposer PROCESS is SIGKILLed mid-goal, no end-of-stream sentinel
/// is ever published (the `GoalContext` dies with the process), so the
/// consumer's feedback drain must fail over to the typed
/// `Error::ActionFeedbackProducerGone` — driven by the producer's liveliness
/// token disappearing — instead of hanging forever or reporting a clean
/// close. `get_result` must then resolve to `ResultOutcome::Abandoned` via
/// the goal handle's confirmed-gone fast path.
///
/// End-to-end flow exercised here:
///   1. Client `fire_goal`, server accepts and publishes feedback in an
///      endless loop — it never completes the goal, so only the kill ends it.
///   2. Client receives the first feedback (proves the goal is live), then
///      keeps draining.
///   3. The test SIGKILLs the exposer process. SIGKILL closes the TCP socket,
///      so the liveliness DELETE propagates immediately and the consumer's
///      watcher confirms the producer gone after its probe window.
///   4. The client's drain unblocks with `ActionFeedbackProducerGone`
///      (matched explicitly — a clean-close error here would mean a phantom
///      sentinel) and `get_result` yields `ResultOutcome::Abandoned`.
///
/// In-library parity is
/// `concurrent_action_producer_death_unblocks_feedback_and_yields_abandoned`
/// in `crates/peppylib/tests/actions.rs`.
#[rstest::rstest]
#[case::peer(crate::helpers::Mode::Peer)]
#[case::router(crate::helpers::Mode::Router)]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn actions_communication_producer_sigkill_unblocks_drain_and_abandons(
    #[case] mode: crate::helpers::Mode,
) {
    let instance = pmi::ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    // --- Consumer (client) project
    let consumer_instance_id = CONSUMER_INSTANCE_ID;
    let temp_dir_consumer = TempDir::new_in(crate::helpers::test_tmp_root())
        .expect("failed to create temp dir for consumer project");
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
        NodeInstanceConfig::new(Name::new(consumer_instance_id).unwrap()),
        CONSUMER_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let consumer_runtime_config = crate::helpers::apply_mode(consumer_runtime_config, mode);
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
        request,
        peppygen::QoSProfile::SensorData,
    ).await?;
    println!("goal accepted={}", action_handle.data.accepted);

    let first = action_handle.on_next_feedback_message().await?;
    println!("first feedback received new_position={:?}", first.new_position);

    // Drain until the producer dies. SIGKILL leaves the goal incomplete, so
    // the clean-close sentinel can never arrive — the only valid exit is the
    // typed producer-gone error. Matching it apart from other errors is the
    // point of this test.
    loop {
        match action_handle.on_next_feedback_message().await {
            Ok(_) => {}
            Err(peppygen::Error::ActionFeedbackProducerGone { instance_id, action_name }) => {
                println!(
                    "feedback drain unblocked: producer gone instance={:?} action={}",
                    instance_id, action_name
                );
                break;
            }
            Err(err) => {
                println!("feedback drain unblocked: UNEXPECTED error={}", err);
                break;
            }
        }
    }

    // The confirmed-gone latch resolves this without polling the dead
    // producer; the generous timeout only covers scheduler hiccups.
    let result = action_handle.get_result(Duration::from_secs(15)).await?;
    match result.outcome {
        brain_move_arm::ResultOutcome::Abandoned => println!("result outcome=Abandoned"),
        other => println!("result outcome=UNEXPECTED {other:?}"),
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

    // --- Exposer (server) project. The worker never completes the goal and
    // never exits on its own: it must still be mid-goal, sentinel unpublished,
    // when the test SIGKILLs the process.
    let exposer_instance_id = EXPOSER_INSTANCE_ID;
    let temp_dir_exposer = TempDir::new_in(crate::helpers::test_tmp_root())
        .expect("failed to create temp dir for exposer project");
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
        NodeInstanceConfig::new(Name::new(exposer_instance_id).unwrap()),
        BRAIN_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let exposer_runtime_config = crate::helpers::apply_mode(exposer_runtime_config, mode);
    let exposer_runtime_config_path = temp_dir_exposer.path().join("peppy_runtime.json5");
    exposer_runtime_config
        .save_json5_launch_config(&exposer_runtime_config_path)
        .unwrap();

    init_cargo_user_node(&user_node_exposer);
    let exposer_main = r#"
use peppygen::exposed_actions::move_arm;
use peppygen::NodeBuilder;
use peppygen::Result;
use std::time::Duration;

async fn expose_action(node_runner: &peppygen::NodeRunner) -> Result<()> {
    let mut action = move_arm::ActionHandle::expose(&node_runner).await?;

    tokio::spawn(async move {
        loop {
            let maybe_ctx = action
                .handle_goal_next_request(|_request| -> Result<move_arm::GoalResponse> {
                    Ok(move_arm::GoalResponse::accept())
                })
                .await;

            let ctx = match maybe_ctx {
                Ok(Some(ctx)) => ctx,
                _ => break,
            };
            println!("server accepted goal");

            // Publish feedback forever and never complete: the goal must be
            // live — sentinel unpublished — when the test SIGKILLs this
            // process. Per-beat prints are deliberately omitted so the
            // undrained stdout pipe cannot fill and block the loop.
            let mut beat: i32 = 0;
            loop {
                let _ = ctx.publish_feedback([beat, beat, beat]).await;
                beat = beat.wrapping_add(1);
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    });

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

    // The consumer's setup fn blocks inside the drain loop until the
    // producer-gone error fires, so its stdout — not a health probe — is the
    // only way to observe progress before the kill.
    let mut consumer = CapturedChild::new(spawn_cargo_run(
        &user_node_consumer,
        &[(RUNTIME_CONFIG_VAR_NAME, &consumer_runtime_config_str)],
    ));

    // The goal must be live (accepted + feedback flowing) before the kill,
    // otherwise the test would exercise fire_goal failure, not mid-goal death.
    consumer.wait_for_stdout_contains(
        "first feedback received",
        DEFAULT_WAIT_TIMEOUT,
        &user_node_consumer,
    );

    // SIGKILL the exposer mid-goal. No graceful close runs: the liveliness
    // token disappearing (TCP socket closed by the kernel) is the only signal
    // the consumer gets.
    exposer_child.kill().expect("failed to SIGKILL exposer");
    let _ = exposer_child.wait();

    // Detection budget: liveliness DELETE propagates immediately on socket
    // close, then the watcher's confirmation probes add up to ~1.5s. The wide
    // timeout only guards against slow CI.
    consumer.wait_for_stdout_contains("result outcome=", DEFAULT_WAIT_TIMEOUT, &user_node_consumer);

    // The setup fn has returned, so the node is now serving health/shutdown
    // and can be told to exit cleanly.
    let ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_core_node: Some(TEST_CORE_NODE),
    };
    wait_for_health_service_reachable_or_exit(
        &ctx,
        CONSUMER_NODE_NAME,
        consumer_instance_id,
        &mut consumer.child,
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

    // Only the consumer's exit is meaningful — the exposer was SIGKILLed, so
    // its status is unconditionally a failure and is not asserted.
    let consumer_output = consumer.wait(Some(Duration::from_secs(10)), &user_node_consumer);

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
        consumer_stdout.contains("goal accepted=true"),
        "consumer goal was not accepted.\nstdout:\n{}",
        consumer_stdout
    );
    assert!(
        consumer_stdout.contains("first feedback received"),
        "consumer never saw live feedback before the kill.\nstdout:\n{}",
        consumer_stdout
    );
    // Critical assertion: the drain exited through the typed producer-gone
    // error carrying the dead producer's identity — not a clean close, not a
    // hang, not some other error.
    assert!(
        consumer_stdout.contains(
            r#"feedback drain unblocked: producer gone instance=Some("exposer_instance") action=move_arm"#
        ),
        "consumer drain did not unblock with ActionFeedbackProducerGone.\nstdout:\n{}\nstderr:\n{}",
        consumer_stdout,
        consumer_stderr
    );
    assert!(
        !consumer_stdout.contains("UNEXPECTED"),
        "consumer hit an unexpected error or outcome.\nstdout:\n{}",
        consumer_stdout
    );
    assert!(
        consumer_stdout.contains("result outcome=Abandoned"),
        "get_result did not resolve to Abandoned after producer death.\nstdout:\n{}\nstderr:\n{}",
        consumer_stdout,
        consumer_stderr
    );
}
