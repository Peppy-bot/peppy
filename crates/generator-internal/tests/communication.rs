mod helpers;

use config::consts::RUNTIME_CONFIG_VAR_NAME;
use config::{
    node::{
        ExposedAction, ExposedService, ExposedTopic, MessageFormat, SubscribedAction,
        SubscribedService, SubscribedTopic,
    },
    peppy_config::{DeploymentInstance, Name},
    runtime::RuntimeConfig,
};
use generator::{LanguageGenerator, SubscribedActionMessage};
use helpers::{
    WaitContext, compile_project, copy_config_to_output, init_cargo_user_node, init_test_env,
    send_shutdown, spawn_cargo_run, wait_for_action_service_reachable_or_exit, wait_for_child,
    wait_for_health_service_reachable_or_exit, wait_for_service_reachable_or_exit,
    write_codegen_fingerprint,
};
use pmi::MessengerBackend;
use std::{fs, time::Duration};
use tempfile::TempDir;

// --- Common test constants
const TEST_MASTER_NODE: &str = "test_master";
const SUBSCRIBER_NODE_NAME: &str = "subscriber_node";
const SUBSCRIBER_INSTANCE_ID: &str = "subscriber_instance";
const EXPOSER_INSTANCE_ID: &str = "exposer_instance";
const SHUTDOWN_SENDER_INSTANCE_ID: &str = "test_shutdown_sender";
const UVC_CAMERA_NODE_NAME: &str = "uvc_camera";
const BRAIN_NODE_NAME: &str = "brain";

// --- Topics exposes and its corresponding subscriber
const EXPOSED_TOPIC_EXAMPLE: &str = r#"
{
  name: "video_stream",
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
    frame: {
      $type: "array",
      $items: "u8"
    }
  }
}
"#;

const SUBSCRIBED_TOPIC_EXAMPLE: &str = r#"
{
  id: "camera_frame",
  node: "uvc_camera",
  name: "video_stream",
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
  frame: {
    $type: "array",
    $items: "u8"
  }
}
"#;

/// Creates 2 projects in separate directory and check if they can send/receive topics
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn topics_communication() {
    let (mut router, _dir, router_host, router_port) =
        peppylib::start_zenohd_process("127.0.0.1", None)
            .await
            .expect("failed to start zenoh router for test");

    // --- Subscriber project
    let subscriber_instance_id = SUBSCRIBER_INSTANCE_ID;
    let temp_dir_proj2 = TempDir::new().unwrap();
    let subscribed_topic: SubscribedTopic =
        serde_json5::from_str(SUBSCRIBED_TOPIC_EXAMPLE).unwrap();
    let subscribed_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE).unwrap();
    let (mut generator, subscriber_dir, user_node_subscriber, peppy_node_config_path) =
        init_test_env(&temp_dir_proj2);
    generator
        .add_subscribed_topic(&subscribed_topic, subscribed_format)
        .unwrap();
    let output_config = copy_config_to_output(&user_node_subscriber, &subscriber_dir);
    generator.build(&subscriber_dir).unwrap();
    fs::remove_file(output_config).unwrap();
    write_codegen_fingerprint(&peppy_node_config_path);

    let subscriber_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        DeploymentInstance {
            instance_id: Name::new(subscriber_instance_id).unwrap(),
            arguments: Default::default(),
        },
        SUBSCRIBER_NODE_NAME,
        TEST_MASTER_NODE,
    )
    .unwrap();
    let subscriber_runtime_config_path = temp_dir_proj2.path().join("peppy_runtime.json5");
    subscriber_runtime_config
        .save_json5_launch_config(&subscriber_runtime_config_path)
        .unwrap();

    init_cargo_user_node(&user_node_subscriber);
    // TODO: An exit signal should be sent to the subscriber to terminate the process
    let subscriber_main = r#"
