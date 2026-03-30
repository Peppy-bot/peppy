use crate::helpers::{
    STUB_PYTHON_NODE_CONFIG, WaitContext, copy_config_to_output, init_python_project_venv,
    init_python_user_node, init_test_env, send_shutdown, spawn_python_run, test_peppy_dirs,
    wait_for_child, wait_for_health_service_reachable_or_exit, wait_for_service_reachable_or_exit,
};
use config::consts::{PEPPYGEN_OUTPUT_PATH, RUNTIME_CONFIG_VAR_NAME};
use config::runtime::NodeInstance;
use config::{
    node::{ConsumedTopic, EmittedTopic, ExposedService, MessageFormat},
    peppy_config::Name,
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
const FRAME_RECEIVED_SERVICE: &str = "frame_received_ack";
const EXPOSED_FRAME_RECEIVED_SERVICE_EXAMPLE: &str = r#"
{
  name: "frame_received_ack"
}
"#;

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
  local_node_id: "uvc_camera",
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

/// Creates 2 Python projects in separate directories and checks if they can send/receive topics.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn topics_communication() {
    let instance = pmi::ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    // --- Receiver project
    let receiver_instance_id = RECEIVER_INSTANCE_ID;
    let temp_dir_proj2 = TempDir::new().unwrap();
    let consumed_topic: ConsumedTopic = serde_json5::from_str(SUBSCRIBED_TOPIC_EXAMPLE).unwrap();
    let subscribed_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE).unwrap();
    let frame_received_service: ExposedService =
        serde_json5::from_str(EXPOSED_FRAME_RECEIVED_SERVICE_EXAMPLE).unwrap();
    let (mut generator, receiver_dir, user_node_receiver, peppy_node_config_path) =
        init_test_env::<generator::PythonGenerator>(&temp_dir_proj2, STUB_PYTHON_NODE_CONFIG);
    generator
        .add_consumed_topic(&consumed_topic, subscribed_format, "uvc_camera")
        .unwrap();
    generator
        .add_exposed_service(&frame_received_service)
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
        NodeInstance {
            instance_id: Name::new(receiver_instance_id).unwrap(),
            arguments: Default::default(),
        },
        RECEIVER_NODE_NAME,
        TEST_CORE_NODE,
    )
    .unwrap();
    let receiver_runtime_config_path = temp_dir_proj2.path().join("peppy_runtime.json5");
    receiver_runtime_config
        .save_json5_launch_config(&receiver_runtime_config_path)
        .unwrap();

    init_python_user_node(&user_node_receiver);
    let receiver_main = r#"
import asyncio
from peppygen import NodeBuilder
from peppygen.exposed_services import frame_received_ack
from peppygen.consumed_topics import uvc_camera_video_stream

async def receive_frames(node_runner, frame_received):
    print("receiver: about to subscribe", flush=True)
    instance_id, frame = await uvc_camera_video_stream.on_next_message_received(node_runner)
    print(
        f"got {frame.width}x{frame.height} frame encoded as {frame.encoding} from {instance_id}",
        flush=True,
    )
    frame_received.set()

