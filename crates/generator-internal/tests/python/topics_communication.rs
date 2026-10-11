use crate::helpers::{
    DEFAULT_WAIT_TIMEOUT, EMITTED_TOPIC_EXAMPLE, STUB_PYTHON_NODE_CONFIG,
    SUBSCRIBED_TOPIC_FORMAT_EXAMPLE, TEST_CORE_NODE, TopicScenario, assert_exited_successfully,
    bind_slot, copy_config_to_output, generate_node_project, init_python_project_venv,
    init_python_user_node, init_test_env, native_dep, python_consumer_stub_node_config,
    save_runtime_config, shut_down_test_node, spawn_python_run, test_node_runtime_config,
    test_peppy_dirs, test_wait_context, wait_for_child, wait_for_health_service_reachable_or_exit,
    wait_for_service_reachable_or_exit,
};
use config::consts::{PEPPYGEN_OUTPUT_PATH, RUNTIME_CONFIG_VAR_NAME};
use config::runtime::NodeInstanceConfig;
use config::{
    node::{ConsumedTopic, MessageFormat, NativeEmittedTopic, NativeExposedService},
    runtime::{Name, RuntimeConfig},
};
use generator::LanguageGenerator;
use std::path::Path;
use std::{fs, time::Duration};
use tempfile::TempDir;

// --- Common test constants
const RECEIVER_NODE_NAME: &str = "receiver_node";
const RECEIVER_INSTANCE_ID: &str = "receiver_instance";
const EMITTER_INSTANCE_ID: &str = "emitter_instance";
const UVC_CAMERA_NODE_NAME: &str = "uvc_camera";
const FRAME_RECEIVED_SERVICE: &str = "frame_received_ack";
const EXPOSED_FRAME_RECEIVED_SERVICE_EXAMPLE: &str = r#"
{
  name: "frame_received_ack"
}
"#;

// --- Topics emitted and its corresponding receiver

const SUBSCRIBED_TOPIC_EXAMPLE: &str = r#"
{
  link_id: "uvc_camera",
  name: "video_stream",
}
"#;

/// The emitter of the retaining scenario: it publishes one frame during setup
/// and keeps no publisher.
const RETAINING_EMITTER_MAIN: &str = r#"
import asyncio
import time
from peppygen import NodeBuilder
from peppygen.emitted_topics import video_stream

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    publisher = await video_stream.declare_publisher(node_runner)
    await publisher.publish(
        video_stream.build_message(
            video_stream.MessageHeader(stamp=time.time(), frame_id=0),
            "rgb8",
            640,
            480,
            bytes([1, 2, 3]),
            ["left", "right"],
        )
    )
    print("emitter: published the retained frame", flush=True)
    return []

def main():
    NodeBuilder().run(setup)

if __name__ == "__main__":
    main()
"#;

/// Creates 2 Python projects in separate directories and checks if they can send/receive topics.
#[rstest::rstest]
#[case::peer(crate::helpers::LocalNodesTopology::Peer)]
#[case::router(crate::helpers::LocalNodesTopology::Router)]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn topics_communication(#[case] topology: crate::helpers::LocalNodesTopology) {
    run_emitter_and_receiver(topology, TopicScenario::LiveStream).await;
}

/// A receiver that starts after the emitter published its one frame on a
/// retaining topic reads that frame from its first `next()`.
#[rstest::rstest]
#[case::peer(crate::helpers::LocalNodesTopology::Peer)]
#[case::router(crate::helpers::LocalNodesTopology::Router)]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn a_receiver_started_late_reads_the_retained_frame(
    #[case] topology: crate::helpers::LocalNodesTopology,
) {
    run_emitter_and_receiver(topology, TopicScenario::RetainingThenLateJoin).await;
}

