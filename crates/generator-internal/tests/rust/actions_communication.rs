use crate::helpers::{
    STUB_NODE_CONFIG, WaitContext, compile_project, copy_config_to_output, init_cargo_user_node,
    init_test_env, send_shutdown, spawn_cargo_run, test_peppy_dirs,
    wait_for_action_service_reachable_or_exit, wait_for_child,
    wait_for_health_service_reachable_or_exit,
};
use config::consts::{PEPPYGEN_OUTPUT_PATH, RUNTIME_CONFIG_VAR_NAME};
use config::runtime::NodeInstance;
use config::{
    node::{ConsumedAction, ExposedAction, MessageFormat},
    peppy_config::Name,
    runtime::RuntimeConfig,
};
use generator::{ConsumedActionMessage, LanguageGenerator};
use std::path::Path;
use std::{fs, time::Duration};
use tempfile::TempDir;

// --- Common test constants
const TEST_CORE_NODE: &str = "test_core";
const SUBSCRIBER_NODE_NAME: &str = "subscriber_node";
const SUBSCRIBER_INSTANCE_ID: &str = "subscriber_instance";
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

const SUBSCRIBED_ACTION_EXAMPLE: &str = r#"
{
  id: "brain_move_arm",
  node: "brain",
  name: "move_arm",
  tag: "0.1.0",
}
"#;

const SUBSCRIBED_ACTION_FEEDBACK_FORMAT: &str = r#"
{
  new_position: {
    $type: "array",
    $items: "i32",
    $length: 3
  }
}
"#;

const SUBSCRIBED_ACTION_RESULT_FORMAT: &str = r#"
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

const SUBSCRIBED_ACTION_GOAL_FORMAT: &str = r#"
{
  arm_id: "u16",
  desired_position: {
    $type: "array",
    $items: "i32",
    $length: 3
  }
}
"#;

const SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT: &str = r#"
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

    // --- Subscriber (client) project
    let subscriber_instance_id = SUBSCRIBER_INSTANCE_ID;
    let temp_dir_subscriber = TempDir::new().unwrap();
    let consumed_action: ConsumedAction = serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE).unwrap();
    let goal_request_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_FORMAT).unwrap();
    let goal_response_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT).unwrap();
    let feedback_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_FEEDBACK_FORMAT).unwrap();
    let result_response_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_RESULT_FORMAT).unwrap();
    let action_messages = ConsumedActionMessage {
        goal_request: Some(goal_request_format),
        goal_response: Some(goal_response_format),
        feedback: Some(feedback_format),
        result_request: None,
        result_response: Some(result_response_format),
    };
    let (mut generator, output_dir_subscriber, user_node_subscriber, peppy_node_config_path) =
        init_test_env::<generator::RustGenerator>(&temp_dir_subscriber, STUB_NODE_CONFIG);
    generator
        .add_consumed_action(&consumed_action, &action_messages)
        .unwrap();
    let output_config = copy_config_to_output(&user_node_subscriber, &output_dir_subscriber);
    generator
        .build(
            &output_dir_subscriber,
            &test_peppy_dirs(),
            Default::default(),
        )
        .unwrap();
    fs::remove_file(output_config).unwrap();
    config::fingerprint::create_codegen_fingerprint(
        &peppy_node_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let subscriber_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstance {
            instance_id: Name::new(subscriber_instance_id).unwrap(),
            arguments: Default::default(),
        },
        SUBSCRIBER_NODE_NAME,
        TEST_CORE_NODE,
    )
    .unwrap();
    let subscriber_runtime_config_path = temp_dir_subscriber.path().join("peppy_runtime.json5");
    subscriber_runtime_config
        .save_json5_launch_config(&subscriber_runtime_config_path)
        .unwrap();

    init_cargo_user_node(&user_node_subscriber);
    let subscriber_main = r#"
use peppygen::consumed_actions::brain_move_arm;
use peppygen::NodeBuilder;
use peppygen::Result;
use std::time::Duration;