use peppygen::NodeBuilder;
use peppygen::subscribed_topics::uvc_camera_video_stream::on_next_message_received;
use peppygen::Result;

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        let (instance_id, frame) = on_next_message_received(&node_runner, None, None).await?;
        println!(
            "got {}x{} frame encoded as {} from {}",
            frame.width, frame.height, frame.encoding, &instance_id
        );
        Ok(())
    })
    }
 "#;
    let main_file = user_node_subscriber.join("src").join("main.rs");
    fs::write(main_file, subscriber_main).expect("failed to write main file");

    // --- Exposer project
    let exposer_instance_id = EXPOSER_INSTANCE_ID;
    let temp_dir_proj1 = TempDir::new().unwrap();
    let exposed_topic: ExposedTopic = serde_json5::from_str(EXPOSED_TOPIC_EXAMPLE).unwrap();
    let (mut generator, exposer_dir, user_node_exposer, peppy_node_config_path) =
        init_test_env(&temp_dir_proj1);
    let exposer_parameters: config::NodeArguments =
        serde_json5::from_str(r#"{ frequency: "f64" }"#).unwrap();
    generator.set_parameters(exposer_parameters.clone());
    generator.add_exposed_topic(&exposed_topic).unwrap();
    let output_config = copy_config_to_output(&user_node_exposer, &exposer_dir);
    generator.build(&exposer_dir).unwrap();
    fs::remove_file(output_config).unwrap();

    // Update the peppy node config to include the parameters schema
    let mut node_config: config::node::NodeConfig =
        serde_json5::from_str(&fs::read_to_string(&peppy_node_config_path).unwrap()).unwrap();
    node_config.parameters = exposer_parameters;
    fs::write(
        &peppy_node_config_path,
        serde_json5::to_string(&node_config).unwrap(),
    )
    .unwrap();
    write_codegen_fingerprint(&peppy_node_config_path);

    let exposer_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        DeploymentInstance {
            instance_id: Name::new(exposer_instance_id).unwrap(),
            arguments: serde_json5::from_str(r#"{ frequency: 10.0 }"#).unwrap(),
        },
        UVC_CAMERA_NODE_NAME, // Must match the node name expected by the subscriber
        TEST_MASTER_NODE,
    )
    .unwrap();
    let exposer_runtime_config_path = temp_dir_proj1.path().join("peppy_runtime.json5");
    exposer_runtime_config
        .save_json5_launch_config(&exposer_runtime_config_path)
        .unwrap();

    init_cargo_user_node(&user_node_exposer);
    // TODO: An exit signal should be sent to the exposer to terminate the process
    let exposer_main = r#"
use peppygen::exposed_topics::video_stream;
use peppygen::NodeBuilder;
use peppygen::Result;
use std::time::Duration;

fn main() -> Result<()> {
    NodeBuilder::new().run(|parameters: peppygen::Parameters, node_runner| async move {
        let frequency_hz: f64 = parameters.frequency;
        let interval = Duration::from_secs_f64(1.0 / frequency_hz);

        let node_runner_clone = node_runner.clone();
        tokio::spawn(async move {
            let mut frame_id = 0u32;
            loop {
                let _ = video_stream::emit(
                    &node_runner_clone,
                    video_stream::MessageHeader {
                        stamp: std::time::SystemTime::now(),
                        frame_id,
                    },
                    "rgb8".to_owned(),
                    640,
                    480,
                    vec![1, 2, 3],
                )
                .await;

                frame_id = frame_id.wrapping_add(1);
                tokio::time::sleep(interval).await;
            }
        });

        Ok(())
    })
    }
 "#;

    let main_file = user_node_exposer.join("src").join("main.rs");
    fs::write(main_file, exposer_main).expect("failed to write main file");

    let user_node_subscriber_config_str =
        subscriber_runtime_config_path.to_str().unwrap().to_owned();
    let user_node_exposer_runtime_config_str =
        exposer_runtime_config_path.to_str().unwrap().to_owned();

    compile_project(&user_node_subscriber);
    compile_project(&user_node_exposer);

    // Spawn both processes
    let mut subscriber_child = spawn_cargo_run(
        &user_node_subscriber,
        &[(RUNTIME_CONFIG_VAR_NAME, &user_node_subscriber_config_str)],
    );
    let mut exposer_child = spawn_cargo_run(
        &user_node_exposer,
        &[(
            RUNTIME_CONFIG_VAR_NAME,
            &user_node_exposer_runtime_config_str,
        )],
    );

    // Wait until both nodes have completed their setup_fn (node_health is reachable).
    // (The subscriber reaches this point only after it receives a frame.)
    let messenger = peppylib::MessengerHandle::from_host_port(&router_host, router_port)
        .await
        .expect("failed to create messenger for shutdown");
    let ctx = WaitContext {
        messenger: &messenger,
        bound_master_node: TEST_MASTER_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_master_node: Some(TEST_MASTER_NODE),
    };
    wait_for_health_service_reachable_or_exit(
        &ctx,
        SUBSCRIBER_NODE_NAME,
        subscriber_instance_id,
        &mut subscriber_child,
        &user_node_subscriber,
        Duration::from_secs(10),
    )
    .await;
    wait_for_health_service_reachable_or_exit(
        &ctx,
        UVC_CAMERA_NODE_NAME,
        exposer_instance_id,
        &mut exposer_child,
        &user_node_exposer,
        Duration::from_secs(10),
    )
    .await;

    send_shutdown(
        &messenger,
        TEST_MASTER_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        SUBSCRIBER_NODE_NAME,
        Some(TEST_MASTER_NODE),
        subscriber_instance_id,
        Duration::from_secs(5),
    )
    .await;
    send_shutdown(
        &messenger,
        TEST_MASTER_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        UVC_CAMERA_NODE_NAME,
        Some(TEST_MASTER_NODE),
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

    router
        .stop_router()
        .await
        .expect("failed to stop zenoh router");
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
  id: "uvc_camera_enable_camera",
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

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn services_communication_no_target_instance_id() {
    let (mut router, _dir, router_host, router_port) =
        peppylib::start_zenohd_process("127.0.0.1", None)
            .await
            .expect("failed to start zenoh router for test");

    // --- Subscriber (client) project
    let subscriber_instance_id = "the_subscriber";
    let temp_dir_subscriber = TempDir::new().unwrap();
    let subscribed_service: SubscribedService =
        serde_json5::from_str(SUBSCRIBED_SERVICE_EXAMPLE).unwrap();
    let subscribed_request_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_SERVICE_REQUEST_FORMAT_EXAMPLE).unwrap();
    let subscribed_response_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_SERVICE_RESPONSE_FORMAT_EXAMPLE).unwrap();
    let (mut generator, output_dir_subscriber, user_node_subscriber, peppy_node_config_path) =
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
    write_codegen_fingerprint(&peppy_node_config_path);

    let subscriber_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        DeploymentInstance {
            instance_id: Name::new(subscriber_instance_id).unwrap(),
            arguments: Default::default(),
        },
        SUBSCRIBER_NODE_NAME,
        TEST_MASTER_NODE,
    )
    .unwrap();
    let subscriber_runtime_config_path = temp_dir_subscriber.path().join("peppy_runtime.json5");
    subscriber_runtime_config
        .save_json5_launch_config(&subscriber_runtime_config_path)
        .unwrap();

    init_cargo_user_node(&user_node_subscriber);
    let subscriber_main = r#"