async fn run_emitter_and_receiver(
    topology: crate::helpers::LocalNodesTopology,
    scenario: TopicScenario,
) {
    let instance = pmi::ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    // --- Receiver project
    let receiver_instance_id = RECEIVER_INSTANCE_ID;
    let temp_dir_receiver = TempDir::new_in(config_test_support::test_tmp_root()).unwrap();
    let consumed_topic: ConsumedTopic = serde_json5::from_str(SUBSCRIBED_TOPIC_EXAMPLE).unwrap();
    let subscribed_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE).unwrap();
    let frame_received_service: NativeExposedService =
        serde_json5::from_str(EXPOSED_FRAME_RECEIVED_SERVICE_EXAMPLE).unwrap();
    let user_node_receiver = generate_node_project::<generator::PythonGenerator>(
        &temp_dir_receiver,
        &python_consumer_stub_node_config("uvc_camera", "v1", "uvc_camera"),
        |generator| {
            generator
                .add_consumed_topic(
                    &consumed_topic,
                    subscribed_format,
                    scenario.retention(),
                    &native_dep("uvc_camera", "v1", "uvc_camera"),
                )
                .unwrap();
            generator
                .add_exposed_service(&frame_received_service, None)
                .unwrap();
        },
    );

    let receiver_runtime_config = test_node_runtime_config(
        &router_host,
        router_port,
        RECEIVER_NODE_NAME,
        receiver_instance_id,
    );
    let receiver_runtime_config = bind_slot(
        receiver_runtime_config,
        "uvc_camera",
        TEST_CORE_NODE,
        EMITTER_INSTANCE_ID,
    );
    let receiver_runtime_config = crate::helpers::apply_topology(receiver_runtime_config, topology);
    let user_node_receiver_config_str =
        save_runtime_config(&receiver_runtime_config, temp_dir_receiver.path());

    init_python_user_node(&user_node_receiver);
    let receiver_main = r#"
import asyncio
import os
import sys
from peppygen import NodeBuilder
from peppygen.exposed_services import frame_received_ack
from peppygen.consumed_topics.uvc_camera import video_stream as uvc_camera_video_stream

async def receive_frames(node_runner, frame_received):
    # `next()` returns None once the subscription has closed; unpacking that
    # directly would raise an opaque TypeError that asyncio swallows as an
    # unhandled task exception, so the ack task would hang on the event until
    # the test's wait timeout. Fail fast to stderr + os._exit(1) instead, so the
    # harness sees the exit and panics with the captured stderr (matches the
    # wire_compat.rs receiver).
    try:
        print("receiver: about to subscribe", flush=True)
        subscription = await uvc_camera_video_stream.subscribe(node_runner)
        received = await subscription.next()
        if received is None:
            print("receive failed: subscription closed before any frame", file=sys.stderr, flush=True)
            os._exit(1)
        producer, frame = received
        print(
            f"got {frame.width}x{frame.height} frame encoded as {frame.encoding} from {producer.core_node}/{producer.instance_id}",
            flush=True,
        )
        frame_received.set()
    except BaseException as e:
        print(f"receive failed: {type(e).__name__}: {e}", file=sys.stderr, flush=True)
        os._exit(1)

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
    let temp_dir_emitter = TempDir::new_in(config_test_support::test_tmp_root()).unwrap();
    let emitted_topic = NativeEmittedTopic {
        retention: scenario.retention(),
        ..serde_json5::from_str(EMITTED_TOPIC_EXAMPLE).unwrap()
    };
    let (mut generator, emitter_dir, user_node_emitter, peppy_node_config_path) =
        init_test_env::<generator::PythonGenerator>(&temp_dir_emitter, STUB_PYTHON_NODE_CONFIG);
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
    let emitter_runtime_config = crate::helpers::apply_topology(emitter_runtime_config, topology);
    let user_node_emitter_runtime_config_str =
        save_runtime_config(&emitter_runtime_config, temp_dir_emitter.path());

    init_python_user_node(&user_node_emitter);
    let live_emitter_main = r#"