const TARGET_NODE_NAME: &str = "brain";
const TARGET_INSTANCE_ID: &str = "exposer_instance";
const FEEDBACK_READY_SERVICE: &str = "move_arm_feedback_ready";
const FEEDBACK_RECEIVED_SERVICE: &str = "move_arm_feedback_received";

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        let request = brain_move_arm::GoalRequest {
            arm_id: 7,
            desired_position: [10, 20, 30],
        };
        let mut goal = brain_move_arm::ActionHandle::fire_goal(
            &node_runner,
            Duration::from_secs(5),
            None,
            None,
            request,
            peppygen::QoSProfile::SensorData,
        ).await?;
        println!("goal accepted={}", goal.data.accepted);

        let feedback_wait = goal.on_next_feedback_message();
        let notify_feedback_ready = peppygen::ServiceMessenger::poll(
            node_runner.messenger(),
            node_runner.processor().bound_core_node(),
            node_runner.processor().bound_instance_id(),
            TARGET_NODE_NAME,
            FEEDBACK_READY_SERVICE,
            Some(node_runner.processor().bound_core_node()),
            Some(TARGET_INSTANCE_ID),
            Vec::<u8>::new().into(),
            Duration::from_secs(5),
        );
        let (feedback, ready_response) = tokio::join!(feedback_wait, notify_feedback_ready);
        ready_response?;
        let feedback = feedback?;
        assert_eq!(feedback.new_position, [7, 31, 43], "unexpected feedback message");
        println!("feedback message received new_position={:?}", feedback.new_position);

        peppygen::ServiceMessenger::poll(
            node_runner.messenger(),
            node_runner.processor().bound_core_node(),
            node_runner.processor().bound_instance_id(),
            TARGET_NODE_NAME,
            FEEDBACK_RECEIVED_SERVICE,
            Some(node_runner.processor().bound_core_node()),
            Some(TARGET_INSTANCE_ID),
            Vec::<u8>::new().into(),
            Duration::from_secs(5),
        )
        .await?;

        let result = goal.get_result(Duration::from_secs(5)).await?;
        println!(
            "result success={} error={:?} final_position={:?}",
            result.data.success,
            result.data.error_msg.as_deref(),
            result.data.final_position
        );

        Ok(())
    })
}
"#;
    let main_file = user_node_subscriber.join("src").join("main.rs");
    fs::write(main_file, subscriber_main).expect("failed to write subscriber main");

    // --- Exposer (server) project
    let exposer_instance_id = EXPOSER_INSTANCE_ID;
    let temp_dir_exposer = TempDir::new().unwrap();
    let exposed_action: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE).unwrap();
    let (mut generator, output_dir_exposer, user_node_exposer, peppy_node_config_path) =
        init_test_env::<generator::RustGenerator>(&temp_dir_exposer, STUB_NODE_CONFIG);
    generator.add_exposed_action(&exposed_action).unwrap();
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
        NodeInstance {
            instance_id: Name::new(exposer_instance_id).unwrap(),
            arguments: Default::default(),
        },
        BRAIN_NODE_NAME, // Must match the node name expected by the subscriber
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
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

