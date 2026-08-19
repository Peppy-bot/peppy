//! End-to-end proof of the generated test surfaces (Rust): generate a
//! consumer node with topic + service + action + pairing links, write a real
//! node (`lib.rs` setup) and a real test using `fixtures::harness::Harness` +
//! the per-link mocks, then run `cargo test` in the node: the full loop over
//! real zenoh: mock publish → node consumes → node polls the mock service →
//! node drives the mock action (accept → feedback → complete) → node's own
//! emissions observed via fixtures (emitted topic + pairing slot), plus the
//! deterministic producer-loss path (`Mock::stop` mid-goal → the node sees
//! `ActionFeedbackProducerGone`, never a hang or clean close). A second test
//! boots the same node in sim time (`Config::use_sim_time`) and drives the
//! harness clock: every `harness.clock.tick(..)` instant is what the node's
//! `peppygen::clock::now_ns` observes.

use config::consts::PEPPYGEN_OUTPUT_PATH;
use config::node::{
    Cardinality, ConsumedAction, ConsumedService, ConsumedTopic, MessageFormat, NativeEmittedTopic,
    NativeExposedService,
};
use generator::{ConsumedActionMessage, DependencyContext, LanguageGenerator, PeerContext};
use std::fs;

use crate::helpers::{
    copy_config_to_output, init_test_env, peppygen_dev_dependency_line, run_cargo_test,
    test_peppy_dirs, test_wrapper_crate_name,
};

// The manifest must declare the slots the generated code addresses: the
// standalone processor sizes its bound/peer sets from `depends_on`, and the
// harness seeding calls are warn-skipped for undeclared slots (the runtime
// then refuses the mismatched generated code as version skew).
const NODE_CONFIG: &str = r#"{
  peppy_schema: "node/v1",
  manifest: {
    name: "generated_node",
    tag: "v1",
    depends_on: {
      nodes: [
        { name: "uvc_camera", tag: "v1", link_id: "camera" },
        { name: "brain", tag: "v1", link_id: "brain" }
      ],
      pairings: [
        { name: "arm_link", tag: "v1", role: "controller", link_id: "arm" }
      ]
    }
  },
  execution: {
    language: "rust",
    run_cmd: ["./target/release/generated_node"]
  }
}
"#;

const EMITTED_STATUS: &str = r#"{
  name: "status",
  qos_profile: "reliable",
  message_format: { outcome: "string" }
}"#;

const EXPOSED_PING: &str = r#"{
  name: "ping",
  request_message_format: { value: "u32" },
  response_message_format: { doubled: "u32" }
}"#;

const CONSUMED_FRAME_TOPIC: &str = r#"{ link_id: "camera", name: "video_stream" }"#;
const CONSUMED_FRAME_FORMAT: &str = r#"{ width: "u32" }"#;

const CONSUMED_ENABLE_SERVICE: &str = r#"{ link_id: "camera", name: "enable_camera" }"#;
const ENABLE_REQUEST_FORMAT: &str = r#"{ enable: "bool" }"#;
const ENABLE_RESPONSE_FORMAT: &str = r#"{ enabled: "bool" }"#;

const CONSUMED_PLAN_ACTION: &str = r#"{ link_id: "brain", name: "plan_motion" }"#;
const PLAN_GOAL_FORMAT: &str = r#"{ arm_id: "u16" }"#;
const PLAN_GOAL_RESPONSE_FORMAT: &str = r#"{ accepted: "bool" }"#;
const PLAN_FEEDBACK_FORMAT: &str = r#"{ progress: "f64" }"#;
const PLAN_RESULT_FORMAT: &str = r#"{ success: "bool" }"#;

const PEER_COMMANDS: &str = r#"{
  name: "joint_commands",
  qos_profile: "reliable",
  message_format: { max_velocity: "f64" }
}"#;

const PEER_STATES: &str = r#"{
  name: "joint_states",
  qos_profile: "sensor_data",
  message_format: { positions: { $type: "array", $items: "f64", $length: 3 } }
}"#;

/// The node under test: consumes a frame topic, and per frame polls the dep
/// service, drives the dep action to completion (or reports its loss), then
/// reports on its own emitted topic and pairing slot. Every branch a real
/// node-author test would need is exercised through the generated surfaces.
const NODE_LIB: &str = r#"
use std::sync::Arc;
use std::time::Duration;