use peppygen::subscribed_services::uvc_camera_enable_camera;
use peppygen::NodeBuilder;
use peppygen::Result;
use std::time::Duration;

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        let request = uvc_camera_enable_camera::Request::new(true);
        let response =
            uvc_camera_enable_camera::poll(&node_runner, Duration::from_secs(5), None, None, request).await?;
        let error_msg = response.data.error_msg.as_deref().unwrap_or("<none>");
        println!(
            "enable_camera result: service_id={} enabled={} error={}",
            &response.instance_id,
            response.data.enabled,
            error_msg
        );

        Ok(())
    })
}
"#;
    let main_file = user_node_subscriber.join("src").join("main.rs");
    fs::write(main_file, subscriber_main).expect("failed to write subscriber main");

    // --- Exposer (server) project
    let exposer_instance_id = "the_exposer";
    let temp_dir_exposer = TempDir::new().unwrap();
    let exposed_service: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE).unwrap();
    let (mut generator, output_dir_exposer, user_node_exposer, peppy_node_config_path) =
        init_test_env(&temp_dir_exposer);
    generator.add_exposed_service(&exposed_service).unwrap();
    let output_config = copy_config_to_output(&user_node_exposer, &output_dir_exposer);
    generator.build(&output_dir_exposer).unwrap();
    fs::remove_file(output_config).unwrap();
    write_codegen_fingerprint(&peppy_node_config_path);

    let exposer_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        DeploymentInstance {
            instance_id: Name::new(exposer_instance_id).unwrap(),
            arguments: Default::default(),
        },
        UVC_CAMERA_NODE_NAME,
        TEST_MASTER_NODE,
    )
    .unwrap();
    let exposer_runtime_config_path = temp_dir_exposer.path().join("peppy_runtime.json5");
    exposer_runtime_config
        .save_json5_launch_config(&exposer_runtime_config_path)
        .unwrap();

    init_cargo_user_node(&user_node_exposer);
    let exposer_main = r#"
