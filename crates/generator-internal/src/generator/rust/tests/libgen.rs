use super::*;
use crate::generator::types::SubscribedActionMessage;
use config::node::{
    ExposedAction, ExposedService, ExposedTopic, MessageFormat, SubscribedAction,
    SubscribedService, SubscribedTopic,
};
use pmi::MessengerBackend;
use std::{fs, thread, time::Duration};
use tempfile::TempDir;

fn start_router_for_tests(rt: &tokio::runtime::Runtime) -> (pmi::Messenger, TempDir, String, u16) {
    rt.block_on(peppylib::start_zenohd_process("127.0.0.1", None))
        .expect("failed to start zenoh router for test")
}

// --- Topics exposes and its corresponding subscriber
const EXPOSED_TOPIC_EXAMPLE: &str = r#"
{
  name: "push_frame",
  qos_profile: "sensor_data",
  message_format: {
    header: {
    $type: "object",
    stamp: "time",
    frame_id: "u32"
  },
  encoding: "string",
    width: "u32",
    height: "u32",
    image: {
      $type: "array",
      $items: "u8",
      $length: 3
    }
  }
}
"#;

const SUBSCRIBED_TOPIC_EXAMPLE: &str = r#"
{
  node: "uvc_camera",
  name: "push_frame",
  tag: "0.1.0"
}
"#;

const SUBSCRIBED_TOPIC_FORMAT_EXAMPLE: &str = r#"
{
  header: {
    $type: "object",
    stamp: "time",
    frame_id: "u32"
  },
  encoding: "string",
  width: "u32",
  height: "u32",
  image: {
    $type: "array",
    $items: "u8",
    $length: 3
  }
}
"#;

