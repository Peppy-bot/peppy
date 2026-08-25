//! The runtime codec against a generated provider node, over the wire.
//!
//! A stub node generated from a fixture contract runs on an ephemeral
//! router. The runtime side reaches it with no generated code at all: the
//! type-erased topic, service and action clients of `message-codec`, each
//! converting with a codec laid out from the contract's message formats.
//!
//! Every shape a message format can take crosses the wire in both
//! directions through `echo_everything`: the runtime encodes the fixture
//! JSON into the request, the generated handler decodes it into its typed
//! structs and serialises it back as the response, and the runtime decodes
//! that. The response must decode to the fixture and re-encode to the very
//! bytes the runtime sent, which is only possible when both sides agree on
//! every field's layout and value. A generated publisher then drives a
//! camera-sized frame to the runtime's subscription, and the runtime walks
//! the `count` action through admission, feedback, completion,
//! cancellation and rejection.

use crate::helpers::{
    DEFAULT_WAIT_TIMEOUT, STUB_NODE_CONFIG, WaitContext, compile_project, copy_config_to_output,
    init_cargo_user_node, init_test_env, send_shutdown, spawn_cargo_run, test_peppy_dirs,
    test_tmp_root, wait_for_child, wait_for_health_service_reachable_or_exit,
};
use config::consts::{PEPPYGEN_OUTPUT_PATH, RUNTIME_CONFIG_VAR_NAME};
use config::node::MessageFormat;
use config::runtime::{Name, NodeInstanceConfig, RuntimeConfig};
use daemon_config::contract::PeppyContractParser;
use generator::{ContractOrigin, LanguageGenerator};
use mcp_test_support::register_contract_members;
use message_codec::MessageCodec;
use message_codec::consumer::{
    ActionClient, ConsumerIdentity, GoalOutcome, MemberBinding, ServiceClient, TopicConsumer,
};
use peppy_mcp_runtime::bridge::bytes_to_base64;
use peppylib::config::QoSProfile;
use peppylib::messaging::{CancelState, ProducerRef, SenderTarget};
use peppylib::runtime::CancellationToken;
use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;

const TEST_CORE_NODE: &str = "test_core";
const STUB_INSTANCE_ID: &str = "codec_stub_instance";
const STUB_NODE_NAME: &str = "generated_node";
const CONSUMER_INSTANCE_ID: &str = "codec_consumer";
const SHUTDOWN_SENDER_INSTANCE_ID: &str = "test_shutdown_sender";
const LINK_ID: &str = "codec_impl";
const CONTRACT_NAME: &str = "codec_fixture";
const CONTRACT_TAG: &str = "v1";

/// The deadline every call gets: generous, because the assertions are about
/// bytes and values, never about timing.
const CALL_DEADLINE: Duration = Duration::from_secs(30);

const FRAME_WIDTH: u16 = 640;
const FRAME_HEIGHT: u16 = 480;

const EVERYTHING_FORMAT: &str =
    include_str!("../../../message-codec-internal/tests/fixtures/everything.json5");
const FRAME_FORMAT: &str =
    include_str!("../../../message-codec-internal/tests/fixtures/frame.json5");
const EVERYTHING_FULL: &str =
    include_str!("../../../message-codec-internal/tests/fixtures/everything.full.json");
const EVERYTHING_MINIMAL: &str =
    include_str!("../../../message-codec-internal/tests/fixtures/everything.minimal.json");

const GOAL_FORMAT: &str =
    r#"{ steps: "u32", label: "string", options: { $type: "object", uppercase: "bool" } }"#;
const FEEDBACK_FORMAT: &str = r#"{ step: "u32", note: "string" }"#;
const RESULT_FORMAT: &str = r#"{ total: "u64", notes: { $type: "array", $items: "string" } }"#;