use peppygen::exposed_services::enable_camera;
use peppygen::NodeBuilder;
use peppygen::Result;

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        enable_camera::handle_next_request(&node_runner, |request| -> Result<enable_camera::Response> {
            println!("received enable_camera request from {}: enable = {}", request.instance_id, request.data.enable);
            Ok(enable_camera::Response::new(
                request.data.enable,
                Some("handled".to_owned()),
            ))
        })
        .await?;

        println!("enable_camera handler finished");

        Ok(())
    })
}
"#;
    let main_file = user_node_exposer.join("src").join("main.rs");
    fs::write(main_file, exposer_main).expect("failed to write exposer main");

    compile_project(&user_node_subscriber);
    compile_project(&user_node_exposer);

    let user_node_exposer_runtime_config_str =
        exposer_runtime_config_path.to_str().unwrap().to_owned();
    let user_node_subscriber_config_str =
        subscriber_runtime_config_path.to_str().unwrap().to_owned();

    let messenger = peppylib::MessengerHandle::from_host_port(&router_host, router_port)
        .await
        .expect("failed to create messenger for test control");
    let ctx = WaitContext {
        messenger: &messenger,
        bound_master_node: TEST_MASTER_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_master_node: Some(TEST_MASTER_NODE),
    };

    // Spawn exposer first so it's ready to handle requests
    let mut exposer_child = spawn_cargo_run(
        &user_node_exposer,
        &[(
            RUNTIME_CONFIG_VAR_NAME,
            &user_node_exposer_runtime_config_str,
        )],
    );

    wait_for_service_reachable_or_exit(
        &ctx,
        UVC_CAMERA_NODE_NAME,
        "enable_camera",
        Some(exposer_instance_id),
        &mut exposer_child,
        &user_node_exposer,
        Duration::from_secs(10),
    )
    .await;

    let mut subscriber_child = spawn_cargo_run(
        &user_node_subscriber,
        &[(RUNTIME_CONFIG_VAR_NAME, &user_node_subscriber_config_str)],
    );

    wait_for_health_service_reachable_or_exit(
        &ctx,
        SUBSCRIBER_NODE_NAME,
        subscriber_instance_id,
        &mut subscriber_child,
        &user_node_subscriber,
        Duration::from_secs(10),
    )
    .await;
    wait_for_health_service_reachable_or_exit(
        &ctx,
        UVC_CAMERA_NODE_NAME,
        exposer_instance_id,
        &mut exposer_child,
        &user_node_exposer,
        Duration::from_secs(10),
    )
    .await;

    send_shutdown(
        &messenger,
        TEST_MASTER_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        SUBSCRIBER_NODE_NAME,
        Some(TEST_MASTER_NODE),
        subscriber_instance_id,
        Duration::from_secs(5),
    )
    .await;
    send_shutdown(
        &messenger,
        TEST_MASTER_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        UVC_CAMERA_NODE_NAME,
        Some(TEST_MASTER_NODE),
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
    let expected_subscriber_log = format!(
        "enable_camera result: service_id={} enabled=true error=handled",
        exposer_instance_id
    );
    assert!(
        subscriber_stdout.contains(&expected_subscriber_log),
        "subscriber did not receive expected service response (expected log: {}).\nstdout:\n{}\nstderr:\n{}",
        expected_subscriber_log,
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
    let expected_request_log = format!(
        "received enable_camera request from {}: enable = true",
        subscriber_instance_id
    );
    assert!(
        exposer_stdout.contains(&expected_request_log)
            && exposer_stdout.contains("enable_camera handler finished"),
        "exposer did not process the enable_camera request.\nstdout:\n{}\nstderr:\n{}",
        exposer_stdout,
        exposer_stderr
    );

    router
        .stop_router()
        .await
        .expect("failed to stop zenoh router");
}

/// If there are multiple services of the same name and the subscriber does not specify an instance_id, it's the first service that respond that connects with the subscriber
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn services_communication_multiple_exposed_instances_same_service_not_target_instance_id() {
    let (mut router, _dir, router_host, router_port) =
        peppylib::start_zenohd_process("127.0.0.1", None)
            .await
            .expect("failed to start zenoh router for test");

    // --- Subscriber (client) project
    let subscriber_instance_id = SUBSCRIBER_INSTANCE_ID;
    let temp_dir_subscriber = TempDir::new().unwrap();
    let subscribed_service: SubscribedService =
        serde_json5::from_str(SUBSCRIBED_SERVICE_EXAMPLE).unwrap();
    let subscribed_request_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_SERVICE_REQUEST_FORMAT_EXAMPLE).unwrap();
    let subscribed_response_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_SERVICE_RESPONSE_FORMAT_EXAMPLE).unwrap();
    let (mut generator, output_dir_subscriber, user_node_subscriber, peppy_node_config_path) =
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
    write_codegen_fingerprint(&peppy_node_config_path);

    let subscriber_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        DeploymentInstance {
            instance_id: Name::new(subscriber_instance_id).unwrap(),
            arguments: Default::default(),
        },
        SUBSCRIBER_NODE_NAME,
        TEST_MASTER_NODE,
    )
    .unwrap();
    let subscriber_runtime_config_path = temp_dir_subscriber.path().join("peppy_runtime.json5");
    subscriber_runtime_config
        .save_json5_launch_config(&subscriber_runtime_config_path)
        .unwrap();

    init_cargo_user_node(&user_node_subscriber);
    let subscriber_main = r#"