const FEEDBACK_READY_SERVICE: &str = "move_arm_feedback_ready";
const FEEDBACK_RECEIVED_SERVICE: &str = "move_arm_feedback_received";

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        let mut action = move_arm::ActionHandle::expose(&node_runner).await?;

        // Set up handshake service listeners BEFORE handling the goal so their
        // Zenoh subscriptions are active when the subscriber starts polling
        // immediately after fire_goal returns. Messages are buffered in the
        // subscription channels until handle_next_request reads them.
        let mut feedback_ready_service = peppygen::ServiceMessenger::listen(
            node_runner.messenger(),
            node_runner.processor().bound_core_node(),
            node_runner.processor().bound_instance_id(),
            node_runner.processor().node_name(),
            FEEDBACK_READY_SERVICE,
        )
        .await?;
        let mut feedback_received_service = peppygen::ServiceMessenger::listen(
            node_runner.messenger(),
            node_runner.processor().bound_core_node(),
            node_runner.processor().bound_instance_id(),
            node_runner.processor().node_name(),
            FEEDBACK_RECEIVED_SERVICE,
        )
        .await?;

        action.handle_goal_next_request(|request| -> Result<move_arm::GoalResponse> {
            println!(
                "server received goal arm_id={} desired={:?}",
                request.data.arm_id,
                request.data.desired_position
            );
            Ok(move_arm::GoalResponse::new(true))
        })
        .await?;

        feedback_ready_service
            .handle_next_request(|_request| async move { Ok(Vec::<u8>::new().into()) })
            .await?;
        println!("server received feedback-ready handshake");

        let feedback_received = Arc::new(AtomicBool::new(false));
        let feedback_received_flag = Arc::clone(&feedback_received);
        let feedback_received_task = tokio::spawn(async move {
            feedback_received_service
                .handle_next_request(|_request| async move { Ok(Vec::<u8>::new().into()) })
                .await?;
            feedback_received_flag.store(true, Ordering::Release);
            Ok::<(), peppygen::Error>(())
        });

        let feedback_message = [7, 31, 43];
        let mut feedback_logged = false;
        while !feedback_received.load(Ordering::Acquire) {
            action.emit_feedback(feedback_message).await?;
            if !feedback_logged {
                println!("server emitted feedback message {:?}", feedback_message);
                feedback_logged = true;
            }
            tokio::task::yield_now().await;
        }
        feedback_received_task
            .await
            .expect("feedback ack task should not panic")?;

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
    })
}
"#;
    let main_file = user_node_exposer.join("src").join("main.rs");
    fs::write(main_file, exposer_main).expect("failed to write exposer main");

    compile_project(&user_node_subscriber);
    compile_project(&user_node_exposer);

    let exposer_runtime_config_str = exposer_runtime_config_path.to_str().unwrap().to_owned();
    let subscriber_runtime_config_str = subscriber_runtime_config_path.to_str().unwrap().to_owned();

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
        "move_arm/goal",
        None,
        &mut exposer_child,
        &user_node_exposer,
    )
    .await;

    let mut subscriber_child = spawn_cargo_run(
        &user_node_subscriber,
        &[(RUNTIME_CONFIG_VAR_NAME, &subscriber_runtime_config_str)],
    );

    let ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_core_node: Some(TEST_CORE_NODE),
    };
    wait_for_health_service_reachable_or_exit(
        &ctx,
        SUBSCRIBER_NODE_NAME,
        subscriber_instance_id,
        &mut subscriber_child,
        &user_node_subscriber,
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
        SUBSCRIBER_NODE_NAME,
        Some(TEST_CORE_NODE),
        subscriber_instance_id,
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
    let subscriber_output = wait_for_child(
        &mut subscriber_child,
        Some(Duration::from_secs(10)),
        &user_node_subscriber,
    );
    let exposer_output = wait_for_child(
        &mut exposer_child,
        Some(Duration::from_secs(10)),
        &user_node_exposer,
    );

    let subscriber_stdout = String::from_utf8_lossy(&subscriber_output.stdout).into_owned();
    let subscriber_stderr = String::from_utf8_lossy(&subscriber_output.stderr).into_owned();
    assert!(
        subscriber_output.status.success(),
        "subscriber cargo run failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        subscriber_output.status.code(),
        subscriber_stdout,
        subscriber_stderr
    );
    assert!(
        subscriber_stdout.contains("goal accepted=true")
            && subscriber_stdout.contains("feedback message received new_position=[7, 31, 43]")
            && subscriber_stdout
                .contains("result success=true error=None final_position=[98, 4, 26]"),
        "subscriber did not complete the action flow.\nstdout:\n{}\nstderr:\n{}",
        subscriber_stdout,
        subscriber_stderr
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

    // --- Subscriber (client) project
    let subscriber_instance_id = SUBSCRIBER_INSTANCE_ID;
    let temp_dir_subscriber = TempDir::new().unwrap();
    let consumed_action: ConsumedAction = serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE).unwrap();
    let goal_request_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_FORMAT).unwrap();
    let goal_response_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT).unwrap();
    let feedback_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_FEEDBACK_FORMAT).unwrap();
    let result_response_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_RESULT_FORMAT).unwrap();
    let action_messages = ConsumedActionMessage {
        goal_request: Some(goal_request_format),
        goal_response: Some(goal_response_format),
        feedback: Some(feedback_format),
        result_request: None,
        result_response: Some(result_response_format),
    };
    let (mut generator, output_dir_subscriber, user_node_subscriber, peppy_node_config_path) =
        init_test_env::<generator::RustGenerator>(&temp_dir_subscriber, STUB_NODE_CONFIG);
    generator
        .add_consumed_action(&consumed_action, &action_messages)
        .unwrap();
    let output_config = copy_config_to_output(&user_node_subscriber, &output_dir_subscriber);
    generator
        .build(
            &output_dir_subscriber,
            &test_peppy_dirs(),
            Default::default(),
        )
        .unwrap();
    fs::remove_file(output_config).unwrap();
    config::fingerprint::create_codegen_fingerprint(
        &peppy_node_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let subscriber_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstance {
            instance_id: Name::new(subscriber_instance_id).unwrap(),
            arguments: Default::default(),
        },
        SUBSCRIBER_NODE_NAME,
        TEST_CORE_NODE,
    )
    .unwrap();
    let subscriber_runtime_config_path = temp_dir_subscriber.path().join("peppy_runtime.json5");
    subscriber_runtime_config
        .save_json5_launch_config(&subscriber_runtime_config_path)
        .unwrap();

    init_cargo_user_node(&user_node_subscriber);
    let subscriber_main = r#"
