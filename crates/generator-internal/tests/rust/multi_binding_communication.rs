//! End-to-end coverage of a multi-cardinality dependency slot: one
//! consumer whose `cameras` slot declares `cardinality: "one_or_more"`
//! and is bound to TWO running producers of the same node. Exercises the
//! whole chain with real processes over a real router:
//!
//! - the manifest's `cardinality` parses and the runtime accepts a
//!   two-producer bound set for the slot,
//! - `bound_producers()` is one slot-level set, identical across the
//!   topic and service modules, in binding declaration order,
//! - the single merged topic subscription yields frames from BOTH bound
//!   producers, each tagged with the producer that published it,
//! - directed service calls: one `poll` per bound producer, each answered
//!   by exactly the targeted instance,
//! - a producer outside the bound set is rejected before anything
//!   reaches the wire.

use crate::helpers::{
    DEFAULT_WAIT_TIMEOUT, EMITTED_TOPIC_EXAMPLE, EXPOSED_SERVICE_EXAMPLE, STUB_NODE_CONFIG,
    SUBSCRIBED_TOPIC_FORMAT_EXAMPLE, WaitContext, bind_slot_many, compile_project,
    copy_config_to_output, init_cargo_user_node, init_test_env, multi_consumer_stub_node_config,
    send_shutdown, spawn_cargo_run, test_peppy_dirs, wait_for_child,
    wait_for_health_service_reachable_or_exit,
};
use config::consts::{PEPPYGEN_OUTPUT_PATH, RUNTIME_CONFIG_VAR_NAME};
use config::runtime::NodeInstanceConfig;
use config::{
    node::{
        ConsumedService, ConsumedTopic, MessageFormat, NativeEmittedTopic, NativeExposedService,
    },
    runtime::{Name, RuntimeConfig},
};
use generator::LanguageGenerator;
use std::path::Path;
use std::{fs, time::Duration};
use tempfile::TempDir;

const TEST_CORE_NODE: &str = "test_core";
const CONSUMER_NODE_NAME: &str = "consumer_node";
const CONSUMER_INSTANCE_ID: &str = "consumer_instance";
const UVC_CAMERA_NODE_NAME: &str = "uvc_camera";
const FRONT_CAMERA_INSTANCE_ID: &str = "front_camera";
const REAR_CAMERA_INSTANCE_ID: &str = "rear_camera";
const SHUTDOWN_SENDER_INSTANCE_ID: &str = "test_shutdown_sender";

/// The consumer's `cameras` slot, consumed as both a topic and a service.
const MULTI_SUBSCRIBED_TOPIC: &str = r#"
{
  link_id: "cameras",
  name: "video_stream",
}
"#;

const MULTI_CONSUMED_SERVICE: &str = r#"
{
  link_id: "cameras",
  name: "enable_camera",
}
"#;

const CONSUMED_SERVICE_REQUEST_FORMAT: &str = r#"{ enable: "bool" }"#;