use peppygen::subscribed_services::uvc_camera_enable_camera;
use peppygen::NodeBuilder;
use peppygen::Result;
use std::time::Duration;

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        let request = uvc_camera_enable_camera::Request::new(true);
        let response =
            uvc_camera_enable_camera::poll(&node_runner, Duration::from_secs(5), None, None, request).await?;
        let error_msg = response.data.error_msg.as_deref().unwrap_or("<none>");
        println!(
            "enable_camera result: enabled={} error={}",
            response.data.enabled,
            error_msg
        );

        Ok(())
    })
}
"#;
    let main_file = user_node_subscriber.join("src").join("main.rs");
    fs::write(main_file, subscriber_main).expect("failed to write subscriber main");

    // --- Exposer 1
    let exposer1_instance_id = "exposer1_instance";
    let temp_dir_exposer1 = TempDir::new().unwrap();
    let exposed_service: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE).unwrap();
    let (mut generator, output_dir_exposer1, user_node_exposer1, peppy_node_config_path) =
        init_test_env(&temp_dir_exposer1);
    generator.add_exposed_service(&exposed_service).unwrap();
    let output_config = copy_config_to_output(&user_node_exposer1, &output_dir_exposer1);
    generator.build(&output_dir_exposer1).unwrap();
    fs::remove_file(output_config).unwrap();
    write_codegen_fingerprint(&peppy_node_config_path);

    let exposer1_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        DeploymentInstance {
            instance_id: Name::new(exposer1_instance_id).unwrap(),
            arguments: Default::default(),
        },
        UVC_CAMERA_NODE_NAME,
        TEST_MASTER_NODE,
    )
    .unwrap();
    let exposer1_runtime_config_path = temp_dir_exposer1.path().join("peppy_runtime.json5");
    exposer1_runtime_config
        .save_json5_launch_config(&exposer1_runtime_config_path)
        .unwrap();

    init_cargo_user_node(&user_node_exposer1);
    let exposer1_main = r#"
use peppygen::exposed_services::enable_camera;
use peppygen::NodeBuilder;
use peppygen::Result;

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        enable_camera::handle_next_request(&node_runner, |request| -> Result<enable_camera::Response> {
            println!("received enable_camera request for {}: {}", request.instance_id, request.data.enable);
            Ok(enable_camera::Response::new(
                request.data.enable,
                Some("handled".to_owned()),
            ))
        })
        .await?;

        println!("enable_camera handler finished");

        Ok(())
    })
}
"#;
    let main_file = user_node_exposer1.join("src").join("main.rs");
    fs::write(main_file, exposer1_main).expect("failed to write exposer main 1");

    // --- Exposer 2
    let exposer2_instance_id = "exposer2_instance";
    let temp_dir_exposer2 = TempDir::new().unwrap();
    let exposed_service2: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE).unwrap();
    let (mut generator, output_dir_exposer2, user_node_exposer2, peppy_node_config_path) =
        init_test_env(&temp_dir_exposer2);
    generator.add_exposed_service(&exposed_service2).unwrap();
    let output_config = copy_config_to_output(&user_node_exposer2, &output_dir_exposer2);
    generator.build(&output_dir_exposer2).unwrap();
    fs::remove_file(output_config).unwrap();
    write_codegen_fingerprint(&peppy_node_config_path);

    let exposer2_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        DeploymentInstance {
            instance_id: Name::new(exposer2_instance_id).unwrap(),
            arguments: Default::default(),
        },
        UVC_CAMERA_NODE_NAME,
        TEST_MASTER_NODE,
    )
    .unwrap();
    let exposer2_runtime_config_path = temp_dir_exposer2.path().join("peppy_runtime.json5");
    exposer2_runtime_config
        .save_json5_launch_config(&exposer2_runtime_config_path)
        .unwrap();

    init_cargo_user_node(&user_node_exposer2);
    let exposer2_main = r#"
