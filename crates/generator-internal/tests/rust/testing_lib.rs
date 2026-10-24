//! End-to-end proof of the generated test surfaces (Rust): generate a
//! consumer node with topic + service + action + pairing links, write a real
//! node (`lib.rs` setup) and a real test using `fixtures::harness::Harness` +
//! the per-link mocks, then run `cargo test` in the node: the full loop over
//! real zenoh: mock publish → node consumes → node polls the mock service →
//! node drives the mock action (accept → feedback → complete) → node's own
//! emissions observed via fixtures (emitted topic + pairing slot), plus the
//! deterministic producer-loss path (`Mock::stop` mid-goal → the node sees
//! `ActionFeedbackProducerGone`, never a hang or clean close). A second test
//! boots the same node as a consumer of the harness's clock domain
//! (`Config::clock`) and plays the simulator: every `harness.clock.tick(..)`
//! instant is what the node's `peppygen::clock::now_ns` observes. A third
//! boots it as that domain's publisher: the node supplies the instants, the
//! test reads them back off the domain's stream and never ticks. A fourth
//! leaves an emitted topic unread while the node publishes more than its
//! queue holds: the harness's session still receives the node's next
//! message, and the unread queue keeps the newest messages. A fifth drives
//! every way a call fails through the mocks: an error reply with a kind of
//! the contract of the slot, a refusal, a failed goal and a goal cancelled
//! for a reason; the node reads each as the typed `Failure` of its member.

use config::failure_kind::MemberSection;
use config::node::{
    Cardinality, ConsumedAction, ConsumedService, ConsumedTopic, MessageFormat, NativeEmittedTopic,
    NativeExposedService, TopicRetention,
};
use daemon_config::consts::PEPPYLIB_OUTPUT_PATH;
use generator::{ConsumedActionMessage, DependencyContext, LanguageGenerator, PeerContext};
use std::fs;

