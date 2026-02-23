use crate::helpers::{
    STUB_NODE_CONFIG, WaitContext, compile_project, copy_config_to_output, init_cargo_user_node,
    init_test_env, send_shutdown, spawn_cargo_run, test_peppy_dirs, wait_for_child,
    wait_for_health_service_reachable_or_exit,
};
use config::consts::{PEPPYGEN_OUTPUT_PATH, RUNTIME_CONFIG_VAR_NAME};
use config::runtime::NodeInstance;
use config::{
    node::{ExposedTopic, MessageFormat, SubscribedTopic},
    peppy_config::Name,
    runtime::RuntimeConfig,
};
use generator::LanguageGenerator;
use std::path::Path;
use std::{fs, time::Duration};
use tempfile::TempDir;

// --- Common test constants
const TEST_DAEMON_NODE: &str = "test_daemon";
const SUBSCRIBER_NODE_NAME: &str = "subscriber_node";
const SUBSCRIBER_INSTANCE_ID: &str = "subscriber_instance";
const EXPOSER_INSTANCE_ID: &str = "exposer_instance";
const SHUTDOWN_SENDER_INSTANCE_ID: &str = "test_shutdown_sender";
const UVC_CAMERA_NODE_NAME: &str = "uvc_camera";

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
    let instance = pmi::ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    // --- Subscriber project
    let subscriber_instance_id = SUBSCRIBER_INSTANCE_ID;
    let temp_dir_proj2 = TempDir::new().unwrap();
    let subscribed_topic: SubscribedTopic =
        serde_json5::from_str(SUBSCRIBED_TOPIC_EXAMPLE).unwrap();
    let subscribed_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE).unwrap();
    let (mut generator, subscriber_dir, user_node_subscriber, peppy_node_config_path) =
        init_test_env::<generator::RustGenerator>(&temp_dir_proj2, STUB_NODE_CONFIG);
    generator
        .add_subscribed_topic(&subscribed_topic, subscribed_format)
        .unwrap();
    let output_config = copy_config_to_output(&user_node_subscriber, &subscriber_dir);
    generator
        .build(&subscriber_dir, &test_peppy_dirs())
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
        TEST_DAEMON_NODE,
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
        init_test_env::<generator::RustGenerator>(&temp_dir_proj1, STUB_NODE_CONFIG);
    let exposer_parameters: config::NodeArguments =
        serde_json5::from_str(r#"{ frequency: "f64" }"#).unwrap();
    generator.set_parameters(exposer_parameters.clone());
    generator.add_exposed_topic(&exposed_topic).unwrap();
    let output_config = copy_config_to_output(&user_node_exposer, &exposer_dir);
    generator.build(&exposer_dir, &test_peppy_dirs()).unwrap();
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
    config::fingerprint::create_codegen_fingerprint(
        &peppy_node_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let exposer_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstance {
            instance_id: Name::new(exposer_instance_id).unwrap(),
            arguments: serde_json5::from_str(r#"{ frequency: 10.0 }"#).unwrap(),
        },
        UVC_CAMERA_NODE_NAME, // Must match the node name expected by the subscriber
        TEST_DAEMON_NODE,
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
        bound_daemon_node: TEST_DAEMON_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_daemon_node: Some(TEST_DAEMON_NODE),
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
        UVC_CAMERA_NODE_NAME,
        exposer_instance_id,
        &mut exposer_child,
        &user_node_exposer,
    )
    .await;

    send_shutdown(
        &messenger,
        TEST_DAEMON_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        SUBSCRIBER_NODE_NAME,
        Some(TEST_DAEMON_NODE),
        subscriber_instance_id,
        Duration::from_secs(5),
    )
    .await;
    send_shutdown(
        &messenger,
        TEST_DAEMON_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        UVC_CAMERA_NODE_NAME,
        Some(TEST_DAEMON_NODE),
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
}