fn fixture_contract() -> String {
    format!(
        r#"{{
        peppy_schema: "contract/v1",
        manifest: {{ name: "{CONTRACT_NAME}", tag: "{CONTRACT_TAG}" }},
        interfaces: {{
            topics: [
                {{ name: "frame", message_format: {FRAME_FORMAT} }},
            ],
            services: [
                {{
                    name: "echo_everything",
                    request_message_format: {EVERYTHING_FORMAT},
                    response_message_format: {EVERYTHING_FORMAT},
                }},
                {{ name: "ping" }},
            ],
            actions: [
                {{
                    name: "count",
                    goal_service: {{ request_message_format: {GOAL_FORMAT} }},
                    feedback_topic: {{ message_format: {FEEDBACK_FORMAT} }},
                    result_service: {{ response_message_format: {RESULT_FORMAT} }},
                }},
            ],
        }},
    }}"#
    )
}

/// The generated provider. `echo_everything` moves every field of the
/// decoded request into the response; `count` walks `steps` feedback
/// messages to completion (noting the label, upper-cased when the goal's
/// nested `options` ask for it), parks goals of 100000 or more steps on the
/// cancel signal, and rejects a goal of zero steps; `frame` publishes one
/// camera-sized rgb8 frame every 100 ms.
const STUB_MAIN: &str = r#"
use peppygen::emitted_topics::codec_impl::frame;
use peppygen::exposed_actions::codec_impl::count;
use peppygen::exposed_services::codec_impl::{echo_everything, ping};
use peppygen::{NodeBuilder, Result};
use std::time::{Duration, UNIX_EPOCH};

fn echo(data: echo_everything::RequestData) -> echo_everything::Response {
    use echo_everything::{
        Response, ResponseMaybePose, ResponsePose, ResponseProfile,
        ResponseProfileWhiteBalance, ResponseSamplesItem,
    };
    Response {
        flag: data.flag,
        label: data.label,
        blob: data.blob,
        stamp: data.stamp,
        tiny: data.tiny,
        small: data.small,
        medium: data.medium,
        big: data.big,
        tiny_signed: data.tiny_signed,
        small_signed: data.small_signed,
        medium_signed: data.medium_signed,
        big_signed: data.big_signed,
        ratio: data.ratio,
        precise: data.precise,
        note: data.note,
        attachment: data.attachment,
        seen_at: data.seen_at,
        checksum: data.checksum,
        pixels: data.pixels,
        gains: data.gains,
        offsets: data.offsets,
        flags: data.flags,
        counters: data.counters,
        deltas: data.deltas,
        weights: data.weights,
        tags: data.tags,
        chunks: data.chunks,
        pose: ResponsePose {
            x_m: data.pose.x_m,
            y_m: data.pose.y_m,
            frame: data.pose.frame,
        },
        profile: ResponseProfile {
            gamma: data.profile.gamma,
            white_balance: ResponseProfileWhiteBalance {
                red: data.profile.white_balance.red,
                blue: data.profile.white_balance.blue,
            },
        },
        samples: data
            .samples
            .into_iter()
            .map(|sample| ResponseSamplesItem {
                offset: sample.offset,
                value: sample.value,
                label: sample.label,
                taken_at: sample.taken_at,
                history: sample.history,
            })
            .collect(),
        maybe_pose: data.maybe_pose.map(|pose| ResponseMaybePose { x_m: pose.x_m }),
        maybe_tags: data.maybe_tags,
    }
}

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        let runner = node_runner.clone();
        tokio::spawn(async move {
            loop {
                echo_everything::handle_next_request(&runner, |request| Ok(echo(request.data)))
                    .await
                    .expect("handle echo_everything");
            }
        });
        let runner = node_runner.clone();
        tokio::spawn(async move {
            loop {
                ping::handle_next_request(&runner, |_request| Ok(()))
                    .await
                    .expect("handle ping");
            }
        });
        let runner = node_runner.clone();
        tokio::spawn(async move {
            let mut action = count::ActionHandle::expose(&runner)
                .await
                .expect("expose count");
            loop {
                let next = action
                    .handle_goal_next_request(|request| -> Result<count::GoalDecision> {
                        if request.data.steps == 0 {
                            return Ok(count::GoalDecision::reject("steps must be positive"));
                        }
                        Ok(count::GoalDecision::accept())
                    })
                    .await;
                let Ok(Some(ctx)) = next else { break };
                let steps = ctx.request().data.steps;
                let label = if ctx.request().data.options.uppercase {
                    ctx.request().data.label.to_uppercase()
                } else {
                    ctx.request().data.label.clone()
                };
                let note = |step: u32| format!("{label}#{step}");
                if steps >= 100000 {
                    let mut step = 0;
                    loop {
                        step += 1;
                        ctx.publish_feedback(step, note(step)).await.expect("publish feedback");
                        tokio::select! {
                            _ = ctx.cancel_signal() => break,
                            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                        }
                    }
                    ctx.complete_cancelled(u64::from(step), vec![note(step)])
                        .await
                        .expect("complete cancelled");
                } else {
                    let mut notes = Vec::new();
                    for step in 1..=steps {
                        notes.push(note(step));
                        ctx.publish_feedback(step, note(step)).await.expect("publish feedback");
                    }
                    ctx.complete(u64::from(steps), notes).await.expect("complete");
                }
            }
        });
        let runner = node_runner.clone();
        tokio::spawn(async move {
            let publisher = frame::declare_publisher(&runner)
                .await
                .expect("declare frame publisher");
            let pixels: Vec<u8> = (0..640usize * 480 * 3).map(|i| (i % 251) as u8).collect();
            let stamp = UNIX_EPOCH + Duration::new(1_700_000_000, 500_000_000);
            loop {
                let payload = frame::build_message(pixels.clone(), "rgb8".to_owned(), 640, 480, stamp)
                    .expect("build frame");
                publisher.publish(payload).await.expect("publish frame");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        Ok(())
    })
}
"#;