use crate::helpers::{
    generate_node_project, peppygen_dev_dependency_line, run_cargo_test, test_wrapper_crate_name,
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
      contracts: [
        { name: "lens_contract", tag: "v1", link_id: "lens" }
      ],
      pairings: [
        { name: "arm_link", tag: "v1", role: "controller", link_id: "arm" },
        { name: "arm_link", tag: "v1", role: "controller", link_id: "limbs", cardinality: "zero_or_more" }
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

const EMITTED_MARKER: &str = r#"{
  name: "marker",
  qos_profile: "reliable",
  message_format: { statuses: "u32" }
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

const CONSUMED_LENS_SERVICE: &str = r#"{ link_id: "lens", name: "check_lens" }"#;
const LENS_RESPONSE_FORMAT: &str = r#"{ clear: "bool" }"#;
/// The `failure_kinds` block of `lens_contract`.
const LENS_FAILURE_KINDS: &[(&str, &str)] = &[("lens_covered", "Something covers the lens")];

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
    let limb_commands =
        peppygen::paired_topics::limbs::joint_commands::declare_publisher(&node_runner).await?;
    // The node's own exposed service, answered concurrently with the frame
    // loop. A value of zero gets an error reply with a kind of peppy.
    let ping_runner = Arc::clone(&node_runner);
    tokio::spawn(async move {
        use peppygen::exposed_services::ping;
        loop {
            let answered = ping::handle_next_request(&ping_runner, |request| {
                if request.data.value == 0 {
                    return Err(ping::Failure::new(
                        ping::FailureKind::PreconditionFailed,
                        "nothing to double",
                    ));
                }
                Ok(ping::Response::new(request.data.value * 2))
            })
            .await;
            if answered.is_err() {
                break;
            }
        }
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
            peppygen::DEFAULT_FEEDBACK_BOUND,
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
        // The multi slot: one message per pair, shaped by the copy the peer
        // belongs to, addressed to that peer alone.
        for member in peppygen::paired_topics::limbs::joint_commands::peers(&node_runner)? {
            let max_velocity = match member.copy.as_deref() {
                Some("robot_a") => 1.0,
                _ => 2.0,
            };
            limb_commands
                .publish_to(
                    &member.info,
                    peppygen::paired_topics::limbs::joint_commands::build_message(max_velocity)?,
                )
                .await?;
        }
    }
    Ok(())
}

/// Failures entry point: per frame, makes the one call the frame's width
/// names, and reports how it failed on the status topic. Width 1 polls the
/// `lens` service; any other width fires a `plan_motion` goal.
pub async fn setup_failures(
    _parameters: peppygen::Parameters,
    node_runner: Arc<peppygen::NodeRunner>,
) -> peppygen::Result<()> {
    use peppygen::consumed_actions::brain::plan_motion;
    use peppygen::consumed_services::lens::check_lens;
    let status =
        peppygen::emitted_topics::status::declare_publisher(&node_runner).await?;
    let mut frames =
        peppygen::consumed_topics::camera::video_stream::subscribe(&node_runner).await?;
    while let Some((_producer, frame)) = frames.next().await? {
        let outcome = match frame.width {
            1 => {
                let lens = check_lens::bound_producer(&node_runner);
                let error = check_lens::poll(&node_runner, lens, Duration::from_secs(10))
                    .await
                    .expect_err("the mock gives an error reply");
                let failure = check_lens::Failure::of(&error)
                    .expect("the error reply has a kind of the contract");
                format!("service {failure}")
            }
            width => {
                let brain = plan_motion::bound_producer(&node_runner);
                let goal = plan_motion::ActionHandle::fire_goal(
                    &node_runner,
                    brain,
                    Duration::from_secs(10),
                    plan_motion::GoalRequest { arm_id: width as u16 },
                    peppygen::DEFAULT_FEEDBACK_BOUND,
                )
                .await?;
                match &goal.refusal {
                    Some(refusal) => format!("refused {refusal}"),
                    None => match goal.get_result(Duration::from_secs(10)).await?.outcome {
                        plan_motion::ResultOutcome::Failed(failure) => format!("failed {failure}"),
                        plan_motion::ResultOutcome::Cancelled { reason, data } => {
                            format!("cancelled reason={reason} ok={}", data.success)
                        }
                        other => format!("unexpected outcome: {other:?}"),
                    },
                }
            }
        };
        status
            .publish(peppygen::emitted_topics::status::build_message(outcome)?)
            .await?;
    }
    Ok(())
}

/// Clock-consumer entry point: reports every instant it observes on the
/// status topic, so the test can assert the exact values it drove through
/// `harness.clock.tick(..)`.
pub async fn setup_clock_consumer(
    _parameters: peppygen::Parameters,
    node_runner: Arc<peppygen::NodeRunner>,
) -> peppygen::Result<()> {
    peppygen::clock::init(&node_runner).await?;
    let status =
        peppygen::emitted_topics::status::declare_publisher(&node_runner).await?;
    let mut last = 0u64;
    loop {
        // Wait on observation: before the domain's first tick now_ns errors
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

/// Status-burst entry point: per frame, publishes as many statuses as the
/// frame's width (`status-1` first), then one marker that counts them. Both
/// topics are `reliable`, so the link delivers them in publish order.
pub async fn setup_status_burst(
    _parameters: peppygen::Parameters,
    node_runner: Arc<peppygen::NodeRunner>,
) -> peppygen::Result<()> {
    let status =
        peppygen::emitted_topics::status::declare_publisher(&node_runner).await?;
    let marker =
        peppygen::emitted_topics::marker::declare_publisher(&node_runner).await?;
    let mut frames =
        peppygen::consumed_topics::camera::video_stream::subscribe(&node_runner).await?;
    while let Some((_producer, frame)) = frames.next().await? {
        for index in 1..=frame.width {
            status
                .publish(peppygen::emitted_topics::status::build_message(format!(
                    "status-{index}"
                ))?)
                .await?;
        }
        marker
            .publish(peppygen::emitted_topics::marker::build_message(frame.width)?)
            .await?;
    }
    Ok(())
}

/// Clock-publisher entry point: the deployment (here the harness config)
/// names this node the publisher of a clock domain, so it supplies that
/// domain's instants, reporting which domain on the status topic and then
/// publishing on until shutdown. A publisher stamps from its own clock, so
/// nothing here reads `now_ns`.
pub async fn setup_clock_publisher(
    _parameters: peppygen::Parameters,
    node_runner: Arc<peppygen::NodeRunner>,
) -> peppygen::Result<()> {
    peppygen::clock::init(&node_runner).await?;
    let publisher = peppylib::clock::ClockPublisher::for_node(&node_runner)
        .await?
        .expect("the harness named this node a clock domain's publisher");
    let status =
        peppygen::emitted_topics::status::declare_publisher(&node_runner).await?;
    status
        .publish(peppygen::emitted_topics::status::build_message(format!(
            "publishes {}",
            publisher.domain()
        ))?)
        .await?;
    let mut time_ns = 1_000u64;
    loop {
        publisher.publish(time_ns).await?;
        time_ns += 1_000;
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}
"#;

#[test]
fn generated_mocks_and_fixtures_drive_a_node_end_to_end() {
    let temp_dir = tempfile::TempDir::new_in(config_test_support::test_tmp_root()).unwrap();
    let user_node =
        generate_node_project::<generator::RustGenerator>(&temp_dir, NODE_CONFIG, |generator| {
            generator.set_node_identity("generated_node", "v1");

            let status: NativeEmittedTopic = serde_json5::from_str(EMITTED_STATUS).unwrap();
            generator.add_emitted_topic(&status, None).unwrap();
            let marker: NativeEmittedTopic = serde_json5::from_str(EMITTED_MARKER).unwrap();
            generator.add_emitted_topic(&marker, None).unwrap();
            let ping: NativeExposedService = serde_json5::from_str(EXPOSED_PING).unwrap();
            generator.add_exposed_service(&ping, None).unwrap();

            let frame_topic: ConsumedTopic = serde_json5::from_str(CONSUMED_FRAME_TOPIC).unwrap();
            let frame_format: MessageFormat = serde_json5::from_str(CONSUMED_FRAME_FORMAT).unwrap();
            let camera_dep =
                DependencyContext::native("uvc_camera", "v1", "camera", Cardinality::One);
            generator
                .add_consumed_topic(
                    &frame_topic,
                    frame_format,
                    TopicRetention::LiveOnly,
                    &camera_dep,
                )
                .unwrap();

            let enable: ConsumedService = serde_json5::from_str(CONSUMED_ENABLE_SERVICE).unwrap();
            let enable_request: MessageFormat =
                serde_json5::from_str(ENABLE_REQUEST_FORMAT).unwrap();
            let enable_response: MessageFormat =
                serde_json5::from_str(ENABLE_RESPONSE_FORMAT).unwrap();
            generator
                .add_consumed_service(&enable, &enable_request, &enable_response, &camera_dep)
                .unwrap();

            let lens: ConsumedService = serde_json5::from_str(CONSUMED_LENS_SERVICE).unwrap();
            let lens_response: MessageFormat = serde_json5::from_str(LENS_RESPONSE_FORMAT).unwrap();
            generator
                .add_consumed_service(
                    &lens,
                    &MessageFormat::default(),
                    &lens_response,
                    &DependencyContext::contract(
                        "lens_contract",
                        "v1",
                        "lens",
                        Cardinality::One,
                        crate::helpers::failure_kinds(
                            LENS_FAILURE_KINDS,
                            &[(MemberSection::Service, "check_lens")],
                        ),
                    ),
                )
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
                cardinality: config::node::Cardinality::One,
            };
            generator.add_peer_emitted_topic(&commands, &peer).unwrap();
            generator.add_peer_consumed_topic(&states, &peer).unwrap();
            let limbs = PeerContext {
                link_id: "limbs".to_string(),
                cardinality: config::node::Cardinality::ZeroOrMore,
                ..peer
            };
            generator.add_peer_emitted_topic(&commands, &limbs).unwrap();
        });

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
peppylib = {{ path = "{PEPPYLIB_OUTPUT_PATH}" }}
{main_dep}
{dev_dep}
"#,
        main_dep = crate::helpers::peppygen_dependency_line(&user_node),
        dev_dep = peppygen_dev_dependency_line(&user_node),
    );
    fs::write(user_node.join("Cargo.toml"), manifest).unwrap();
    fs::create_dir_all(user_node.join("src")).unwrap();
    fs::write(user_node.join("src/lib.rs"), NODE_LIB).unwrap();

    // The harness's queue for a `reliable` topic: the standard tier's size,
    // which a harness session takes from `SubscriberBufferSizes::default()`.
    let status_queue = pmi::SubscriberBufferSizes::default().standard;
    let node_test = format!(
        r#"
use std::time::Duration;

use peppygen::fixtures::harness::{{Config, Harness, PeerMemberSpec}};
use peppygen::mock::deps::brain::plan_motion as mock_plan;
use peppygen::mock::deps::camera::enable_camera as mock_enable;
use peppygen::mock::deps::camera::video_stream as mock_frames;
use peppygen::mock::deps::lens::check_lens as mock_lens;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mocks_and_fixtures_drive_the_node_end_to_end() {{
    // Two mock peers in the multi slot, one per robot copy.
    let config = Config {{
        limbs_members: vec![
            PeerMemberSpec {{
                instance_id: "left".to_string(),
                copy: Some("robot_a".to_string()),
            }},
            PeerMemberSpec {{
                instance_id: "right".to_string(),
                copy: Some("robot_b".to_string()),
            }},
        ],
        ..Config::default()
    }};
    let (mut harness, mut mocks) = Harness::start_with(config, {crate_name}::setup)
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

    // Each mock peer of the multi slot hears the one message addressed to
    // it, shaped by the copy the harness seeded it in.
    let left = mocks.pairings.limbs[0]
        .joint_commands
        .next()
        .await
        .expect("left limb command should decode")
        .expect("left limb subscription should be open");
    assert!((left.max_velocity - 1.0).abs() < f64::EPSILON);
    let right = mocks.pairings.limbs[1]
        .joint_commands
        .next()
        .await
        .expect("right limb command should decode")
        .expect("right limb subscription should be open");
    assert!((right.max_velocity - 2.0).abs() < f64::EPSILON);

    // The node's own exposed service, driven through fixtures.
    let pong = peppygen::fixtures::exposed_services::ping::poll(
        &harness,
        &peppygen::fixtures::exposed_services::ping::RequestData {{ value: 21 }},
        Duration::from_secs(10),
    )
    .await
    .expect("fixtures poll of the node's service should succeed");
    assert_eq!(pong.doubled, 42);

    // Its error reply reaches the fixture with its kind.
    let error = peppygen::fixtures::exposed_services::ping::poll(
        &harness,
        &peppygen::fixtures::exposed_services::ping::RequestData {{ value: 0 }},
        Duration::from_secs(10),
    )
    .await
    .expect_err("the node gives an error reply for zero");
    let failure = peppygen::fixtures::exposed_services::ping::Failure::of(&error)
        .expect("the error reply has a kind of the member");
    assert_eq!(
        failure.kind,
        peppygen::fixtures::exposed_services::ping::FailureKind::PreconditionFailed
    );
    assert_eq!(failure.message, "nothing to double");

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

// Every way a call fails reaches the node as the typed `Failure` of its
// member: an error reply with a kind of the contract of the slot, a refusal,
// a failed goal with no result, and a goal cancelled for a reason.
async fn publish_frame(mocks: &peppygen::fixtures::harness::Mocks, width: u32) {{
    mocks
        .deps
        .camera
        .video_stream
        .publish(&mock_frames::Message {{ width }})
        .await
        .expect("the frame should deliver")
}}

async fn next_status(harness: &mut Harness) -> String {{
    tokio::time::timeout(Duration::from_secs(10), harness.emitted.status.next())
        .await
        .expect("the node should report the call")
        .expect("status should decode")
        .expect("status subscription should be open")
        .outcome
}}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failures_reach_the_node_typed() {{
    let (mut harness, mut mocks) =
        Harness::start_with(Config::default(), {crate_name}::setup_failures)
            .await
            .expect("harness should start");

    publish_frame(&mocks, 1).await;
    let responder = mocks
        .deps
        .lens
        .check_lens
        .next_request(Duration::from_secs(10))
        .await
        .expect("the node should poll the lens");
    responder
        .respond_failure(mock_lens::FailureKind::LensCovered, "tape on the lens")
        .await
        .expect("the error reply should send");
    assert_eq!(next_status(&mut harness).await, "service lens_covered: tape on the lens");

    publish_frame(&mocks, 2).await;
    let pending = mocks
        .deps
        .brain
        .plan_motion
        .next_goal(Duration::from_secs(10))
        .await
        .expect("the node should fire a goal");
    pending
        .refuse(mock_plan::FailureKind::ResourceUnavailable, "the arm is gone")
        .await
        .expect("the refusal should send");
    assert_eq!(next_status(&mut harness).await, "refused resource_unavailable: the arm is gone");

    publish_frame(&mocks, 3).await;
    let active = mocks
        .deps
        .brain
        .plan_motion
        .next_goal(Duration::from_secs(10))
        .await
        .expect("the node should fire a goal")
        .accept(mock_plan::GoalResponseData::new(true))
        .await
        .expect("accept should succeed");
    active
        .fail(mock_plan::FailureKind::AttemptFailed, "the plan diverged")
        .await
        .expect("fail should succeed");
    assert_eq!(next_status(&mut harness).await, "failed attempt_failed: the plan diverged");

    publish_frame(&mocks, 4).await;
    let active = mocks
        .deps
        .brain
        .plan_motion
        .next_goal(Duration::from_secs(10))
        .await
        .expect("the node should fire a goal")
        .accept(mock_plan::GoalResponseData::new(true))
        .await
        .expect("accept should succeed");
    active
        .complete_cancelled("stopped by the operator", &mock_plan::ResultResponseData::new(false))
        .await
        .expect("complete_cancelled should succeed");
    assert_eq!(
        next_status(&mut harness).await,
        "cancelled reason=stopped by the operator ok=false"
    );

    harness.shutdown().await.expect("clean shutdown");
}}

// A consumer of the harness's clock domain: the test is the simulator.
// `Config::clock` boots the node bound to that domain, and every instant
// the test drives through `harness.clock.tick(..)` is what the node's
// `peppygen::clock::now_ns` observes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clock_consumer_reads_the_instants_the_test_drives() {{
    let (mut harness, _mocks) = Harness::start_with(
        peppygen::fixtures::harness::Config {{
            clock: peppylib::testing::HarnessClock::Consumer,
            ..Default::default()
        }},
        {crate_name}::setup_clock_consumer,
    )
    .await
    .expect("harness should start on the clock domain");

    // tick() itself waits for the node's clock subscription (opened by
    // peppygen::clock::init) before publishing, so nothing here sleeps.
    harness.clock.tick(1_000).await.expect("first tick");
    let status = tokio::time::timeout(Duration::from_secs(10), harness.emitted.status.next())
        .await
        .expect("the node should observe the first instant of its domain")
        .expect("status should decode")
        .expect("status subscription should be open");
    assert_eq!(status.outcome, "t=1000");

    harness.clock.tick(2_000).await.expect("second tick");
    let status = tokio::time::timeout(Duration::from_secs(10), harness.emitted.status.next())
        .await
        .expect("the node should observe the second instant of its domain")
        .expect("status should decode")
        .expect("status subscription should be open");
    assert_eq!(status.outcome, "t=2000");

    harness.shutdown().await.expect("clean shutdown");
}}

// The node as its clock domain's publisher: `Config::clock` names it the
// instance supplying the harness domain, and every instant it publishes is
// read back off that domain's stream in order, with the test never ticking.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clock_publisher_supplies_the_instants_the_test_reads() {{
    let (mut harness, _mocks) = Harness::start_with(
        peppygen::fixtures::harness::Config {{
            clock: peppylib::testing::HarnessClock::Publisher,
            ..Default::default()
        }},
        {crate_name}::setup_clock_publisher,
    )
    .await
    .expect("harness should start with the node publishing its domain");

    let status = tokio::time::timeout(Duration::from_secs(10), harness.emitted.status.next())
        .await
        .expect("the node should report the domain it publishes")
        .expect("status should decode")
        .expect("status subscription should be open");
    assert_eq!(
        status.outcome,
        format!(
            "publishes {{}}@{{}}",
            peppylib::testing::HARNESS_CLOCK_DOMAIN,
            peppylib::testing::STANDALONE_CORE_NODE
        )
    );

    // The node supplies this domain, so the harness has nothing to drive.
    harness
        .clock
        .tick(1)
        .await
        .expect_err("the node publishes this domain");

    let mut ticks = peppylib::clock::subscribe(
        harness.node_runner(),
        peppylib::messaging::TopicBuffer::KeepLatest,
    )
    .await
    .expect("subscription to the node's domain");
    let mut next_tick = async || {{
        tokio::time::timeout(Duration::from_secs(10), ticks.on_next_tick())
            .await
            .expect("the node should publish its domain")
            .expect("tick should decode")
            .expect("clock subscription should be open")
            .time()
    }};
    let first = next_tick().await;
    assert!(first >= 1_000, "ticks are the node's own instants: {{first}}");
    assert_eq!(next_tick().await, first + 1_000);
    assert_eq!(next_tick().await, first + 2_000);

    harness.shutdown().await.expect("clean shutdown");
}}

// A test can leave an emitted topic unread. The node publishes one status
// more than the harness's status queue holds, then a marker: the marker
// still reaches the harness's session, and the unread queue then holds every
// status but the first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unread_emitted_topic_never_holds_up_the_harness() {{
    let (mut harness, mocks) =
        Harness::start_with(Config::default(), {crate_name}::setup_status_burst)
            .await
            .expect("harness should start");

    let statuses: u32 = {status_queue} + 1;
    mocks
        .deps
        .camera
        .video_stream
        .publish(&mock_frames::Message {{ width: statuses }})
        .await
        .expect("the frame should deliver");

    let marker = tokio::time::timeout(Duration::from_secs(10), harness.emitted.marker.next())
        .await
        .expect("the unread statuses must not hold up the marker")
        .expect("marker should decode")
        .expect("marker subscription should be open");
    assert_eq!(marker.statuses, statuses);

    // Every status came before the marker, so the queue holds its newest
    // statuses now.
    for index in 2..=statuses {{
        let status = tokio::time::timeout(Duration::from_secs(10), harness.emitted.status.next())
            .await
            .expect("the queue holds the newest statuses")
            .expect("status should decode")
            .expect("status subscription should be open");
        assert_eq!(status.outcome, format!("status-{{index}}"));
    }}

    harness.shutdown().await.expect("clean shutdown");
}}
"#
    );
    fs::create_dir_all(user_node.join("tests")).unwrap();
    fs::write(user_node.join("tests/e2e.rs"), node_test).unwrap();

    run_cargo_test(&user_node);
}