pub async fn setup(
    _parameters: peppygen::Parameters,
    node_runner: Arc<peppygen::NodeRunner>,
) -> peppygen::Result<()> {
    // Wall mode under the harness: init is the no-op wrapper and now_ns
    // reads the OS clock immediately.
    peppygen::clock::init(&node_runner).await?;
    assert!(peppygen::clock::now_ns()? > 0);
    let status =
        peppygen::emitted_topics::status::declare_publisher(&node_runner).await?;
    let commands =
        peppygen::paired_topics::arm::joint_commands::declare_publisher(&node_runner).await?;
    // The node's own exposed service, answered concurrently with the frame loop.
    let ping_runner = Arc::clone(&node_runner);
    tokio::spawn(async move {
        let _ = peppygen::exposed_services::ping::handle_next_request(
            &ping_runner,
            |request| Ok(peppygen::exposed_services::ping::Response::new(request.data.value * 2)),
        )
        .await;
    });
    let mut frames =
        peppygen::consumed_topics::camera::video_stream::subscribe(&node_runner).await?;
    while let Some((_producer, frame)) = frames.next().await? {
        let camera =
            peppygen::consumed_services::camera::enable_camera::bound_producer(&node_runner);
        let response = peppygen::consumed_services::camera::enable_camera::poll(
            &node_runner,
            camera,
            Duration::from_secs(10),
            peppygen::consumed_services::camera::enable_camera::Request::new(true),
        )
        .await?;
        assert!(response.data.enabled, "mock service should have enabled");

        let brain =
            peppygen::consumed_actions::brain::plan_motion::bound_producer(&node_runner);
        let mut goal = peppygen::consumed_actions::brain::plan_motion::ActionHandle::fire_goal(
            &node_runner,
            brain,
            Duration::from_secs(10),
            peppygen::consumed_actions::brain::plan_motion::GoalRequest {
                arm_id: frame.width as u16,
            },
            peppygen::QoSProfile::Reliable,
        )
        .await?;
        assert!(goal.accepted, "mock should accept the goal");
        let outcome = match goal.on_next_feedback_message().await {
            Ok(feedback) => {
                let result = goal.get_result(Duration::from_secs(10)).await?;
                match result.outcome {
                    peppygen::consumed_actions::brain::plan_motion::ResultOutcome::Completed(
                        data,
                    ) => format!("done fb={} ok={}", feedback.progress, data.success),
                    other => format!("unexpected outcome: {other:?}"),
                }
            }
            Err(peppygen::Error::ActionFeedbackProducerGone { .. }) => {
                String::from("producer-gone")
            }
            Err(error) => return Err(error),
        };
        status
            .publish(peppygen::emitted_topics::status::build_message(outcome)?)
            .await?;
        commands
            .publish(peppygen::paired_topics::arm::joint_commands::build_message(0.5)?)
            .await?;
    }
    Ok(())
}

/// Sim-time entry point: reports every sim-clock value it observes on the
/// status topic, so the test can assert the exact virtual instants it drove
/// through `harness.clock.tick(..)`.
pub async fn setup_sim_clock(
    _parameters: peppygen::Parameters,
    node_runner: Arc<peppygen::NodeRunner>,
) -> peppygen::Result<()> {
    peppygen::clock::init(&node_runner).await?;
    let status =
        peppygen::emitted_topics::status::declare_publisher(&node_runner).await?;
    let mut last = 0u64;
    loop {
        // Wait on observation: before the first tick now_ns errors
        // (ClockNotReady), afterwards it returns the last driven instant.
        match peppygen::clock::now_ns() {
            Ok(ns) if ns != last => {
                last = ns;
                status
                    .publish(peppygen::emitted_topics::status::build_message(
                        format!("t={ns}"),
                    )?)
                    .await?;
            }
            _ => tokio::time::sleep(Duration::from_millis(5)).await,
        }
    }
}
"#;

