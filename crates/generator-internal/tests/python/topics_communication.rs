use crate::helpers::{
    STUB_PYTHON_NODE_CONFIG, WaitContext, compile_python_project, copy_config_to_output,
    init_python_user_node, init_test_env, send_shutdown, spawn_python_run, wait_for_child,
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
const TEST_MASTER_NODE: &str = "test_master";
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

/// Creates 2 Python projects in separate directories and checks if they can send/receive topics.
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
        init_test_env::<generator::PythonGenerator>(&temp_dir_proj2, STUB_PYTHON_NODE_CONFIG);
    generator
        .add_subscribed_topic(&subscribed_topic, subscribed_format)
        .unwrap();
    let output_config = copy_config_to_output(&user_node_subscriber, &subscriber_dir);
    generator.build(&subscriber_dir).unwrap();
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
        TEST_MASTER_NODE,
    )
    .unwrap();
    let subscriber_runtime_config_path = temp_dir_proj2.path().join("peppy_runtime.json5");
    subscriber_runtime_config
        .save_json5_launch_config(&subscriber_runtime_config_path)
        .unwrap();

    init_python_user_node(&user_node_subscriber);
    let subscriber_main = r#"
import asyncio
import sys
import traceback
from peppygen import NodeBuilder
from peppygen.subscribed_topics import uvc_camera_video_stream

def setup(parameters, node_runner):
    async def async_main():
        try:
            print("subscriber: about to subscribe", flush=True)
            instance_id, frame = await uvc_camera_video_stream.on_next_message_received(node_runner)
            print(
                f"got {frame.width}x{frame.height} frame encoded as {frame.encoding} from {instance_id}",
                flush=True,
            )
        except Exception as e:
            print(f"subscriber error: {e}", flush=True)
            traceback.print_exc()
            raise
    try:
        asyncio.run(async_main())
    except Exception as e:
        print(f"subscriber asyncio.run error: {e}", flush=True)
        traceback.print_exc()
        raise

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;
    let main_file = user_node_subscriber.join("main.py");
    fs::write(main_file, subscriber_main).expect("failed to write main.py");

    // --- Exposer project
    let exposer_instance_id = EXPOSER_INSTANCE_ID;
    let temp_dir_proj1 = TempDir::new().unwrap();
    let exposed_topic: ExposedTopic = serde_json5::from_str(EXPOSED_TOPIC_EXAMPLE).unwrap();
    let (mut generator, exposer_dir, user_node_exposer, peppy_node_config_path) =
        init_test_env::<generator::PythonGenerator>(&temp_dir_proj1, STUB_PYTHON_NODE_CONFIG);
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
        TEST_MASTER_NODE,
    )
    .unwrap();
    let exposer_runtime_config_path = temp_dir_proj1.path().join("peppy_runtime.json5");
    exposer_runtime_config
        .save_json5_launch_config(&exposer_runtime_config_path)
        .unwrap();

    init_python_user_node(&user_node_exposer);
    let exposer_main = r#"
import asyncio
import time
import sys
import traceback
import threading
from peppygen import NodeBuilder
from peppygen.exposed_topics import video_stream

def setup(parameters, node_runner):
    frequency_hz = parameters["frequency"]
    interval = 1.0 / frequency_hz

    def emit_loop():
        async def run():
            frame_id = 0
            print(f"exposer: starting emit loop with interval={interval}", flush=True)
            while True:
                try:
                    await video_stream.emit(
                        node_runner,
                        video_stream.MessageHeader(stamp=time.time(), frame_id=frame_id),
                        "rgb8",
                        640,
                        480,
                        bytes([1, 2, 3]),
                    )
                    if frame_id == 0:
                        print("exposer: first emit succeeded", flush=True)
                except Exception as e:
                    print(f"exposer emit error: {e}", flush=True)
                    traceback.print_exc()
                    raise
                frame_id = (frame_id + 1) % (2**32)
                await asyncio.sleep(interval)
        try:
            asyncio.run(run())
        except Exception as e:
            print(f"exposer asyncio.run error: {e}", file=sys.stderr, flush=True)
            traceback.print_exc(file=sys.stderr)

    threading.Thread(target=emit_loop, daemon=True).start()
    print("exposer: daemon thread started", flush=True)

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;

    let main_file = user_node_exposer.join("main.py");
    fs::write(main_file, exposer_main).expect("failed to write main.py");

    let user_node_subscriber_config_str =
        subscriber_runtime_config_path.to_str().unwrap().to_owned();
    let user_node_exposer_runtime_config_str =
        exposer_runtime_config_path.to_str().unwrap().to_owned();

    println!("User node subscriber = {}", user_node_subscriber.display());
    println!("User node exposer = {}", user_node_exposer.display());
    compile_python_project(&user_node_subscriber);
    compile_python_project(&user_node_exposer);

    // Spawn both processes
    let mut subscriber_child = spawn_python_run(
        &user_node_subscriber,
        &[(RUNTIME_CONFIG_VAR_NAME, &user_node_subscriber_config_str)],
    );
    let mut exposer_child = spawn_python_run(
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

    // First wait for the exposer health to ensure it's running
    wait_for_health_service_reachable_or_exit(
        &ctx,
        UVC_CAMERA_NODE_NAME,
        exposer_instance_id,
        &mut exposer_child,
        &user_node_exposer,
        Duration::from_secs(15),
    )
    .await;

    // Now wait for subscriber (which blocks until it receives a message)
    let subscriber_timeout = Duration::from_secs(20);
    let start = std::time::Instant::now();
    loop {
        if let Some(status) = subscriber_child
            .try_wait()
            .expect("failed to poll subscriber")
        {
            let output = wait_for_child(&mut subscriber_child, None, &user_node_subscriber);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            panic!(
                "subscriber exited before health became reachable (status: {:?})\nstdout:\n{}\nstderr:\n{}",
                status.code(),
                stdout,
                stderr
            );
        }

        let reachable = tokio::time::timeout(
            Duration::from_secs(2),
            peppylib::ServiceMessenger::is_reachable(
                &messenger,
                TEST_MASTER_NODE,
                SHUTDOWN_SENDER_INSTANCE_ID,
                SUBSCRIBER_NODE_NAME,
                peppylib::messaging::NODE_HEALTH_SERVICE,
                Some(TEST_MASTER_NODE),
                Some(subscriber_instance_id),
            ),
        )
        .await
        .unwrap_or(Ok(false))
        .unwrap_or(false);

        if reachable {
            break;
        }

        if start.elapsed() > subscriber_timeout {
            // Kill both processes and dump output for debugging
            let _ = subscriber_child.kill();
            let _ = exposer_child.kill();
            let sub_out = wait_for_child(&mut subscriber_child, None, &user_node_subscriber);
            let exp_out = wait_for_child(&mut exposer_child, None, &user_node_exposer);
            panic!(
                "subscriber timed out after {:?}\n\n--- SUBSCRIBER stdout ---\n{}\n--- SUBSCRIBER stderr ---\n{}\n\n--- EXPOSER stdout ---\n{}\n--- EXPOSER stderr ---\n{}",
                subscriber_timeout,
                String::from_utf8_lossy(&sub_out.stdout),
                String::from_utf8_lossy(&sub_out.stderr),
                String::from_utf8_lossy(&exp_out.stdout),
                String::from_utf8_lossy(&exp_out.stderr),
            );
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }

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
}
