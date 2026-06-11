use crate::helpers::{
    DEFAULT_WAIT_TIMEOUT, STUB_NODE_CONFIG, WaitContext, compile_project, copy_config_to_output,
    init_cargo_user_node, init_test_env, send_shutdown, spawn_cargo_run, test_peppy_dirs,
    wait_for_child, wait_for_health_service_reachable_or_exit,
};
use config::consts::{PEPPYGEN_OUTPUT_PATH, RUNTIME_CONFIG_VAR_NAME};
use config::runtime::NodeInstanceConfig;
use config::{
    launcher::Name,
    node::{ConsumedTopic, EmittedTopic, MessageFormat},
    runtime::RuntimeConfig,
};
use generator::LanguageGenerator;
use std::path::Path;
use std::{fs, time::Duration};
use tempfile::TempDir;

// --- Common test constants
const TEST_CORE_NODE: &str = "test_core";
const RECEIVER_NODE_NAME: &str = "receiver_node";
const RECEIVER_INSTANCE_ID: &str = "receiver_instance";
const EMITTER_INSTANCE_ID: &str = "emitter_instance";
const SHUTDOWN_SENDER_INSTANCE_ID: &str = "test_shutdown_sender";
const UVC_CAMERA_NODE_NAME: &str = "uvc_camera";

// --- Topics emitted and its corresponding receiver
const EMITTED_TOPIC_EXAMPLE: &str = r#"
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
  link_id: "uvc_camera",
  name: "video_stream",
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