use peppygen::exposed_services::enable_camera;
use peppygen::NodeBuilder;
use peppygen::Result;
use std::time::Duration;

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        enable_camera::handle_next_request(&node_runner, |request| -> Result<enable_camera::Response> {
            println!("received enable_camera request for {}: {}", request.instance_id, request.data.enable);
            // Sleep to ensure exposer1 responds first
            std::thread::sleep(Duration::from_secs(2));
            Ok(enable_camera::Response::new(
                request.data.enable,
                Some("handled_by_exposer2".to_owned()),
            ))
        })
        .await?;

        println!("enable_camera handler finished");

        Ok(())
    })
}
"#;
    let main_file = user_node_exposer2.join("src").join("main.rs");
    fs::write(main_file, exposer2_main).expect("failed to write exposer main 2");

    // Compilation + execution
    compile_project(&user_node_subscriber);
    compile_project(&user_node_exposer1);
    compile_project(&user_node_exposer2);

    let exposer1_runtime_config_str = exposer1_runtime_config_path.to_str().unwrap().to_owned();
    let exposer2_runtime_config_str = exposer2_runtime_config_path.to_str().unwrap().to_owned();
    let subscriber_runtime_config_str = subscriber_runtime_config_path.to_str().unwrap().to_owned();

    let messenger = peppylib::MessengerHandle::from_host_port(&router_host, router_port)
        .await
        .expect("failed to create messenger for test control");
    let ctx = WaitContext {
        messenger: &messenger,
        bound_master_node: TEST_MASTER_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_master_node: Some(TEST_MASTER_NODE),
    };

    // Spawn both exposers first so they're ready to handle requests
    let mut exposer1_child = spawn_cargo_run(
        &user_node_exposer1,
        &[(RUNTIME_CONFIG_VAR_NAME, &exposer1_runtime_config_str)],
    );
    let mut exposer2_child = spawn_cargo_run(
        &user_node_exposer2,
        &[(RUNTIME_CONFIG_VAR_NAME, &exposer2_runtime_config_str)],
    );

    wait_for_service_reachable_or_exit(
        &ctx,
        UVC_CAMERA_NODE_NAME,
        "enable_camera",
        Some(exposer1_instance_id),
        &mut exposer1_child,
        &user_node_exposer1,
        Duration::from_secs(10),
    )
    .await;
    wait_for_service_reachable_or_exit(
        &ctx,
        UVC_CAMERA_NODE_NAME,
        "enable_camera",
        Some(exposer2_instance_id),
        &mut exposer2_child,
        &user_node_exposer2,
        Duration::from_secs(10),
    )
    .await;

    let mut subscriber_child = spawn_cargo_run(
        &user_node_subscriber,
        &[(RUNTIME_CONFIG_VAR_NAME, &subscriber_runtime_config_str)],
    );

    wait_for_health_service_reachable_or_exit(
        &ctx,
        SUBSCRIBER_NODE_NAME,
        subscriber_instance_id,
        &mut subscriber_child,
        &user_node_subscriber,
        Duration::from_secs(10),
    )
    .await;
    wait_for_health_service_reachable_or_exit(
        &ctx,
        UVC_CAMERA_NODE_NAME,
        exposer1_instance_id,
        &mut exposer1_child,
        &user_node_exposer1,
        Duration::from_secs(10),
    )
    .await;
    wait_for_health_service_reachable_or_exit(
        &ctx,
        UVC_CAMERA_NODE_NAME,
        exposer2_instance_id,
        &mut exposer2_child,
        &user_node_exposer2,
        Duration::from_secs(10),
    )
    .await;

    send_shutdown(
        &messenger,
        TEST_MASTER_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        SUBSCRIBER_NODE_NAME,
        Some(TEST_MASTER_NODE),
        subscriber_instance_id,
        Duration::from_secs(5),
    )
    .await;
    send_shutdown(
        &messenger,
        TEST_MASTER_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        UVC_CAMERA_NODE_NAME,
        Some(TEST_MASTER_NODE),
        exposer1_instance_id,
        Duration::from_secs(5),
    )
    .await;
    send_shutdown(
        &messenger,
        TEST_MASTER_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        UVC_CAMERA_NODE_NAME,
        Some(TEST_MASTER_NODE),
        exposer2_instance_id,
        Duration::from_secs(5),
    )
    .await;

    // Wait for all processes to exit
    let subscriber_output = wait_for_child(
        &mut subscriber_child,
        Some(Duration::from_secs(10)),
        &user_node_subscriber,
    );
    let exposer_output1 = wait_for_child(
        &mut exposer1_child,
        Some(Duration::from_secs(10)),
        &user_node_exposer1,
    );
    let exposer_output2 = wait_for_child(
        &mut exposer2_child,
        Some(Duration::from_secs(10)),
        &user_node_exposer2,
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

    let exposer1_stdout = String::from_utf8_lossy(&exposer_output1.stdout).into_owned();
    let exposer1_stderr = String::from_utf8_lossy(&exposer_output1.stderr).into_owned();
    let exposer2_stdout = String::from_utf8_lossy(&exposer_output2.stdout).into_owned();
    let exposer2_stderr = String::from_utf8_lossy(&exposer_output2.stderr).into_owned();

    // Both exposers should have received the request
    let expected_request_log = format!(
        "received enable_camera request for {}: true",
        subscriber_instance_id
    );
    assert!(
        exposer1_stdout.contains(&expected_request_log)
            && exposer1_stdout.contains("enable_camera handler finished"),
        "exposer1 did not process the enable_camera request.\nstdout:\n{}\nstderr:\n{}",
        exposer1_stdout,
        exposer1_stderr
    );
    assert!(
        exposer2_stdout.contains(&expected_request_log)
            && exposer2_stdout.contains("enable_camera handler finished"),
        "exposer2 did not process the enable_camera request.\nstdout:\n{}\nstderr:\n{}",
        exposer2_stdout,
        exposer2_stderr
    );

    // Subscriber should have received a response from exposer1 (the faster responder)
    assert!(
        subscriber_stdout.contains("enable_camera result: enabled=true error=handled"),
        "subscriber should have received response from exposer1 (the faster responder), not exposer2.\nstdout:\n{}\nstderr:\n{}",
        subscriber_stdout,
        subscriber_stderr
    );

    router
        .stop_router()
        .await
        .expect("failed to stop zenoh router");
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
    let (mut router, _dir, router_host, router_port) =
        peppylib::start_zenohd_process("127.0.0.1", None)
            .await
            .expect("failed to start zenoh router for test");

    // --- Subscriber (client) project
    let subscriber_instance_id = SUBSCRIBER_INSTANCE_ID;
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
    let (mut generator, output_dir_subscriber, user_node_subscriber, peppy_node_config_path) =
        init_test_env(&temp_dir_subscriber);
    generator
        .add_subscribed_action(&subscribed_action, Some(&action_messages))
        .unwrap();
    let output_config = copy_config_to_output(&user_node_subscriber, &output_dir_subscriber);
    generator.build(&output_dir_subscriber).unwrap();
    fs::remove_file(output_config).unwrap();
    write_codegen_fingerprint(&peppy_node_config_path);

    let subscriber_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        DeploymentInstance {
            instance_id: Name::new(subscriber_instance_id).unwrap(),
            arguments: Default::default(),
        },
        SUBSCRIBER_NODE_NAME,
        TEST_MASTER_NODE,
    )
    .unwrap();
    let subscriber_runtime_config_path = temp_dir_subscriber.path().join("peppy_runtime.json5");
    subscriber_runtime_config
        .save_json5_launch_config(&subscriber_runtime_config_path)
        .unwrap();

    init_cargo_user_node(&user_node_subscriber);
    let subscriber_main = r#"