fn format(text: &str) -> MessageFormat {
    serde_json5::from_str(text).expect("the fixture format parses")
}

fn codec(name: &str, text: &str) -> MessageCodec {
    MessageCodec::new(name, format(text)).expect("the fixture format lays out")
}

fn binding(member: &str) -> MemberBinding {
    MemberBinding {
        target: SenderTarget::contract(CONTRACT_NAME, CONTRACT_TAG).expect("valid target"),
        member: member.to_string(),
        producers: vec![ProducerRef::new(TEST_CORE_NODE, STUB_INSTANCE_ID)],
    }
}

fn expected_frame() -> Value {
    let pixels: Vec<u8> = (0..FRAME_WIDTH as usize * FRAME_HEIGHT as usize * 3)
        .map(|i| (i % 251) as u8)
        .collect();
    json!({
        "frame": bytes_to_base64(&pixels),
        "encoding": "rgb8",
        "width": FRAME_WIDTH,
        "height": FRAME_HEIGHT,
        "stamp": "2023-11-14T22:13:20.500000000Z",
    })
}

/// Bounds a wait on the provider by [`DEFAULT_WAIT_TIMEOUT`]; the provider
/// answers on its own schedule and the assertion is never about timing.
async fn bounded<T>(what: &str, future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(DEFAULT_WAIT_TIMEOUT, future)
        .await
        .unwrap_or_else(|_| panic!("{what} did not happen within {DEFAULT_WAIT_TIMEOUT:?}"))
}