import asyncio
import time
from peppygen import NodeBuilder
from peppygen.emitted_topics import video_stream

async def setup(parameters, node_runner) -> list[asyncio.Task]:
    frequency_hz = parameters.frequency
    interval = 1.0 / frequency_hz

    async def emit_loop():
        publisher = await video_stream.declare_publisher(node_runner)
        frame_id = 0
        print(f"emitter: starting emit loop with interval={interval}", flush=True)
        while True:
            await publisher.publish(
                video_stream.build_message(
                    video_stream.MessageHeader(stamp=time.time(), frame_id=frame_id),
                    "rgb8",
                    640,
                    480,
                    bytes([1, 2, 3]),
                    ["left", "right"],
                )
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

    let emitter_main = match scenario {
        TopicScenario::LiveStream => live_emitter_main,
        TopicScenario::RetainingThenLateJoin => RETAINING_EMITTER_MAIN,
    };
    let main_file = user_node_emitter.join("main.py");
    fs::write(main_file, emitter_main).expect("failed to write main.py");

    println!(
        "User node receiver PEPPY_RUNTIME_CONFIG=\"{}\"",
        user_node_receiver_config_str
    );
    println!("User node receiver = {}", user_node_receiver.display());
    println!(
        "User node emitter PEPPY_RUNTIME_CONFIG=\"{}\"",
        user_node_emitter_runtime_config_str
    );
    println!("User node emitter = {}", user_node_emitter.display());

    init_python_project_venv(&user_node_receiver);
    init_python_project_venv(&user_node_emitter);

    let spawn_receiver = || {
        spawn_python_run(
            &user_node_receiver,
            &[(RUNTIME_CONFIG_VAR_NAME, &user_node_receiver_config_str)],
        )
    };
    let spawn_emitter = || {
        spawn_python_run(
            &user_node_emitter,
            &[(
                RUNTIME_CONFIG_VAR_NAME,
                &user_node_emitter_runtime_config_str,
            )],
        )
    };
    let messenger = peppylib::MessengerHandle::connect(&router_host, router_port)
        .await
        .expect("failed to create messenger for shutdown");
    let ctx = test_wait_context(&messenger);

    // A node's health service is reachable once its setup returned: for the
    // retaining emitter, once it published its one frame. The late-join
    // receiver starts after that.
    let early_receiver = match scenario {
        TopicScenario::LiveStream => Some(spawn_receiver()),
        TopicScenario::RetainingThenLateJoin => None,
    };
    let mut emitter_child = spawn_emitter();
    wait_for_health_service_reachable_or_exit(
        &ctx,
        UVC_CAMERA_NODE_NAME,
        emitter_instance_id,
        &mut emitter_child,
        &user_node_emitter,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;
    let mut receiver_child = match early_receiver {
        Some(child) => child,
        None => spawn_receiver(),
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

    // Wait until the receiver confirms receiving a frame.
    wait_for_service_reachable_or_exit(
        &ctx,
        RECEIVER_NODE_NAME,
        FRAME_RECEIVED_SERVICE,
        Some(receiver_instance_id),
        &mut receiver_child,
        &user_node_receiver,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;
    shut_down_test_node(&messenger, RECEIVER_NODE_NAME, receiver_instance_id).await;
    shut_down_test_node(&messenger, UVC_CAMERA_NODE_NAME, emitter_instance_id).await;
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

    let (receiver_stdout, receiver_stderr) =
        assert_exited_successfully("receiver process", &receiver_output);
    assert!(
        receiver_stdout.contains(&format!(
            "got 640x480 frame encoded as rgb8 from {TEST_CORE_NODE}/{EMITTER_INSTANCE_ID}"
        )),
        "receiver did not receive emitter frame with full producer identity.\nstdout:\n{}\nstderr:\n{}",
        receiver_stdout,
        receiver_stderr
    );

    assert_exited_successfully("emitter process", &emitter_output);
}