use peppygen::consumed_actions::brain_move_arm;
use peppygen::NodeBuilder;
use peppygen::Result;
use std::time::Duration;

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        let request = brain_move_arm::GoalRequest {
            arm_id: 7,
            desired_position: [10, 20, 30],
        };
        let goal = brain_move_arm::ActionHandle::fire_goal(
            &node_runner,
            Duration::from_secs(5),
            None,
            None,
            request,
            peppygen::QoSProfile::SensorData,
        ).await?;
        println!("goal accepted={}", goal.data.accepted);

        let cancel_response = goal.cancel_goal(Duration::from_secs(5)).await?;
        let error_msg = cancel_response.data.error_message.as_deref().unwrap_or("<none>");
        println!(
            "cancel accepted={} error={}",
            cancel_response.data.accepted,
            error_msg
        );

        Ok(())
    })
}
"#;
    let main_file = user_node_subscriber.join("src").join("main.rs");
    fs::write(main_file, subscriber_main).expect("failed to write subscriber main");

    // --- Exposer (server) project
    let exposer_instance_id = EXPOSER_INSTANCE_ID;
    let temp_dir_exposer = TempDir::new().unwrap();
    let exposed_action: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE).unwrap();
    let (mut generator, output_dir_exposer, user_node_exposer, peppy_node_config_path) =
        init_test_env::<generator::RustGenerator>(&temp_dir_exposer, STUB_NODE_CONFIG);
    generator.add_exposed_action(&exposed_action).unwrap();
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
        NodeInstance {
            instance_id: Name::new(exposer_instance_id).unwrap(),
            arguments: Default::default(),
        },
        BRAIN_NODE_NAME, // Must match the node name expected by the subscriber
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

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
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
    })
}
"#;
    let main_file = user_node_exposer.join("src").join("main.rs");
    fs::write(main_file, exposer_main).expect("failed to write exposer main");

    compile_project(&user_node_subscriber);
    compile_project(&user_node_exposer);

    let exposer_runtime_config_str = exposer_runtime_config_path.to_str().unwrap().to_owned();
    let subscriber_runtime_config_str = subscriber_runtime_config_path.to_str().unwrap().to_owned();

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
        "move_arm/goal",
        None,
        &mut exposer_child,
        &user_node_exposer,
    )
    .await;

    let mut subscriber_child = spawn_cargo_run(
        &user_node_subscriber,
        &[(RUNTIME_CONFIG_VAR_NAME, &subscriber_runtime_config_str)],
    );

    let ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_core_node: Some(TEST_CORE_NODE),
    };
    wait_for_health_service_reachable_or_exit(
        &ctx,
        SUBSCRIBER_NODE_NAME,
        subscriber_instance_id,
        &mut subscriber_child,
        &user_node_subscriber,
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
        SUBSCRIBER_NODE_NAME,
        Some(TEST_CORE_NODE),
        subscriber_instance_id,
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
    let subscriber_output = wait_for_child(
        &mut subscriber_child,
        Some(Duration::from_secs(10)),
        &user_node_subscriber,
    );
    let exposer_output = wait_for_child(
        &mut exposer_child,
        Some(Duration::from_secs(10)),
        &user_node_exposer,
    );

    let subscriber_stdout = String::from_utf8_lossy(&subscriber_output.stdout).into_owned();
    let subscriber_stderr = String::from_utf8_lossy(&subscriber_output.stderr).into_owned();
    assert!(
        subscriber_output.status.success(),
        "subscriber cargo run failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        subscriber_output.status.code(),
        subscriber_stdout,
        subscriber_stderr
    );
    assert!(
        subscriber_stdout.contains("goal accepted=true")
            && subscriber_stdout.contains("cancel accepted=false error=goal cancelled by server"),
        "subscriber did not complete the cancel flow.\nstdout:\n{}\nstderr:\n{}",
        subscriber_stdout,
        subscriber_stderr
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