use peppygen::subscribed_actions::brain_move_arm;
use peppygen::NodeBuilder;
use peppygen::Result;
use std::time::Duration;

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        let request = brain_move_arm::GoalRequest {
            arm_id: 7,
            desired_position: [10, 20, 30],
        };
        let mut goal = brain_move_arm::fire_goal(
            &node_runner,
            Duration::from_secs(5),
            None,
            None,
            request,
            peppygen::QoSProfile::SensorData,
        ).await?;
        println!("goal accepted={}", goal.data.accepted);

        let feedback = brain_move_arm::on_next_feedback_message(&mut goal.action_handle).await?;
        assert_eq!(feedback.new_position, [7, 31, 43], "unexpected feedback message");
        println!("feedback message received new_position={:?}", feedback.new_position);

        let result = brain_move_arm::get_result(&node_runner, &goal.action_handle, Duration::from_secs(5)).await?;
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
        init_test_env(&temp_dir_exposer);
    generator.add_exposed_action(&exposed_action).unwrap();
    let output_config = copy_config_to_output(&user_node_exposer, &output_dir_exposer);
    generator.build(&output_dir_exposer).unwrap();
    fs::remove_file(output_config).unwrap();
    write_codegen_fingerprint(&peppy_node_config_path);

    let exposer_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        DeploymentInstance {
            instance_id: Name::new(exposer_instance_id).unwrap(),
            arguments: Default::default(),
        },
        BRAIN_NODE_NAME, // Must match the node name expected by the subscriber
        TEST_MASTER_NODE,
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
        bound_master_node: TEST_MASTER_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_master_node: None,
    };
    wait_for_action_service_reachable_or_exit(
        &action_ctx,
        BRAIN_NODE_NAME,
        "move_arm/goal",
        None,
        &mut exposer_child,
        &user_node_exposer,
        Duration::from_secs(15),
    )
    .await;

    let mut subscriber_child = spawn_cargo_run(
        &user_node_subscriber,
        &[(RUNTIME_CONFIG_VAR_NAME, &subscriber_runtime_config_str)],
    );

    let ctx = WaitContext {
        messenger: &messenger,
        bound_master_node: TEST_MASTER_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_master_node: Some(TEST_MASTER_NODE),
    };
    wait_for_health_service_reachable_or_exit(
        &ctx,
        SUBSCRIBER_NODE_NAME,
        subscriber_instance_id,
        &mut subscriber_child,
        &user_node_subscriber,
        Duration::from_secs(15),
    )
    .await;
    wait_for_health_service_reachable_or_exit(
        &ctx,
        BRAIN_NODE_NAME,
        exposer_instance_id,
        &mut exposer_child,
        &user_node_exposer,
        Duration::from_secs(15),
    )
    .await;

    send_shutdown(
        &messenger,
        TEST_MASTER_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        SUBSCRIBER_NODE_NAME,
        Some(TEST_MASTER_NODE),
        subscriber_instance_id,
        Duration::from_secs(5),
    )
    .await;
    send_shutdown(
        &messenger,
        TEST_MASTER_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        BRAIN_NODE_NAME,
        Some(TEST_MASTER_NODE),
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

    router
        .stop_router()
        .await
        .expect("failed to stop zenoh router");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn actions_communication_cancel_goal() {
    let (mut router, _dir, router_host, router_port) =
        peppylib::start_zenohd_process("127.0.0.1", None)
            .await
            .expect("failed to start zenoh router for test");

    // --- Subscriber (client) project
    let subscriber_instance_id = SUBSCRIBER_INSTANCE_ID;
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
    let (mut generator, output_dir_subscriber, user_node_subscriber, peppy_node_config_path) =
        init_test_env(&temp_dir_subscriber);
    generator
        .add_subscribed_action(&subscribed_action, Some(&action_messages))
        .unwrap();
    let output_config = copy_config_to_output(&user_node_subscriber, &output_dir_subscriber);
    generator.build(&output_dir_subscriber).unwrap();
    fs::remove_file(output_config).unwrap();
    write_codegen_fingerprint(&peppy_node_config_path);

    let subscriber_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        DeploymentInstance {
            instance_id: Name::new(subscriber_instance_id).unwrap(),
            arguments: Default::default(),
        },
        SUBSCRIBER_NODE_NAME,
        TEST_MASTER_NODE,
    )
    .unwrap();
    let subscriber_runtime_config_path = temp_dir_subscriber.path().join("peppy_runtime.json5");
    subscriber_runtime_config
        .save_json5_launch_config(&subscriber_runtime_config_path)
        .unwrap();

    init_cargo_user_node(&user_node_subscriber);
    let subscriber_main = r#"
use peppygen::subscribed_actions::brain_move_arm;
use peppygen::NodeBuilder;
use peppygen::Result;
use std::time::Duration;

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        let request = brain_move_arm::GoalRequest {
            arm_id: 7,
            desired_position: [10, 20, 30],
        };
        let goal = brain_move_arm::fire_goal(
            &node_runner,
            Duration::from_secs(5),
            None,
            None,
            request,
            peppygen::QoSProfile::SensorData,
        ).await?;
        println!("goal accepted={}", goal.data.accepted);

        let cancel_response = brain_move_arm::cancel_goal(&node_runner, &goal.action_handle, Duration::from_secs(5)).await?;
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
        init_test_env(&temp_dir_exposer);
    generator.add_exposed_action(&exposed_action).unwrap();
    let output_config = copy_config_to_output(&user_node_exposer, &output_dir_exposer);
    generator.build(&output_dir_exposer).unwrap();
    fs::remove_file(output_config).unwrap();
    write_codegen_fingerprint(&peppy_node_config_path);

    let exposer_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        DeploymentInstance {
            instance_id: Name::new(exposer_instance_id).unwrap(),
            arguments: Default::default(),
        },
        BRAIN_NODE_NAME, // Must match the node name expected by the subscriber
        TEST_MASTER_NODE,
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
        bound_master_node: TEST_MASTER_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_master_node: None,
    };
    wait_for_action_service_reachable_or_exit(
        &action_ctx,
        BRAIN_NODE_NAME,
        "move_arm/goal",
        None,
        &mut exposer_child,
        &user_node_exposer,
        Duration::from_secs(15),
    )
    .await;

    let mut subscriber_child = spawn_cargo_run(
        &user_node_subscriber,
        &[(RUNTIME_CONFIG_VAR_NAME, &subscriber_runtime_config_str)],
    );

    let ctx = WaitContext {
        messenger: &messenger,
        bound_master_node: TEST_MASTER_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_master_node: Some(TEST_MASTER_NODE),
    };
    wait_for_health_service_reachable_or_exit(
        &ctx,
        SUBSCRIBER_NODE_NAME,
        subscriber_instance_id,
        &mut subscriber_child,
        &user_node_subscriber,
        Duration::from_secs(15),
    )
    .await;
    wait_for_health_service_reachable_or_exit(
        &ctx,
        BRAIN_NODE_NAME,
        exposer_instance_id,
        &mut exposer_child,
        &user_node_exposer,
        Duration::from_secs(15),
    )
    .await;

    send_shutdown(
        &messenger,
        TEST_MASTER_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        SUBSCRIBER_NODE_NAME,
        Some(TEST_MASTER_NODE),
        subscriber_instance_id,
        Duration::from_secs(5),
    )
    .await;
    send_shutdown(
        &messenger,
        TEST_MASTER_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        BRAIN_NODE_NAME,
        Some(TEST_MASTER_NODE),
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

    router
        .stop_router()
        .await
        .expect("failed to stop zenoh router");
}