#[test]
fn generated_mocks_and_fixtures_drive_a_node_end_to_end() {
    let temp_dir = tempfile::TempDir::new_in(crate::helpers::test_tmp_root()).unwrap();
    let (mut generator, output_dir, user_node, _config_path) =
        init_test_env::<generator::RustGenerator>(&temp_dir, NODE_CONFIG);

    generator.set_node_identity("generated_node", "v1");

    let status: NativeEmittedTopic = serde_json5::from_str(EMITTED_STATUS).unwrap();
    generator.add_emitted_topic(&status, None).unwrap();
    let ping: NativeExposedService = serde_json5::from_str(EXPOSED_PING).unwrap();
    generator.add_exposed_service(&ping, None).unwrap();

    let frame_topic: ConsumedTopic = serde_json5::from_str(CONSUMED_FRAME_TOPIC).unwrap();
    let frame_format: MessageFormat = serde_json5::from_str(CONSUMED_FRAME_FORMAT).unwrap();
    let camera_dep = DependencyContext::native("uvc_camera", "v1", "camera", Cardinality::One);
    generator
        .add_consumed_topic(&frame_topic, frame_format, &camera_dep)
        .unwrap();

    let enable: ConsumedService = serde_json5::from_str(CONSUMED_ENABLE_SERVICE).unwrap();
    let enable_request: MessageFormat = serde_json5::from_str(ENABLE_REQUEST_FORMAT).unwrap();
    let enable_response: MessageFormat = serde_json5::from_str(ENABLE_RESPONSE_FORMAT).unwrap();
    generator
        .add_consumed_service(&enable, &enable_request, &enable_response, &camera_dep)
        .unwrap();

    let plan: ConsumedAction = serde_json5::from_str(CONSUMED_PLAN_ACTION).unwrap();
    let plan_messages = ConsumedActionMessage {
        goal_request: Some(serde_json5::from_str(PLAN_GOAL_FORMAT).unwrap()),
        goal_response: Some(serde_json5::from_str(PLAN_GOAL_RESPONSE_FORMAT).unwrap()),
        feedback: Some(serde_json5::from_str(PLAN_FEEDBACK_FORMAT).unwrap()),
        result_response: Some(serde_json5::from_str(PLAN_RESULT_FORMAT).unwrap()),
    };
    generator
        .add_consumed_action(
            &plan,
            &plan_messages,
            &DependencyContext::native("brain", "v1", "brain", Cardinality::One),
        )
        .unwrap();

    let commands: NativeEmittedTopic = serde_json5::from_str(PEER_COMMANDS).unwrap();
    let states: NativeEmittedTopic = serde_json5::from_str(PEER_STATES).unwrap();
    let peer = PeerContext {
        link_id: "arm".to_string(),
        pairing_name: "arm_link".to_string(),
        pairing_tag: "v1".to_string(),
        optional: false,
    };
    generator.add_peer_emitted_topic(&commands, &peer).unwrap();
    generator.add_peer_consumed_topic(&states, &peer).unwrap();

    let output_config = copy_config_to_output(&user_node, &output_dir);
    generator
        .build(&output_dir, &test_peppy_dirs(), Default::default())
        .unwrap();
    fs::remove_file(output_config).unwrap();
    config::fingerprint::create_codegen_fingerprint(
        &user_node.join(config::consts::NODE_CONFIG_FILE),
        std::path::Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    // The node crate: lib.rs holds the importable setup (the split node init
    // scaffolds), the test drives it through the generated harness.
    let crate_name = test_wrapper_crate_name(&user_node);
    let manifest = format!(
        r#"[package]
name = "{crate_name}"
version = "0.1.0"
edition = "2024"

[lib]
name = "{crate_name}"
path = "src/lib.rs"

[dependencies]
tokio = {{ version = "1", features = ["macros", "rt-multi-thread", "time"] }}
{main_dep}
{dev_dep}
"#,
        main_dep = crate::helpers::peppygen_dependency_line(&user_node),
        dev_dep = peppygen_dev_dependency_line(&user_node),
    );
    fs::write(user_node.join("Cargo.toml"), manifest).unwrap();
    fs::create_dir_all(user_node.join("src")).unwrap();
    fs::write(user_node.join("src/lib.rs"), NODE_LIB).unwrap();

    let node_test = format!(
        r#"
use std::time::Duration;

use peppygen::fixtures::harness::Harness;
use peppygen::mock::deps::brain::plan_motion as mock_plan;
use peppygen::mock::deps::camera::enable_camera as mock_enable;
use peppygen::mock::deps::camera::video_stream as mock_frames;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mocks_and_fixtures_drive_the_node_end_to_end() {{
    let (mut harness, mut mocks) = Harness::start({crate_name}::setup)
        .await
        .expect("harness should start");

    // ---- Frame 1: the happy path through every generated surface. ----
    mocks
        .deps
        .camera
        .video_stream
        .publish(&mock_frames::Message {{ width: 7 }})
        .await
        .expect("first frame should deliver (readiness is deterministic)");

    let (request, responder) = mocks
        .deps
        .camera
        .enable_camera
        .next_request(Duration::from_secs(10))
        .await
        .expect("the node should poll the mock service");
    assert!(request.enable);
    responder
        .respond(mock_enable::ResponseData::new(true))
        .await
        .expect("mock respond should succeed");

    let pending = mocks
        .deps
        .brain
        .plan_motion
        .next_goal(Duration::from_secs(10))
        .await
        .expect("the node should fire a goal");
    assert_eq!(pending.request.arm_id, 7, "goal carries the frame width");
    let active = pending
        .accept(mock_plan::GoalResponseData::new(true))
        .await
        .expect("accept should succeed");
    active
        .publish_feedback(&mock_plan::FeedbackMessage {{ progress: 0.5 }})
        .await
        .expect("feedback should publish");
    active
        .complete(&mock_plan::ResultResponseData::new(true))
        .await
        .expect("complete should succeed");

    let status = tokio::time::timeout(Duration::from_secs(10), harness.emitted.status.next())
        .await
        .expect("the node should publish its first status")
        .expect("status should decode")
        .expect("status subscription should be open");
    assert_eq!(status.outcome, "done fb=0.5 ok=true");

    let command = mocks
        .pairings
        .arm
        .joint_commands
        .next()
        .await
        .expect("pairing command should decode")
        .expect("pairing subscription should be open");
    assert!((command.max_velocity - 0.5).abs() < f64::EPSILON);

    // The node's own exposed service, driven through fixtures.
    let pong = peppygen::fixtures::exposed_services::ping::poll(
        &harness,
        &peppygen::fixtures::exposed_services::ping::RequestData {{ value: 21 }},
        Duration::from_secs(10),
    )
    .await
    .expect("fixtures poll of the node's service should succeed");
    assert_eq!(pong.doubled, 42);

    // ---- Frame 2: deterministic producer loss mid-goal. ----
    mocks
        .deps
        .camera
        .video_stream
        .publish(&mock_frames::Message {{ width: 9 }})
        .await
        .expect("second frame should deliver");
    let (_request, responder) = mocks
        .deps
        .camera
        .enable_camera
        .next_request(Duration::from_secs(10))
        .await
        .expect("the node should poll the mock service again");
    responder
        .respond(mock_enable::ResponseData::new(true))
        .await
        .expect("mock respond should succeed");

    let pending = mocks
        .deps
        .brain
        .plan_motion
        .next_goal(Duration::from_secs(10))
        .await
        .expect("the node should fire a second goal");
    let _active = pending
        .accept(mock_plan::GoalResponseData::new(true))
        .await
        .expect("accept should succeed");
    // Stop the whole brain mock with the goal context still held: the
    // producer-loss shape. The node must observe the typed loss, not a
    // clean close and not a hang.
    mocks.deps.brain.stop();

    let status = tokio::time::timeout(Duration::from_secs(10), harness.emitted.status.next())
        .await
        .expect("the node should report the producer loss on its status topic")
        .expect("status should decode")
        .expect("status subscription should be open");
    assert_eq!(status.outcome, "producer-gone");

    harness.shutdown().await.expect("clean shutdown");
}}

// Sim time under the harness: the test is the simulator. `use_sim_time`
// boots the node with the sim-clock source installed, and every virtual
// instant the test drives through `harness.clock.tick(..)` is what the
// node's `peppygen::clock::now_ns` observes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sim_time_is_test_driven_under_the_harness() {{
    let (mut harness, _mocks) = Harness::start_with(
        peppygen::fixtures::harness::Config {{
            use_sim_time: true,
            ..Default::default()
        }},
        {crate_name}::setup_sim_clock,
    )
    .await
    .expect("harness should start in sim time");

    // tick() itself waits for the node's clock subscription (opened by
    // peppygen::clock::init) before publishing, so nothing here sleeps.
    harness.clock.tick(1_000).await.expect("first tick");
    let status = tokio::time::timeout(Duration::from_secs(10), harness.emitted.status.next())
        .await
        .expect("the node should observe the first sim instant")
        .expect("status should decode")
        .expect("status subscription should be open");
    assert_eq!(status.outcome, "t=1000");

    harness.clock.tick(2_000).await.expect("second tick");
    let status = tokio::time::timeout(Duration::from_secs(10), harness.emitted.status.next())
        .await
        .expect("the node should observe the second sim instant")
        .expect("status should decode")
        .expect("status subscription should be open");
    assert_eq!(status.outcome, "t=2000");

    // Wall-clock knobs are refused in sim mode, loudly.
    harness
        .clock
        .set_offset_ns(1)
        .expect_err("sim time has no wall offset to skew");

    harness.shutdown().await.expect("clean shutdown");
}}
"#
    );
    fs::create_dir_all(user_node.join("tests")).unwrap();
    fs::write(user_node.join("tests/e2e.rs"), node_test).unwrap();

    run_cargo_test(&user_node);
}