async def expose_frame_received_ack(node_runner, frame_received):
    await frame_received.wait()
    await frame_received_ack.handle_next_request(node_runner, lambda _request: None)

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    frame_received = asyncio.Event()
    return [
        asyncio.create_task(receive_frames(node_runner, frame_received)),
        asyncio.create_task(expose_frame_received_ack(node_runner, frame_received)),
    ]

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;
    let main_file = user_node_receiver.join("main.py");
    fs::write(main_file, receiver_main).expect("failed to write main.py");

    // --- Emitter project
    let emitter_instance_id = EMITTER_INSTANCE_ID;
    let temp_dir_proj1 = TempDir::new().unwrap();
    let emitted_topic: EmittedTopic = serde_json5::from_str(EMITTED_TOPIC_EXAMPLE).unwrap();
    let (mut generator, emitter_dir, user_node_emitter, peppy_node_config_path) =
        init_test_env::<generator::PythonGenerator>(&temp_dir_proj1, STUB_PYTHON_NODE_CONFIG);
    let emitter_parameters: config::RawNodeArguments =
        serde_json5::from_str(r#"{ frequency: "f64" }"#).unwrap();
    generator.set_parameters(emitter_parameters.clone());
    generator.add_emitted_topic(&emitted_topic).unwrap();
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
        NodeInstance {
            instance_id: Name::new(emitter_instance_id).unwrap(),
            arguments: serde_json5::from_str(r#"{ frequency: 10.0 }"#).unwrap(),
        },
        UVC_CAMERA_NODE_NAME, // Must match the node name expected by the receiver
        TEST_CORE_NODE,
    )
    .unwrap();
    let emitter_runtime_config_path = temp_dir_proj1.path().join("peppy_runtime.json5");
    emitter_runtime_config
        .save_json5_launch_config(&emitter_runtime_config_path)
        .unwrap();

    init_python_user_node(&user_node_emitter);
    let emitter_main = r#"
import asyncio
import time
from peppygen import NodeBuilder
from peppygen.emitted_topics import video_stream

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    frequency_hz = parameters.frequency
    interval = 1.0 / frequency_hz

    async def emit_loop():
        frame_id = 0
        print(f"emitter: starting emit loop with interval={interval}", flush=True)
        while True:
            await video_stream.emit(
                node_runner,
                video_stream.MessageHeader(stamp=time.time(), frame_id=frame_id),
                "rgb8",
                640,
                480,
                bytes([1, 2, 3]),
            )
            if frame_id == 0:
                print("emitter: first emit succeeded", flush=True)
            frame_id = (frame_id + 1) % (2**32)
            await asyncio.sleep(interval)

    print("emitter: background task started", flush=True)
    return [asyncio.create_task(emit_loop())]

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;

    let main_file = user_node_emitter.join("main.py");
    fs::write(main_file, emitter_main).expect("failed to write main.py");

    let user_node_receiver_config_str = receiver_runtime_config_path.to_str().unwrap().to_owned();
    let user_node_emitter_runtime_config_str =
        emitter_runtime_config_path.to_str().unwrap().to_owned();

    println!(
        "User node receiver PEPPY_RUNTIME_CONFIG=\"{}\"",
        &receiver_runtime_config_path.display()
    );
    println!("User node receiver = {}", user_node_receiver.display());
    println!(
        "User node emitter PEPPY_RUNTIME_CONFIG=\"{}\"",
        &emitter_runtime_config_path.display()
    );
    println!("User node emitter = {}", user_node_emitter.display());

    init_python_project_venv(&user_node_receiver);
    init_python_project_venv(&user_node_emitter);

    // Spawn both processes
    let mut receiver_child = spawn_python_run(
        &user_node_receiver,
        &[(RUNTIME_CONFIG_VAR_NAME, &user_node_receiver_config_str)],
    );
    let mut emitter_child = spawn_python_run(
        &user_node_emitter,
        &[(
            RUNTIME_CONFIG_VAR_NAME,
            &user_node_emitter_runtime_config_str,
        )],
    );

    let messenger = peppylib::MessengerHandle::from_host_port(&router_host, router_port)
        .await
        .expect("failed to create messenger for shutdown");
    let ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_core_node: Some(TEST_CORE_NODE),
    };

    // Wait for both nodes to expose health/ready endpoints.
    wait_for_health_service_reachable_or_exit(
        &ctx,
        RECEIVER_NODE_NAME,
        receiver_instance_id,
        &mut receiver_child,
        &user_node_receiver,
    )
    .await;
    wait_for_health_service_reachable_or_exit(
        &ctx,
        UVC_CAMERA_NODE_NAME,
        emitter_instance_id,
        &mut emitter_child,
        &user_node_emitter,
    )
    .await;

    // Wait until the receiver confirms receiving a frame.
    wait_for_service_reachable_or_exit(
        &ctx,
        RECEIVER_NODE_NAME,
        FRAME_RECEIVED_SERVICE,
        Some(receiver_instance_id),
        &mut receiver_child,
        &user_node_receiver,
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