/// Creates 2 projects in separate directory and check if they can send/receive topics
#[test]
fn topics_communication() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("failed to create tokio runtime");

    let (mut router, _dir, router_host, router_port) = rt
        .block_on(peppylib::start_zenohd_process("127.0.0.1", None))
        .expect("failed to start zenoh router for test");

    // --- Subscriber project
    let temp_dir_proj2 = TempDir::new().unwrap();
    let subscribed_topic: SubscribedTopic =
        serde_json5::from_str(SUBSCRIBED_TOPIC_EXAMPLE).unwrap();
    let subscribed_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE).unwrap();
    let (mut generator, output_dir2, user_node_subscriber) = init_test_env(&temp_dir_proj2);
    generator
        .add_subscribed_topic(&subscribed_topic, subscribed_format)
        .unwrap();
    let output_config = copy_config_to_output(&user_node_subscriber, &output_dir2);
    generator.build(&output_dir2).unwrap();
    fs::remove_file(output_config).unwrap();
    init_cargo_user_node(&user_node_subscriber);
    let subscriber_main = format!(
        "
use peppygen::subscribed_topics::uvc_camera_push_frame::on_next_message_received;
use peppygen::{{Messenger, Result}};

#[tokio::main]
async fn main() -> Result<()> {{
    let messenger = Messenger::connect(\"{}\", {}).await?;

    let (instance_id, frame) = on_next_message_received(&messenger).await?;
    println!(
        \"got {{}}x{{}} frame encoded as {{}} from {{}}\",
        frame.width, frame.height, frame.encoding, &instance_id
    );

    Ok(())
}}
",
        router_host, router_port
    );
    let main_file = user_node_subscriber.join("src").join("main.rs");
    fs::write(main_file, &subscriber_main).expect("failed to write main file");

    // --- Exposer project
    let exposer_instance_name = "test_instance";
    let temp_dir_proj1 = TempDir::new().unwrap();
    let exposed_topic: ExposedTopic = serde_json5::from_str(EXPOSED_TOPIC_EXAMPLE).unwrap();
    let (mut generator, output_dir1, user_node_exposer) = init_test_env(&temp_dir_proj1);
    generator.add_exposed_topic(&exposed_topic).unwrap();
    let output_config = copy_config_to_output(&user_node_exposer, &output_dir1);
    generator.build(&output_dir1).unwrap();
    fs::remove_file(output_config).unwrap();
    init_cargo_user_node(&user_node_exposer);
    let exposer_main = format!(
        "
use peppygen::exposed_topics::push_frame;
use peppygen::{{Messenger, Result}};

#[tokio::main]
async fn main() -> Result<()> {{
    let messenger = Messenger::connect(\"{}\", {}).await?;

    push_frame::emit(
        &messenger,
        push_frame::MessageHeader {{
            stamp: std::time::SystemTime::now(),
            frame_id: 42,
        }},
        \"rgb8\".to_owned(),
        640,
        480,
        [1, 2, 3],
    )
    .await?;

    Ok(())
}}
",
        router_host, router_port
    );

    let main_file = user_node_exposer.join("src").join("main.rs");
    fs::write(main_file, &exposer_main).expect("failed to write main file");

    compile_project(&user_node_subscriber);
    compile_project(&user_node_exposer);

    let subscriber_dir = user_node_subscriber.clone();
    let subscriber_thread =
        thread::spawn(move || run_cargo_run(&subscriber_dir, Some(Duration::from_secs(5)), &[]));

    // Give the subscriber a moment to connect before emitting frames.
    thread::sleep(Duration::from_millis(500));

    let exposer_dir = user_node_exposer.clone();
    let exposer_thread = thread::spawn(move || {
        run_cargo_run(
            &exposer_dir,
            Some(Duration::from_secs(5)),
            &[("PEPPY_INSTANCE_ID", exposer_instance_name)],
        )
    });

    let subscriber_output = subscriber_thread
        .join()
        .expect("subscriber thread panicked");
    let exposer_output = exposer_thread.join().expect("exposer thread panicked");

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
        subscriber_stdout.contains("got 640x480 frame encoded as rgb8"),
        "subscriber did not receive exposer frame.\nstdout:\n{}\nstderr:\n{}",
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

    rt.block_on(async {
        router
            .stop_router()
            .await
            .expect("failed to stop zenoh router");
    });
}

// --- Services exposes and its corresponding subscriber
const EXPOSED_SERVICE_EXAMPLE: &str = r#"
{
  name: "enable_camera",
  request_message_format: {
    enable: "bool"
  },
  response_message_format: {
    enabled: "bool",
    error_msg: {
      $type: "string",
      $optional: true
    },
  }
}
"#;

const SUBSCRIBED_SERVICE_EXAMPLE: &str = r#"
{
  node: "uvc_camera",
  name: "enable_camera",
  tag: "0.1.0"
}
"#;

const SUBSCRIBED_SERVICE_REQUEST_FORMAT_EXAMPLE: &str = r#"
{
  enable: "bool"
}
"#;

const SUBSCRIBED_SERVICE_RESPONSE_FORMAT_EXAMPLE: &str = r#"
{
    enabled: "bool",
    error_msg: {
      $type: "string",
      $optional: true
    },
}
"#;

#[test]
fn services_communication() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("failed to create tokio runtime");

    let (mut router, _dir, router_host, router_port) = rt
        .block_on(peppylib::start_zenohd_process("127.0.0.1", None))
        .expect("failed to start zenoh router for test");

    // --- Subscriber (client) project
    let temp_dir_subscriber = TempDir::new().unwrap();
    let subscribed_service: SubscribedService =
        serde_json5::from_str(SUBSCRIBED_SERVICE_EXAMPLE).unwrap();
    let subscribed_request_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_SERVICE_REQUEST_FORMAT_EXAMPLE).unwrap();
    let subscribed_response_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_SERVICE_RESPONSE_FORMAT_EXAMPLE).unwrap();
    let (mut generator, output_dir_subscriber, user_node_subscriber) =
        init_test_env(&temp_dir_subscriber);
    generator
        .add_subscribed_service(
            &subscribed_service,
            Some(&subscribed_request_format),
            Some(&subscribed_response_format),
        )
        .unwrap();
    let output_config = copy_config_to_output(&user_node_subscriber, &output_dir_subscriber);
    generator.build(&output_dir_subscriber).unwrap();
    fs::remove_file(output_config).unwrap();
    init_cargo_user_node(&user_node_subscriber);
    let subscriber_main = format!(
        "
use peppygen::subscribed_services::uvc_camera_enable_camera;
use peppygen::{{Messenger, Result}};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {{
    let messenger = Messenger::connect(\"{}\", {}).await?;

    let response =
        uvc_camera_enable_camera::poll(&messenger, Duration::from_secs(5), true).await?;
    let error_msg = response.error_msg.as_deref().unwrap_or(\"<none>\");
    println!(
        \"enable_camera result: enabled={{}} error={{}}\",
        response.enabled,
        error_msg
    );

    Ok(())
}}
",
        router_host, router_port
    );
    let main_file = user_node_subscriber.join("src").join("main.rs");
    fs::write(main_file, subscriber_main).expect("failed to write subscriber main");

    // --- Exposer (server) project
    let temp_dir_exposer = TempDir::new().unwrap();
    let exposed_service: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE).unwrap();
    let (mut generator, output_dir_exposer, user_node_exposer) = init_test_env(&temp_dir_exposer);
    generator.add_exposed_service(&exposed_service).unwrap();
    let output_config = copy_config_to_output(&user_node_exposer, &output_dir_exposer);
    generator.build(&output_dir_exposer).unwrap();
    fs::remove_file(output_config).unwrap();
    init_cargo_user_node(&user_node_exposer);
    let exposer_main = format!(
        "
use peppygen::exposed_services::enable_camera;
use peppygen::{{Messenger, Result}};

#[tokio::main]
async fn main() -> Result<()> {{
    let messenger = Messenger::connect(\"{}\", {}).await?;

    enable_camera::handle_next_request(&messenger, |request| -> Result<enable_camera::Response> {{
        println!(\"received enable_camera request: {{}}\", request.enable);
        Ok(enable_camera::Response::new(
            request.enable,
            Some(\"handled\".to_owned()),
        ))
    }})
    .await?;

    println!(\"enable_camera handler finished\");

    Ok(())
}}
",
        router_host, router_port
    );
    let main_file = user_node_exposer.join("src").join("main.rs");
    fs::write(main_file, exposer_main).expect("failed to write exposer main");

    compile_project(&user_node_subscriber);
    compile_project(&user_node_exposer);

    let exposer_dir = user_node_exposer.clone();
    let exposer_thread =
        thread::spawn(move || run_cargo_run(&exposer_dir, Some(Duration::from_secs(10)), &[]));

    // Give the exposer a moment to start listening before the subscriber sends a request.
    thread::sleep(Duration::from_millis(500));

    let subscriber_dir = user_node_subscriber.clone();
    let subscriber_thread =
        thread::spawn(move || run_cargo_run(&subscriber_dir, Some(Duration::from_secs(10)), &[]));

    let exposer_output = exposer_thread.join().expect("exposer thread panicked");
    let subscriber_output = subscriber_thread
        .join()
        .expect("subscriber thread panicked");

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
        subscriber_stdout.contains("enable_camera result: enabled=true error=handled"),
        "subscriber did not receive expected service response.\nstdout:\n{}\nstderr:\n{}",
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
        exposer_stdout.contains("received enable_camera request: true")
            && exposer_stdout.contains("enable_camera handler finished"),
        "exposer did not process the enable_camera request.\nstdout:\n{}\nstderr:\n{}",
        exposer_stdout,
        exposer_stderr
    );

    rt.block_on(async {
        router
            .stop_router()
            .await
            .expect("failed to stop zenoh router");
    });
}

// --- Actions
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

#[test]
fn actions_communication() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("failed to create tokio runtime");

    let (mut router, _dir, router_host, router_port) = rt
        .block_on(peppylib::start_zenohd_process("127.0.0.1", None))
        .expect("failed to start zenoh router for test");

    // --- Subscriber (client) project
    let temp_dir_subscriber = TempDir::new().unwrap();
    let subscribed_action: SubscribedAction =
        serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE).unwrap();
    let goal_request_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_FORMAT).unwrap();
    let goal_response_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT).unwrap();
    let feedback_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_FEEDBACK_FORMAT).unwrap();
    let result_response_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_RESULT_FORMAT).unwrap();
    let action_messages = SubscribedActionMessage {
        goal_request: Some(goal_request_format),
        goal_response: Some(goal_response_format),
        feedback: Some(feedback_format),
        result_request: None,
        result_response: Some(result_response_format),
    };
    let (mut generator, output_dir_subscriber, user_node_subscriber) =
        init_test_env(&temp_dir_subscriber);
    generator
        .add_subscribed_action(&subscribed_action, Some(&action_messages))
        .unwrap();
    let output_config = copy_config_to_output(&user_node_subscriber, &output_dir_subscriber);
    generator.build(&output_dir_subscriber).unwrap();
    fs::remove_file(output_config).unwrap();
    init_cargo_user_node(&user_node_subscriber);
    let subscriber_main = format!(
        "
use peppygen::subscribed_actions::brain_move_arm;
use peppygen::{{Messenger, Result}};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {{
    let messenger = Messenger::connect(\"{}\", {}).await?;

    let goal = brain_move_arm::fire_goal(&messenger, Duration::from_secs(5), 7, [10, 20, 30]).await?;
    println!(\"goal accepted={{}}\", goal.accepted);

    let feedback = brain_move_arm::on_next_feedback_message(&messenger).await?;
    assert_eq!(feedback.new_position, [7, 31, 43], \"unexpected feedback message\");
    println!(\"feedback message received new_position={{:?}}\", feedback.new_position);

    let result = brain_move_arm::get_action_result(&messenger, Duration::from_secs(5)).await?;
    println!(
        \"result success={{}} error={{:?}} final_position={{:?}}\",
        result.success,
        result.error_msg.as_deref(),
        result.final_position
    );

    Ok(())
}}
",
        router_host, router_port
    );
    let main_file = user_node_subscriber.join("src").join("main.rs");
    fs::write(main_file, subscriber_main).expect("failed to write subscriber main");

    // --- Exposer (server) project
    let temp_dir_exposer = TempDir::new().unwrap();
    let exposed_action: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE).unwrap();
    let (mut generator, output_dir_exposer, user_node_exposer) = init_test_env(&temp_dir_exposer);
    generator.add_exposed_action(&exposed_action).unwrap();
    let output_config = copy_config_to_output(&user_node_exposer, &output_dir_exposer);
    generator.build(&output_dir_exposer).unwrap();
    fs::remove_file(output_config).unwrap();
    init_cargo_user_node(&user_node_exposer);
    let exposer_main = format!(
        "
use peppygen::exposed_actions::move_arm;
use peppygen::{{Messenger, Result}};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {{
    let messenger = Messenger::connect(\"{}\", {}).await?;

    move_arm::handle_goal_next_request(&messenger, |request| -> Result<move_arm::GoalResponse> {{
        println!(
            \"server received goal arm_id={{}} desired={{:?}}\",
            request.arm_id,
            request.desired_position
        );
        Ok(move_arm::GoalResponse::new(true))
    }})
    .await?;

    // Small delay before sending a feedback message
    tokio::time::sleep(Duration::from_millis(200)).await;

    let feedback_message = [7, 31, 43];
    move_arm::emit_feedback(&messenger, feedback_message).await?;
    println!(\"server emitted feedback message {{:?}}\", feedback_message);

    let final_position = [98, 4, 26];
    move_arm::handle_result_next_request(&messenger, || -> Result<move_arm::ResultResponse> {{
        println!(\"server preparing action result\");
        let final_pos = final_position.clone();
        Ok(move_arm::ResultResponse::new(
            true,
            None,
            final_pos,
        ))
    }})
    .await?;

    println!(\"server handled result request. Final position sent: {{:?}}\", &final_position);

    Ok(())
}}
",
        router_host, router_port
    );
    let main_file = user_node_exposer.join("src").join("main.rs");
    fs::write(main_file, exposer_main).expect("failed to write exposer main");

    compile_project(&user_node_subscriber);
    compile_project(&user_node_exposer);

    let exposer_dir = user_node_exposer.clone();
    let exposer_thread =
        thread::spawn(move || run_cargo_run(&exposer_dir, Some(Duration::from_secs(15)), &[]));

    // Give the exposer a moment to start listening for goals before firing one.
    thread::sleep(Duration::from_millis(500));

    let subscriber_dir = user_node_subscriber.clone();
    let subscriber_thread =
        thread::spawn(move || run_cargo_run(&subscriber_dir, Some(Duration::from_secs(15)), &[]));

    let exposer_output = exposer_thread.join().expect("exposer thread panicked");
    let subscriber_output = subscriber_thread
        .join()
        .expect("subscriber thread panicked");

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

    rt.block_on(async {
        router
            .stop_router()
            .await
            .expect("failed to stop zenoh router");
    });
}

#[test]
fn actions_communication_cancel_goal() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("failed to create tokio runtime");

    let (mut router, _dir, router_host, router_port) = start_router_for_tests(&rt);

    // --- Subscriber (client) project
    let temp_dir_subscriber = TempDir::new().unwrap();
    let subscribed_action: SubscribedAction =
        serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE).unwrap();
    let goal_request_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_FORMAT).unwrap();
    let goal_response_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT).unwrap();
    let feedback_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_FEEDBACK_FORMAT).unwrap();
    let result_response_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_RESULT_FORMAT).unwrap();
    let action_messages = SubscribedActionMessage {
        goal_request: Some(goal_request_format),
        goal_response: Some(goal_response_format),
        feedback: Some(feedback_format),
        result_request: None,
        result_response: Some(result_response_format),
    };
    let (mut generator, output_dir_subscriber, user_node_subscriber) =
        init_test_env(&temp_dir_subscriber);
    generator
        .add_subscribed_action(&subscribed_action, Some(&action_messages))
        .unwrap();
    let output_config = copy_config_to_output(&user_node_subscriber, &output_dir_subscriber);
    generator.build(&output_dir_subscriber).unwrap();
    fs::remove_file(output_config).unwrap();
    init_cargo_user_node(&user_node_subscriber);
    let subscriber_main = format!(
        "
use peppygen::subscribed_actions::brain_move_arm;
use peppygen::{{Messenger, Result}};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {{
    let messenger = Messenger::connect(\"{}\", {}).await?;

    let goal = brain_move_arm::fire_goal(&messenger, Duration::from_secs(5), 7, [10, 20, 30]).await?;
    println!(\"goal accepted={{}}\", goal.accepted);

    tokio::time::sleep(Duration::from_millis(200)).await;

    let cancel_response = brain_move_arm::cancel_goal(&messenger, Duration::from_secs(5)).await?;
    let error_msg = cancel_response.error_message.as_deref().unwrap_or(\"<none>\");
    println!(
        \"cancel accepted={{}} error={{}}\",
        cancel_response.accepted,
        error_msg
    );

    Ok(())
}}
",
        router_host, router_port
    );
    let main_file = user_node_subscriber.join("src").join("main.rs");
    fs::write(main_file, subscriber_main).expect("failed to write subscriber main");

    // --- Exposer (server) project
    let temp_dir_exposer = TempDir::new().unwrap();
    let exposed_action: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE).unwrap();
    let (mut generator, output_dir_exposer, user_node_exposer) = init_test_env(&temp_dir_exposer);
    generator.add_exposed_action(&exposed_action).unwrap();
    let output_config = copy_config_to_output(&user_node_exposer, &output_dir_exposer);
    generator.build(&output_dir_exposer).unwrap();
    fs::remove_file(output_config).unwrap();
    init_cargo_user_node(&user_node_exposer);
    let exposer_main = format!(
        "
use peppygen::exposed_actions::move_arm;
use peppygen::{{Messenger, Result}};

#[tokio::main]
async fn main() -> Result<()> {{
    let messenger = Messenger::connect(\"{}\", {}).await?;

    move_arm::handle_goal_next_request(&messenger, |request| -> Result<move_arm::GoalResponse> {{
        println!(
            \"server received goal arm_id={{}} desired={{:?}}\",
            request.arm_id,
            request.desired_position
        );
        Ok(move_arm::GoalResponse::new(true))
    }})
    .await?;
    println!(\"server handled goal request\");

    let cancel_error = \"goal cancelled by server\";

    move_arm::handle_cancel_next_request(&messenger, || -> Result<move_arm::CancelResponse> {{
        println!(\"server received cancel request\");
        Ok(move_arm::CancelResponse::new(
            false,
            Some(cancel_error.to_owned()),
        ))
    }})
    .await?;

    println!(\"server responded to cancel request error={{}}\", cancel_error);

    Ok(())
}}
",
        router_host, router_port
    );
    let main_file = user_node_exposer.join("src").join("main.rs");
    fs::write(main_file, exposer_main).expect("failed to write exposer main");

    compile_project(&user_node_subscriber);
    compile_project(&user_node_exposer);

    let exposer_dir = user_node_exposer.clone();
    let exposer_thread =
        thread::spawn(move || run_cargo_run(&exposer_dir, Some(Duration::from_secs(15)), &[]));

    thread::sleep(Duration::from_millis(500));

    let subscriber_dir = user_node_subscriber.clone();
    let subscriber_thread =
        thread::spawn(move || run_cargo_run(&subscriber_dir, Some(Duration::from_secs(15)), &[]));

    let exposer_output = exposer_thread.join().expect("exposer thread panicked");
    let subscriber_output = subscriber_thread
        .join()
        .expect("subscriber thread panicked");

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

    rt.block_on(async {
        router
            .stop_router()
            .await
            .expect("failed to stop zenoh router");
    });
}