#[rstest::rstest]
#[case::peer(crate::helpers::Mode::Peer)]
#[case::router(crate::helpers::Mode::Router)]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn one_or_more_slot_fans_in_topics_and_directs_services(#[case] mode: crate::helpers::Mode) {
    let instance = pmi::ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    // --- Consumer project: one `one_or_more` slot, consumed as a topic
    // and a service.
    let temp_dir_consumer = TempDir::new_in(crate::helpers::test_tmp_root()).unwrap();
    let consumed_topic: ConsumedTopic = serde_json5::from_str(MULTI_SUBSCRIBED_TOPIC).unwrap();
    let topic_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE).unwrap();
    let consumed_service: ConsumedService = serde_json5::from_str(MULTI_CONSUMED_SERVICE).unwrap();
    let service_request_format: MessageFormat =
        serde_json5::from_str(CONSUMED_SERVICE_REQUEST_FORMAT).unwrap();
    let service_response_format: MessageFormat =
        serde_json5::from_str(crate::helpers::CONSUMED_SERVICE_RESPONSE_FORMAT_EXAMPLE).unwrap();

    let (mut generator, consumer_dir, user_node_consumer, consumer_config_path) =
        init_test_env::<generator::RustGenerator>(
            &temp_dir_consumer,
            &multi_consumer_stub_node_config(UVC_CAMERA_NODE_NAME, "v1", "cameras", "one_or_more"),
        );
    let dependency = generator::DependencyContext::native(
        UVC_CAMERA_NODE_NAME,
        "v1",
        "cameras",
        config::node::Cardinality::OneOrMore,
    );
    generator
        .add_consumed_topic(&consumed_topic, topic_format, &dependency)
        .unwrap();
    generator
        .add_consumed_service(
            &consumed_service,
            &service_request_format,
            &service_response_format,
            &dependency,
        )
        .unwrap();
    let output_config = copy_config_to_output(&user_node_consumer, &consumer_dir);
    generator
        .build(&consumer_dir, &test_peppy_dirs(), Default::default())
        .unwrap();
    fs::remove_file(output_config).unwrap();
    config::fingerprint::create_codegen_fingerprint(
        &consumer_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    // Both cameras bound to the ONE `cameras` slot, in declaration order.
    let consumer_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstanceConfig::new(Name::new(CONSUMER_INSTANCE_ID).unwrap()),
        CONSUMER_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .unwrap();
    let consumer_runtime_config = bind_slot_many(
        consumer_runtime_config,
        "cameras",
        TEST_CORE_NODE,
        &[FRONT_CAMERA_INSTANCE_ID, REAR_CAMERA_INSTANCE_ID],
    );
    let consumer_runtime_config = crate::helpers::apply_mode(consumer_runtime_config, mode);
    let consumer_runtime_config_path = temp_dir_consumer.path().join("peppy_runtime.json5");
    consumer_runtime_config
        .save_json5_launch_config(&consumer_runtime_config_path)
        .unwrap();

    init_cargo_user_node(&user_node_consumer);
    let consumer_main = r#"
use peppygen::consumed_services::cameras_enable_camera;
use peppygen::consumed_topics::cameras_video_stream;
use peppygen::{NodeBuilder, ProducerRef, Result};
use std::collections::HashSet;
use std::time::Duration;

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        // One slot-level set, identical across every module sharing the
        // slot's link_id, in binding declaration order.
        let cameras = cameras_video_stream::bound_producers(&node_runner);
        assert_eq!(cameras, cameras_enable_camera::bound_producers(&node_runner));
        let bound_ids: Vec<&str> = cameras.iter().map(|p| p.instance_id.as_str()).collect();
        println!("bound producers: {}", bound_ids.join(","));

        // The single merged subscription covers both bound cameras and
        // tags every frame with its producer.
        let mut subscription = cameras_video_stream::subscribe(&node_runner).await?;
        let mut seen: HashSet<String> = HashSet::new();
        while seen.len() < 2 {
            let Some((producer, frame)) = subscription.next().await? else {
                break;
            };
            assert_eq!(frame.width, 640, "frame payload must decode per producer");
            seen.insert(producer.instance_id.clone());
        }
        let mut seen: Vec<String> = seen.into_iter().collect();
        seen.sort();
        println!("frames from: {}", seen.join(","));

        // Directed calls: one poll per bound camera, each answered by
        // exactly the targeted instance.
        for camera in cameras_enable_camera::bound_producers(&node_runner) {
            let request = cameras_enable_camera::Request::new(true);
            let response = cameras_enable_camera::poll(
                &node_runner,
                camera,
                Duration::from_secs(5),
                request,
            )
            .await?;
            assert_eq!(
                response.instance_id, camera.instance_id,
                "the targeted camera must be the one that answers"
            );
            println!("enabled {} answered_by {}", camera.instance_id, response.instance_id);
        }

        // A plainly constructed producer outside the slot's bound set is
        // rejected before anything reaches the wire.
        let ghost = ProducerRef::new(
            node_runner.processor().bound_core_node(),
            "ghost_camera",
        );
        let request = cameras_enable_camera::Request::new(true);
        let rejected = cameras_enable_camera::poll(
            &node_runner,
            &ghost,
            Duration::from_secs(5),
            request,
        )
        .await;
        match rejected {
            Ok(_) => panic!("an out-of-set target must be rejected"),
            Err(err) => println!("ghost rejected: {err}"),
        }

        Ok(())
    })
}
"#;
    fs::write(
        user_node_consumer.join("src").join("main.rs"),
        consumer_main,
    )
    .expect("failed to write consumer main");

    // --- Camera project: emits `video_stream` and exposes
    // `enable_camera`; compiled once, spawned twice as `front_camera` and
    // `rear_camera`.
    let temp_dir_camera = TempDir::new_in(crate::helpers::test_tmp_root()).unwrap();
    let emitted_topic: NativeEmittedTopic = serde_json5::from_str(EMITTED_TOPIC_EXAMPLE).unwrap();
    let exposed_service: NativeExposedService =
        serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE).unwrap();
    let (mut generator, camera_dir, user_node_camera, camera_config_path) =
        init_test_env::<generator::RustGenerator>(&temp_dir_camera, STUB_NODE_CONFIG);
    generator.add_emitted_topic(&emitted_topic, None).unwrap();
    generator
        .add_exposed_service(&exposed_service, None)
        .unwrap();
    let output_config = copy_config_to_output(&user_node_camera, &camera_dir);
    generator
        .build(&camera_dir, &test_peppy_dirs(), Default::default())
        .unwrap();
    fs::remove_file(output_config).unwrap();
    config::fingerprint::create_codegen_fingerprint(
        &camera_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let camera_runtime_config_path = |instance_id: &str| {
        let runtime_config = RuntimeConfig::new(
            &router_host,
            router_port,
            NodeInstanceConfig::new(Name::new(instance_id).unwrap()),
            UVC_CAMERA_NODE_NAME,
            "v1",
            TEST_CORE_NODE,
        )
        .unwrap();
        let runtime_config = crate::helpers::apply_mode(runtime_config, mode);
        let path = temp_dir_camera
            .path()
            .join(format!("peppy_runtime_{instance_id}.json5"));
        runtime_config.save_json5_launch_config(&path).unwrap();
        path
    };
    let front_config_path = camera_runtime_config_path(FRONT_CAMERA_INSTANCE_ID);
    let rear_config_path = camera_runtime_config_path(REAR_CAMERA_INSTANCE_ID);

    init_cargo_user_node(&user_node_camera);
    let camera_main = r#"
use peppygen::emitted_topics::video_stream;
use peppygen::exposed_services::enable_camera;
use peppygen::NodeBuilder;
use peppygen::Result;
use std::time::Duration;

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        let publisher_runner = node_runner.clone();
        tokio::spawn(async move {
            let publisher = video_stream::declare_publisher(&publisher_runner)
                .await
                .expect("declare video_stream publisher");
            let mut frame_id = 0u32;
            loop {
                let payload = video_stream::build_message(
                    video_stream::MessageHeader {
                        stamp: std::time::SystemTime::now(),
                        frame_id,
                    },
                    "rgb8".to_owned(),
                    640,
                    480,
                    vec![1, 2, 3],
                )
                .expect("build video_stream message");
                publisher
                    .publish(payload)
                    .await
                    .expect("publish video_stream message");
                frame_id = frame_id.wrapping_add(1);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });

        let service_runner = node_runner.clone();
        tokio::spawn(async move {
            loop {
                enable_camera::handle_next_request(
                    &service_runner,
                    |request| -> Result<enable_camera::Response> {
                        Ok(enable_camera::Response::new(
                            request.data.enable,
                            Some("handled".to_owned()),
                        ))
                    },
                )
                .await
                .expect("handle enable_camera request");
            }
        });

        Ok(())
    })
}
"#;
    fs::write(user_node_camera.join("src").join("main.rs"), camera_main)
        .expect("failed to write camera main");

    compile_project(&user_node_consumer);
    compile_project(&user_node_camera);

    let consumer_config_str = consumer_runtime_config_path.to_str().unwrap().to_owned();
    let front_config_str = front_config_path.to_str().unwrap().to_owned();
    let rear_config_str = rear_config_path.to_str().unwrap().to_owned();

    let mut front_child = spawn_cargo_run(
        &user_node_camera,
        &[(RUNTIME_CONFIG_VAR_NAME, &front_config_str)],
    );
    let mut rear_child = spawn_cargo_run(
        &user_node_camera,
        &[(RUNTIME_CONFIG_VAR_NAME, &rear_config_str)],
    );
    let mut consumer_child = spawn_cargo_run(
        &user_node_consumer,
        &[(RUNTIME_CONFIG_VAR_NAME, &consumer_config_str)],
    );

    let messenger = peppylib::MessengerHandle::connect(&router_host, router_port)
        .await
        .expect("failed to create messenger for shutdown");
    let ctx = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_core_node: TEST_CORE_NODE,
    };
    for (instance_id, child, dir) in [
        (
            FRONT_CAMERA_INSTANCE_ID,
            &mut front_child,
            &user_node_camera,
        ),
        (REAR_CAMERA_INSTANCE_ID, &mut rear_child, &user_node_camera),
    ] {
        wait_for_health_service_reachable_or_exit(
            &ctx,
            UVC_CAMERA_NODE_NAME,
            instance_id,
            child,
            dir,
            DEFAULT_WAIT_TIMEOUT,
        )
        .await;
    }

    // The consumer's health service becomes reachable only after its
    // setup_fn (all the in-process assertions) has completed; the node then
    // stays alive until shutdown, so stop it and read its stdout.
    wait_for_health_service_reachable_or_exit(
        &ctx,
        CONSUMER_NODE_NAME,
        CONSUMER_INSTANCE_ID,
        &mut consumer_child,
        &user_node_consumer,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;
    send_shutdown(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        CONSUMER_NODE_NAME,
        TEST_CORE_NODE,
        CONSUMER_INSTANCE_ID,
        Duration::from_secs(5),
    )
    .await;
    let consumer_output = wait_for_child(
        &mut consumer_child,
        Some(Duration::from_secs(10)),
        &user_node_consumer,
    );

    for instance_id in [FRONT_CAMERA_INSTANCE_ID, REAR_CAMERA_INSTANCE_ID] {
        send_shutdown(
            &messenger,
            TEST_CORE_NODE,
            SHUTDOWN_SENDER_INSTANCE_ID,
            UVC_CAMERA_NODE_NAME,
            TEST_CORE_NODE,
            instance_id,
            Duration::from_secs(5),
        )
        .await;
    }
    wait_for_child(
        &mut front_child,
        Some(Duration::from_secs(10)),
        &user_node_camera,
    );
    wait_for_child(
        &mut rear_child,
        Some(Duration::from_secs(10)),
        &user_node_camera,
    );

    let stdout = String::from_utf8_lossy(&consumer_output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&consumer_output.stderr).into_owned();
    assert!(
        consumer_output.status.success(),
        "consumer cargo run failed with status: {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        consumer_output.status.code(),
    );

    // Binding declaration order, verbatim.
    assert!(
        stdout.contains("bound producers: front_camera,rear_camera"),
        "bound_producers() must preserve binding declaration order.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    // Fan-in: the one merged subscription delivered from BOTH producers.
    assert!(
        stdout.contains("frames from: front_camera,rear_camera"),
        "the merged subscription must yield frames from both bound cameras.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    // Directed routing: each poll answered by exactly its target.
    assert!(
        stdout.contains("enabled front_camera answered_by front_camera")
            && stdout.contains("enabled rear_camera answered_by rear_camera"),
        "each directed poll must be answered by the targeted camera.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    // Membership: an out-of-set target fails before the wire, naming the
    // slot and the rejected producer.
    assert!(
        stdout.contains("ghost rejected:")
            && stdout.contains("ghost_camera")
            && stdout.contains("`cameras`"),
        "an out-of-set target must be rejected with slot and producer context.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