/// Calls `echo_everything` with `value` and proves the round trip: the
/// generated side decoded every field the runtime encoded, and re-encoded
/// them to the bytes the runtime produces for the same JSON.
async fn assert_echo_round_trips(
    client: &ServiceClient,
    messenger: &peppylib::MessengerHandle,
    identity: &ConsumerIdentity,
    codec: &MessageCodec,
    value: &Value,
) {
    let echoed = bounded(
        "echo_everything answers",
        client.call(
            messenger,
            identity,
            &binding("echo_everything"),
            &ProducerRef::new(TEST_CORE_NODE, STUB_INSTANCE_ID),
            value,
            CALL_DEADLINE,
        ),
    )
    .await
    .expect("echo_everything succeeds");
    assert_eq!(
        &echoed, value,
        "the generated handler decoded and re-encoded every field"
    );
    assert_eq!(
        codec.encode(&echoed).expect("the echo re-encodes"),
        codec.encode(value).expect("the fixture encodes"),
        "the echo re-encodes to the bytes the runtime sent"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_runtime_codec_exchanges_every_message_shape_with_a_generated_node() {
    let instance = pmi::ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    // --- The generated provider.
    let temp_dir = TempDir::new_in(test_tmp_root()).expect("temp dir for the stub");
    let (mut stub_generator, output_dir, user_node, _config_path) =
        init_test_env::<generator::RustGenerator>(&temp_dir, STUB_NODE_CONFIG);
    let origin = ContractOrigin {
        link_id: LINK_ID.to_string(),
        contract_name: CONTRACT_NAME.to_string(),
        contract_tag: CONTRACT_TAG.to_string(),
    };
    let contract = PeppyContractParser::from_content(&fixture_contract()).expect("contract parses");
    register_contract_members(&mut stub_generator, &contract, &origin);
    let output_config = copy_config_to_output(&user_node, &output_dir);
    stub_generator
        .build(&output_dir, &test_peppy_dirs(), Default::default())
        .expect("build stub peppygen");
    fs::remove_file(output_config).expect("remove staged config");
    config::fingerprint::create_codegen_fingerprint(
        &user_node.join(config::consts::NODE_CONFIG_FILE),
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );
    let runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstanceConfig::new(Name::new(STUB_INSTANCE_ID).expect("valid instance id")),
        STUB_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .expect("stub runtime config builds");
    let runtime_config_path = temp_dir.path().join("peppy_runtime.json5");
    runtime_config
        .save_json5_launch_config(&runtime_config_path)
        .expect("write stub runtime config");
    init_cargo_user_node(&user_node);
    fs::write(user_node.join("src").join("main.rs"), STUB_MAIN).expect("write stub main");
    compile_project(&user_node);

    let config_str = runtime_config_path.to_str().expect("utf-8 path").to_owned();
    let mut stub_child = spawn_cargo_run(&user_node, &[(RUNTIME_CONFIG_VAR_NAME, &config_str)]);

    // --- The runtime side: one session, no generated code.
    let messenger = peppylib::MessengerHandle::connect(&router_host, router_port)
        .await
        .expect("the consumer session connects");
    let wait = WaitContext {
        messenger: &messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_core_node: TEST_CORE_NODE,
    };
    wait_for_health_service_reachable_or_exit(
        &wait,
        STUB_NODE_NAME,
        STUB_INSTANCE_ID,
        &mut stub_child,
        &user_node,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;
    let identity = ConsumerIdentity {
        core_node: TEST_CORE_NODE.to_string(),
        instance_id: CONSUMER_INSTANCE_ID.to_string(),
    };
    let producer = ProducerRef::new(TEST_CORE_NODE, STUB_INSTANCE_ID);

    // --- Every shape, both directions, through the echo service.
    let everything = codec("echo_everything", EVERYTHING_FORMAT);
    let echo = ServiceClient::new(Some(everything.clone()), Some(everything.clone()));
    let full: Value = serde_json::from_str(EVERYTHING_FULL).expect("fixture json");
    let minimal: Value = serde_json::from_str(EVERYTHING_MINIMAL).expect("fixture json");
    assert_echo_round_trips(&echo, &messenger, &identity, &everything, &full).await;
    assert_echo_round_trips(&echo, &messenger, &identity, &everything, &minimal).await;

    // A member without formats carries nothing either way.
    let ping = ServiceClient::new(None, None);
    let answer = bounded(
        "ping answers",
        ping.call(
            &messenger,
            &identity,
            &binding("ping"),
            &producer,
            &json!({}),
            CALL_DEADLINE,
        ),
    )
    .await
    .expect("ping succeeds");
    assert_eq!(answer, json!({}));

    // --- A generated publisher to the runtime's subscription.
    let shutdown = CancellationToken::new();
    let mut frames = TopicConsumer::subscribe(
        &messenger,
        &identity,
        &binding("frame"),
        QoSProfile::Standard,
        codec("frame", FRAME_FORMAT),
        shutdown.clone(),
    )
    .await
    .expect("the frame subscription opens");
    let (from, message) = bounded("a frame arrives", frames.next_message())
        .await
        .expect("the subscription is open");
    assert_eq!(from, producer);
    let frame = frames.decode(&message).expect("the frame decodes");
    assert_eq!(frame, expected_frame());
    assert_eq!(
        frames.codec().encode(&frame).expect("the frame re-encodes"),
        message.payload_bytes().as_ref(),
        "the runtime re-encodes the generated publisher's bytes exactly"
    );
    shutdown.cancel();

    // --- The action walk: completion with feedback, cancellation, rejection.
    let count = ActionClient::new(
        Some(codec("count_goal", GOAL_FORMAT)),
        Some(codec("count_feedback", FEEDBACK_FORMAT)),
        Some(codec("count_result", RESULT_FORMAT)),
    );
    let fire = |goal: Value| {
        let count = count.clone();
        let messenger = &messenger;
        let identity = &identity;
        let producer = &producer;
        async move {
            bounded(
                "the goal is admitted",
                count.fire_goal(
                    messenger,
                    identity,
                    &binding("count"),
                    producer,
                    &goal,
                    QoSProfile::Standard,
                    CALL_DEADLINE,
                ),
            )
            .await
            .expect("the goal is sent")
        }
    };

    let mut handle = fire(json!({
        "steps": 3,
        "label": "abc",
        "options": { "uppercase": true },
    }))
    .await;
    assert!(handle.accepted(), "a positive goal is accepted");
    let mut feedback = Vec::new();
    while let Some(value) = bounded("feedback or the end of the stream", handle.next_feedback())
        .await
        .expect("feedback decodes")
    {
        feedback.push(value);
    }
    assert_eq!(
        feedback,
        [
            json!({ "step": 1, "note": "ABC#1" }),
            json!({ "step": 2, "note": "ABC#2" }),
            json!({ "step": 3, "note": "ABC#3" }),
        ],
        "the generated handler decoded the nested goal options"
    );
    let outcome = bounded(
        "the result answers",
        handle.result(&messenger, CALL_DEADLINE),
    )
    .await
    .expect("the result is requested");
    assert_eq!(
        outcome,
        GoalOutcome::Completed(json!({ "total": "3", "notes": ["ABC#1", "ABC#2", "ABC#3"] }))
    );

    let mut handle = fire(json!({
        "steps": 100000,
        "label": "long",
        "options": { "uppercase": false },
    }))
    .await;
    assert!(handle.accepted());
    let first = bounded("the parked goal reports progress", handle.next_feedback())
        .await
        .expect("feedback decodes")
        .expect("the stream is open");
    assert_eq!(first["note"], "long#1");
    let cancel = bounded(
        "the cancel is acknowledged",
        handle.cancel(&messenger, CALL_DEADLINE),
    )
    .await
    .expect("the cancel is sent");
    assert_eq!(cancel, CancelState::Signalled);
    while bounded("the feedback stream closes", handle.next_feedback())
        .await
        .expect("feedback decodes")
        .is_some()
    {}
    let outcome = bounded(
        "the result answers",
        handle.result(&messenger, CALL_DEADLINE),
    )
    .await
    .expect("the result is requested");
    assert_eq!(outcome, GoalOutcome::Cancelled);

    let handle = fire(json!({
        "steps": 0,
        "label": "none",
        "options": { "uppercase": false },
    }))
    .await;
    assert!(!handle.accepted(), "a zero-step goal is rejected");
    assert_eq!(handle.rejection_reason(), Some("steps must be positive"));

    // --- Teardown: cooperative shutdown through the framework service.
    send_shutdown(
        &messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        STUB_NODE_NAME,
        TEST_CORE_NODE,
        STUB_INSTANCE_ID,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;
    let output = wait_for_child(&mut stub_child, Some(Duration::from_secs(10)), &user_node);
    assert!(
        output.status.success(),
        "the stub exits cleanly:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