/// Creates 2 projects in separate directory and check if they can send/receive topics.
/// Runs under all four transport profiles (peer/router mode × shm on/off).
#[rstest::rstest]
#[case::peer_shm(crate::helpers::TransportProfile::PEER_SHM)]
#[case::router_shm(crate::helpers::TransportProfile::ROUTER_SHM)]
#[case::peer_no_shm(crate::helpers::TransportProfile::PEER_NO_SHM)]
#[case::router_no_shm(crate::helpers::TransportProfile::ROUTER_NO_SHM)]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn topics_communication(#[case] profile: crate::helpers::TransportProfile) {
    let instance = pmi::ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    // --- Receiver project
    let receiver_instance_id = RECEIVER_INSTANCE_ID;
    let temp_dir_proj2 = TempDir::new_in(crate::helpers::test_tmp_root()).unwrap();
    let consumed_topic: ConsumedTopic = serde_json5::from_str(SUBSCRIBED_TOPIC_EXAMPLE).unwrap();
    let subscribed_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE).unwrap();
    let (mut generator, receiver_dir, user_node_receiver, peppy_node_config_path) =
        init_test_env::<generator::RustGenerator>(&temp_dir_proj2, STUB_NODE_CONFIG);
    generator
        .add_consumed_topic(
            &consumed_topic,
            subscribed_format,
            &generator::DependencyContext::native("uvc_camera", "v1"),
        )
        .unwrap();
    let output_config = copy_config_to_output(&user_node_receiver, &receiver_dir);
    generator
        .build(&receiver_dir, &test_peppy_dirs(), Default::default())
        .unwrap();
    fs::remove_file(output_config).unwrap();
    config::fingerprint::create_codegen_fingerprint(
        &peppy_node_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let receiver_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstanceConfig::new(Name::new(receiver_instance_id).unwrap()),
        RECEIVER_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let receiver_runtime_config = crate::helpers::apply_profile(receiver_runtime_config, profile);
    let receiver_runtime_config_path = temp_dir_proj2.path().join("peppy_runtime.json5");
    receiver_runtime_config
        .save_json5_launch_config(&receiver_runtime_config_path)
        .unwrap();

    init_cargo_user_node(&user_node_receiver);
    // TODO: An exit signal should be sent to the receiver to terminate the process
    let receiver_main = r#"
use peppygen::NodeBuilder;
use peppygen::consumed_topics::uvc_camera_video_stream::on_next_message_received;
use peppygen::Result;

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        let (instance_id, frame) = on_next_message_received(&node_runner, None).await?;
        println!(
            "got {}x{} frame encoded as {} from {}",
            frame.width, frame.height, frame.encoding, &instance_id
        );
        Ok(())
    })
}
"#;
    let main_file = user_node_receiver.join("src").join("main.rs");
    fs::write(main_file, receiver_main).expect("failed to write main file");

    // --- Emitter project
    let emitter_instance_id = EMITTER_INSTANCE_ID;
    let temp_dir_proj1 = TempDir::new_in(crate::helpers::test_tmp_root()).unwrap();
    let emitted_topic: EmittedTopic = serde_json5::from_str(EMITTED_TOPIC_EXAMPLE).unwrap();
    let (mut generator, emitter_dir, user_node_emitter, peppy_node_config_path) =
        init_test_env::<generator::RustGenerator>(&temp_dir_proj1, STUB_NODE_CONFIG);
    let emitter_parameters: config::ParameterSchema =
        serde_json5::from_str(r#"{ frequency: "f64" }"#).unwrap();
    generator.set_parameters(emitter_parameters.clone());
    generator.add_emitted_topic(&emitted_topic, None).unwrap();
    let output_config = copy_config_to_output(&user_node_emitter, &emitter_dir);
    generator
        .build(&emitter_dir, &test_peppy_dirs(), Default::default())
        .unwrap();
    fs::remove_file(output_config).unwrap();

    // Update the peppy node config to include the parameters schema
    let mut node_config: config::node::NodeConfig =
        serde_json5::from_str(&fs::read_to_string(&peppy_node_config_path).unwrap()).unwrap();
    node_config.execution.parameters = emitter_parameters;
    fs::write(
        &peppy_node_config_path,
        serde_json5::to_string(&node_config).unwrap(),
    )
    .unwrap();
    config::fingerprint::create_codegen_fingerprint(
        &peppy_node_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let emitter_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstanceConfig {
            arguments: serde_json5::from_str(r#"{ frequency: 10.0 }"#).unwrap(),
            ..NodeInstanceConfig::new(Name::new(emitter_instance_id).unwrap())
        },
        UVC_CAMERA_NODE_NAME, // Must match the node name expected by the receiver
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let emitter_runtime_config = crate::helpers::apply_profile(emitter_runtime_config, profile);
    let emitter_runtime_config_path = temp_dir_proj1.path().join("peppy_runtime.json5");
    emitter_runtime_config
        .save_json5_launch_config(&emitter_runtime_config_path)
        .unwrap();

    init_cargo_user_node(&user_node_emitter);
    // TODO: An exit signal should be sent to the emitter to terminate the process
    let emitter_main = r#"
use peppygen::emitted_topics::video_stream;
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

    let main_file = user_node_emitter.join("src").join("main.rs");
    fs::write(main_file, emitter_main).expect("failed to write main file");

    let user_node_receiver_config_str = receiver_runtime_config_path.to_str().unwrap().to_owned();
    let user_node_emitter_runtime_config_str =
        emitter_runtime_config_path.to_str().unwrap().to_owned();

    compile_project(&user_node_receiver);
    compile_project(&user_node_emitter);

    // Spawn both processes
    let mut receiver_child = spawn_cargo_run(
        &user_node_receiver,
        &[(RUNTIME_CONFIG_VAR_NAME, &user_node_receiver_config_str)],
    );
    let mut emitter_child = spawn_cargo_run(
        &user_node_emitter,
        &[(
            RUNTIME_CONFIG_VAR_NAME,
            &user_node_emitter_runtime_config_str,
        )],
    );

    // Wait until both nodes have completed their setup_fn (node_health is reachable).
    // (The receiver reaches this point only after it receives a frame.)
    let messenger = peppylib::MessengerHandle::from_host_port(&router_host, router_port)
        .await
        .expect("failed to create messenger for shutdown");
    let ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_core_node: Some(TEST_CORE_NODE),
    };
    wait_for_health_service_reachable_or_exit(
        &ctx,
        RECEIVER_NODE_NAME,
        receiver_instance_id,
        &mut receiver_child,
        &user_node_receiver,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;
    wait_for_health_service_reachable_or_exit(
        &ctx,
        UVC_CAMERA_NODE_NAME,
        emitter_instance_id,
        &mut emitter_child,
        &user_node_emitter,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;

    send_shutdown(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        RECEIVER_NODE_NAME,
        Some(TEST_CORE_NODE),
        receiver_instance_id,
        Duration::from_secs(5),
    )
    .await;
    send_shutdown(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        UVC_CAMERA_NODE_NAME,
        Some(TEST_CORE_NODE),
        emitter_instance_id,
        Duration::from_secs(5),
    )
    .await;

    // Wait for both processes to exit
    let receiver_output = wait_for_child(
        &mut receiver_child,
        Some(Duration::from_secs(10)),
        &user_node_receiver,
    );
    let emitter_output = wait_for_child(
        &mut emitter_child,
        Some(Duration::from_secs(10)),
        &user_node_emitter,
    );

    let receiver_stdout = String::from_utf8_lossy(&receiver_output.stdout).into_owned();
    let receiver_stderr = String::from_utf8_lossy(&receiver_output.stderr).into_owned();
    assert!(
        receiver_output.status.success(),
        "receiver cargo run failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        receiver_output.status.code(),
        receiver_stdout,
        receiver_stderr
    );
    assert!(
        receiver_stdout.contains("got 640x480 frame encoded as rgb8"),
        "receiver did not receive emitter frame.\nstdout:\n{}\nstderr:\n{}",
        receiver_stdout,
        receiver_stderr
    );

    let emitter_stdout = String::from_utf8_lossy(&emitter_output.stdout).into_owned();
    let emitter_stderr = String::from_utf8_lossy(&emitter_output.stderr).into_owned();
    assert!(
        emitter_output.status.success(),
        "emitter cargo run failed with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        emitter_output.status.code(),
        emitter_stdout,
        emitter_stderr
    );
}
