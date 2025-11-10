use super::*;
use config::node::{ExposedService, ExposedTopic};
use pmi::MessengerBackend;
use std::{fs, thread, time::Duration};
use tempfile::TempDir;

// --- Topics exposes and its corresponding subscriber
const EXPOSED_TOPIC_EXAMPLE: &str = r#"
{
  name: "push_frame",
  qos_profile: "sensor_data",
  message_format: {
    header: {
    type: "object",
    stamp: "time",
    frame_id: "u32"
  },
  encoding: "string",
    width: "u32",
    height: "u32",
    image: {
      type: "array",
      items: "u8",
      length: 3
    }
  }
}
"#;

const SUBSCRIBED_TOPIC_EXAMPLE: &str = r#"
{
  name: "push_frame",
  tag: "0.1.0"
}
"#;

const SUBSCRIBED_TOPIC_FORMAT_EXAMPLE: &str = r#"
{
  header: {
    type: "object",
    stamp: "time",
    frame_id: "u32"
  },
  encoding: "string",
  width: "u32",
  height: "u32",
  image: {
    type: "array",
    items: "u8",
    length: 3
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
        .block_on(peppylib::start_zenohd_process())
        .expect("failed to start zenoh router for test");

    // --- Subscriber project
    let temp_dir_proj2 = TempDir::new().unwrap();
    let subscribed_topic: SubscribedTopic =
        serde_json5::from_str(SUBSCRIBED_TOPIC_EXAMPLE).unwrap();
    let subscribed_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE).unwrap();
    let (mut generator, output_dir2, user_node_subscriber) = init_test_env(&temp_dir_proj2);
    generator
        .add_subscribed_topic(&subscribed_topic, Some(&subscribed_format))
        .unwrap();
    let output_config = copy_config_to_output(&user_node_subscriber, &output_dir2);
    generator.build(&output_dir2).unwrap();
    fs::remove_file(output_config).unwrap();
    init_cargo_user_node(&user_node_subscriber);
    let subscriber_main = format!(
        "
use peppygen::subscribed_topics::push_frame::on_next_message_received;
use peppygen::{{Messenger, Result}};

#[tokio::main]
async fn main() -> Result<()> {{
    let messenger = Messenger::connect(\"{}\", {}).await?;

    let frame = on_next_message_received(&messenger).await?;
    println!(
        \"got {{}}x{{}} frame encoded as {{}}\",
        frame.width, frame.height, frame.encoding
    );

    Ok(())
}}
",
        router_host, router_port
    );
    let main_file = user_node_subscriber.join("src").join("main.rs");
    fs::write(main_file, &subscriber_main).expect("failed to write main file");

    // --- Exposer project
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
use peppygen::exposed_topics::push_frame::{{emit, MessageHeader}};
use peppygen::{{Messenger, Result}};

#[tokio::main]
async fn main() -> Result<()> {{
    let messenger = Messenger::connect(\"{}\", {}).await?;

    emit(
        &messenger,
        MessageHeader {{
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
        thread::spawn(move || run_cargo_run(&subscriber_dir, Some(Duration::from_secs(5))));

    // Give the subscriber a moment to connect before emitting frames.
    thread::sleep(Duration::from_millis(500));

    let exposer_dir = user_node_exposer.clone();
    let exposer_thread = thread::spawn(move || run_cargo_run(&exposer_dir, None));

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
    error_msg: "string"
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
    error_msg: "string"
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
        .block_on(peppylib::start_zenohd_process())
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
    println!(
        \"enable_camera result: enabled={{}} error={{}}\",
        response.enabled,
        response.error_msg
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
        Ok(enable_camera::Response::new(request.enable, \"handled\".to_owned()))
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
        thread::spawn(move || run_cargo_run(&exposer_dir, Some(Duration::from_secs(10))));

    // Give the exposer a moment to start listening before the subscriber sends a request.
    thread::sleep(Duration::from_millis(500));

    let subscriber_dir = user_node_subscriber.clone();
    let subscriber_thread =
        thread::spawn(move || run_cargo_run(&subscriber_dir, Some(Duration::from_secs(10))));

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
        type: "array",
        items: "i32",
        length: 3
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
        type: "array",
        items: "i32",
        length: 3
      }
    }
  },
  result_service: {
    response_message_format: {
      success: "bool",
      error_msg: "string",
      final_position: {
        type: "array",
        items: "i32",
        length: 3
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
    type: "array",
    items: "i32",
    length: 3
  }
}
"#;

const SUBSCRIBED_ACTION_RESULT_FORMAT: &str = r#"
{
  success: "bool",
  error_msg: "string",
  final_position: {
    type: "array",
    items: "i32",
    length: 3
  }
}
"#;

const SUBSCRIBED_ACTION_GOAL_FORMAT: &str = r#"
{
  arm_id: "u16",
  desired_position: {
    type: "array",
    items: "i32",
    length: 3
  }
}
"#;

#[test]
fn actions_communication() {}
