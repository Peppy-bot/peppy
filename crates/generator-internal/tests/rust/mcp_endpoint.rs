//! Endpoint end-to-end for the generated MCP server node.
//!
//! Runs the actual pipeline on a fixture exposure: validate and emit the
//! node with `generate_exposure_node`, generate its peppygen through the
//! real `generate_peppygen_lib` entry point, compile it, and boot it against
//! an ephemeral zenoh router with a generated stub provider filling its
//! contract slot. A real MCP client then drives the running endpoint over
//! Streamable HTTP under `2026-07-28`: `server/discover` advertising the
//! published identity and hints, catalog listing with caching hints,
//! unavailable-then-served resource reads with the JPEG default
//! representation, a subscription notification after a publish, read-only
//! and mutating tool round-trips, a deadline miss on a service the provider
//! never answers, rejection of restrict violations and unknown names before
//! anything reaches the Peppy graph, and the full action-backed task walk:
//! the capability refusal, `CreateTaskResult`, confirmation through
//! `input_required` and `tasks/update`, feedback-driven progress,
//! cancellation, the terminal-state mapping, and reconnect-and-resume on
//! the same task handle.
//!
//! The design doc's harness sketch boots the node on the `MockAdapter`;
//! compiled nodes always dial a real transport, so like every other
//! communication e2e this uses an ephemeral router instead. Staleness on an
//! injected clock is covered deterministically by the runtime crate's own
//! tests; a real process runs on the real clock, so freshness windows here
//! are wide enough to never expire mid-test.

use crate::helpers::{
    DEFAULT_WAIT_TIMEOUT, STUB_NODE_CONFIG, WaitContext, bind_slot, compile_project,
    copy_config_to_output, init_cargo_user_node, init_test_env, send_shutdown, spawn_cargo_run,
    test_peppy_dirs, wait_for_child, wait_for_health_service_reachable_or_exit,
};
use config::consts::{PEPPYGEN_OUTPUT_PATH, RUNTIME_CONFIG_VAR_NAME};
use config::runtime::{Name, NodeInstanceConfig, RuntimeConfig};
use daemon_config::contract::PeppyContractParser;
use daemon_config::mcp_exposure::PeppyMcpExposureParser;
use daemon_config::repository::ManifestFingerprint;
use generator::{
    ContractOrigin, DeploymentInterface, LanguageGenerator, ResolvedContractDocument,
    generate_exposure_node, generate_peppygen_lib,
};
use mcp_test_support::{
    Client, compile_node, confirmation_accept, connect_with_tasks, ephemeral_port, protocol_error,
    register_contract_members,
};
use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, CancelTaskParams, ClientInfo,
    DetailedTask, ErrorCode, GetTaskParams, ProtocolVersion, ReadResourceRequestParams,
    RequestMetaObject, ServerNotification, SubscriptionFilter, TaskStatus, object,
};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ClientLifecycleMode, ClientServiceExt};
use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;
use std::{fs, thread};
use tempfile::TempDir;

const TEST_CORE_NODE: &str = "test_core";
const MCP_INSTANCE_ID: &str = "mcp_server_instance";
const STUB_INSTANCE_ID: &str = "camera_stub_instance";
const STUB_NODE_NAME: &str = "generated_node";
const SHUTDOWN_SENDER_INSTANCE_ID: &str = "test_shutdown_sender";

const STATUS_URI: &str = "peppy://resource/front_camera.status";
const FRAME_URI: &str = "peppy://resource/front_camera.latest_frame";

const CAMERA_CONTRACT: &str = r#"{
    peppy_schema: "contract/v1",
    manifest: { name: "rgb_camera", tag: "v1" },
    interfaces: {
        topics: [
            {
                name: "video_stream",
                qos_profile: "sensor_data",
                message_format: {
                    frame: { $type: "array", $items: "u8" },
                    encoding: "string",
                    width: "u16",
                    height: "u16",
                },
            },
            {
                name: "camera_status",
                message_format: {
                    battery: "u8",
                    note: "string",
                    recording: "bool",
                },
            },
        ],
        services: [
            {
                name: "video_stream_info",
                response_message_format: {
                    width: "u16",
                    height: "u16",
                    fps: "f32",
                    device: "string",
                },
            },
            {
                name: "set_brightness",
                request_message_format: { value: "i32" },
                response_message_format: { applied: "bool" },
            },
            {
                name: "freeze_probe",
                response_message_format: { ok: "bool" },
            },
        ],
        actions: [
            {
                name: "record_clip",
                goal_service: {
                    request_message_format: { duration_frames: "u32" },
                },
                feedback_topic: {
                    message_format: { frame: "u32" },
                },
                result_service: {
                    response_message_format: { frames_written: "u32" },
                },
            },
        ],
    },
}"#;

/// The exposure under test. Freshness windows are minutes wide on purpose:
/// the compiled node runs on the real clock, and this test asserts policy
/// behavior that is clock-independent (staleness itself is covered on an
/// injected clock in the runtime crate).
fn endpoint_exposure() -> String {
    let camera_sha = ManifestFingerprint::of_bytes(CAMERA_CONTRACT.as_bytes()).to_string();
    format!(
        r#"{{
        peppy_schema: "mcp_exposure/v1",
        manifest: {{ name: "camera_endpoint", tag: "v1" }},
        server: {{
            title: "OpenArm camera",
            instructions: "Observe and control the front camera on this robot.",
        }},
        targets: {{
            front_camera: {{
                contract: {{ name: "rgb_camera", tag: "v1", sha256: "{camera_sha}" }},
                topics: [
                    {{
                        member: "video_stream",
                        resource: "front_camera.latest_frame",
                        description: "Latest frame from the front-facing camera, JPEG encoded.",
                        freshness: {{ max_age_ms: 600000 }},
                        update: {{ max_hz: 100 }},
                        representation: {{
                            image: "jpeg",
                            quality: 80,
                            fields: {{
                                data: "frame",
                                encoding: "encoding",
                                width: "width",
                                height: "height",
                            }},
                        }},
                        max_result_bytes: 524288,
                        on_oversize: "downscale",
                    }},
                    {{
                        member: "camera_status",
                        resource: "front_camera.status",
                        description: "Latest camera status snapshot.",
                        freshness: {{ max_age_ms: 600000 }},
                        update: {{ max_hz: 100 }},
                        max_result_bytes: 8192,
                        on_oversize: "reject",
                    }},
                ],
                services: [
                    {{
                        member: "video_stream_info",
                        tool: "front_camera.info",
                        description: "Report the camera's resolution, frame rate, and encoding.",
                        operation: "read_only",
                        deadline_ms: 5000,
                    }},
                    {{
                        member: "set_brightness",
                        tool: "front_camera.set_brightness",
                        description: "Set the camera brightness in device units.",
                        operation: "mutating",
                        deadline_ms: 5000,
                        restrict: {{ value: {{ min: -64, max: 64 }} }},
                    }},
                    {{
                        member: "freeze_probe",
                        tool: "front_camera.freeze_probe",
                        description: "Report the frame-freeze detector state.",
                        operation: "read_only",
                        deadline_ms: 1500,
                    }},
                ],
                actions: [
                    {{
                        member: "record_clip",
                        tool: "front_camera.record_clip",
                        description: "Record a short clip to local storage. Long-running; returns a task handle.",
                        operation: "long_running",
                        safety_sensitive: true,
                        confirmation_required: true,
                        deadline_ms: 600000,
                    }},
                ],
            }},
        }},
    }}"#
    )
}

/// Writes the generated MCP server node into `node_dir` and returns the
/// formats needed to build its peppygen.
fn emit_mcp_node(node_dir: &Path) {
    let exposure =
        PeppyMcpExposureParser::from_content(&endpoint_exposure()).expect("exposure parses");
    let contracts = vec![ResolvedContractDocument {
        sha256: ManifestFingerprint::of_bytes(CAMERA_CONTRACT.as_bytes()),
        document: PeppyContractParser::from_content(CAMERA_CONTRACT).expect("contract parses"),
    }];
    let node = generate_exposure_node(&exposure, &contracts).expect("the exposure generates");
    assert_eq!(node.node_dir_name, "camera_endpoint_mcp");
    for file in &node.files {
        let path = node_dir.join(&file.path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create node subdirectory");
        }
        fs::write(&path, &file.content).expect("write generated node file");
    }
}

/// The consumed interfaces the daemon would resolve for the node's contract
/// slot: derived from the same bundle the node was emitted from, so the
/// tests feed peppygen exactly what the generated manifest consumes.
fn node_consumed_interfaces() -> Vec<DeploymentInterface> {
    let exposure =
        PeppyMcpExposureParser::from_content(&endpoint_exposure()).expect("exposure parses");
    let contract = PeppyContractParser::from_content(CAMERA_CONTRACT).expect("contract parses");
    let bundle = generator::build_exposure_bundle(
        &exposure,
        &[ResolvedContractDocument {
            sha256: ManifestFingerprint::of_bytes(CAMERA_CONTRACT.as_bytes()),
            document: PeppyContractParser::from_content(CAMERA_CONTRACT).expect("contract parses"),
        }],
    )
    .expect("the exposure validates");
    mcp_test_support::consumed_interfaces(&bundle, &[(&contract, "front_camera")])
}

/// Compiles the emitted node. `compile_project` serves the test-wrapper
/// crates; the MCP node keeps its own manifest, so the shared harness
/// mirrors the same steps: unique peppygen package name (aliased back so
/// generated `use peppygen::…` code is unchanged), offline build in the
/// shared target dir, binary copied where `spawn_cargo_run` looks.
fn compile_mcp_node(node_dir: &Path) {
    compile_node(node_dir, "camera_endpoint_mcp", "user_node");
}

/// Waits until the endpoint accepts TCP connections, panicking with the
/// child's output if the node exits first.
fn wait_for_endpoint_or_exit(port: u16, child: &mut std::process::Child, dir: &Path) {
    let deadline = std::time::Instant::now() + DEFAULT_WAIT_TIMEOUT;
    loop {
        if std::net::TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            Duration::from_millis(200),
        )
        .is_ok()
        {
            return;
        }
        if let Some(status) = child.try_wait().expect("poll the node process") {
            let mut stdout = Vec::new();
            if let Some(mut out) = child.stdout.take() {
                use std::io::Read as _;
                let _ = out.read_to_end(&mut stdout);
            }
            let mut stderr = Vec::new();
            if let Some(mut err) = child.stderr.take() {
                use std::io::Read as _;
                let _ = err.read_to_end(&mut stderr);
            }
            panic!(
                "the MCP node at {} exited ({status}) before serving\nstdout:\n{}\nstderr:\n{}",
                dir.display(),
                String::from_utf8_lossy(&stdout),
                String::from_utf8_lossy(&stderr),
            );
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the MCP endpoint did not accept connections within {DEFAULT_WAIT_TIMEOUT:?}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// [`mcp_test_support::poll_task_until`] bounded by this file's
/// [`DEFAULT_WAIT_TIMEOUT`].
async fn poll_task_until(
    client: &Client,
    task_id: &str,
    description: &str,
    accept: impl Fn(&DetailedTask) -> bool,
) -> DetailedTask {
    mcp_test_support::poll_task_until(client, DEFAULT_WAIT_TIMEOUT, task_id, description, accept)
        .await
}

/// Fires `record_clip` as a task and walks the confirmation gate: the task
/// parks in `input_required` with the confirmation elicitation, and the
/// accept delivered via `tasks/update` releases the goal.
async fn start_confirmed_record_clip(client: &Client, duration_frames: u32) -> String {
    let response = client
        .call_tool_once(
            CallToolRequestParams::new("front_camera.record_clip")
                .with_arguments(object(json!({ "duration_frames": duration_frames }))),
        )
        .await
        .expect("the task-backed tool answers");
    let CallToolResponse::Task(created) = response else {
        panic!("expected a task handle, got {response:?}");
    };
    assert_eq!(created.task.status, TaskStatus::Working);
    // The exposure's `deadline_ms` plus the runtime's one-second TTL grace:
    // the task manager's TTL sweep is a hard stop reporting a generic
    // expiry, so it must land after the deadline the bridge enforces rather
    // than race it.
    assert_eq!(
        created.task.ttl_ms,
        Some(600000 + 1000),
        "the advertised TTL is the whole-goal deadline plus the runtime's grace"
    );
    let task_id = created.task.task_id;

    let parked = poll_task_until(client, &task_id, "input_required", |task| {
        task.status() == TaskStatus::InputRequired
    })
    .await;
    let rmcp::model::TaskPayload::InputRequired { input_requests } = parked.payload else {
        panic!("expected input_required, got {:?}", parked.payload);
    };
    assert!(
        input_requests.contains_key("confirmation"),
        "the confirmation elicitation is outstanding"
    );
    client
        .update_task(confirmation_accept(&task_id))
        .await
        .expect("the confirmation is delivered");
    task_id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_endpoint_serves_the_exposure_end_to_end() {
    let instance = pmi::ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (router_host, router_port) = (instance.host.clone(), instance.port);

    // --- The MCP server node: emitted from the exposure, peppygen built by
    // the real generator entry point.
    let temp_dir_mcp =
        TempDir::new_in(crate::helpers::test_tmp_root()).expect("temp dir for the MCP node");
    let node_dir = temp_dir_mcp.path().join("camera_endpoint_mcp");
    fs::create_dir_all(&node_dir).expect("create node dir");
    emit_mcp_node(&node_dir);
    generate_peppygen_lib(
        config::node::PeppygenLanguage::Rust,
        &node_dir,
        node_consumed_interfaces(),
        "test-git-hash",
        &test_peppy_dirs(),
        Default::default(),
        None,
    )
    .expect("peppygen generates for the emitted node");

    let http_port = ephemeral_port();
    let mcp_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstanceConfig {
            arguments: serde_json5::from_str(&format!("{{ port: {http_port} }}"))
                .expect("port argument parses"),
            ..NodeInstanceConfig::new(Name::new(MCP_INSTANCE_ID).expect("valid instance id"))
        },
        "camera_endpoint_mcp",
        "v1",
        TEST_CORE_NODE,
    )
    .expect("runtime config builds");
    let mcp_runtime_config = bind_slot(
        mcp_runtime_config,
        "front_camera",
        TEST_CORE_NODE,
        STUB_INSTANCE_ID,
    );
    let mcp_runtime_config_path = temp_dir_mcp.path().join("peppy_runtime.json5");
    mcp_runtime_config
        .save_json5_launch_config(&mcp_runtime_config_path)
        .expect("write runtime config");

    // --- The stub provider: a generated node implementing the contract,
    // publishing frames and status and answering both services.
    let temp_dir_stub =
        TempDir::new_in(crate::helpers::test_tmp_root()).expect("temp dir for the stub");
    let (mut stub_generator, output_dir_stub, user_node_stub, _stub_config_path) =
        init_test_env::<generator::RustGenerator>(&temp_dir_stub, STUB_NODE_CONFIG);
    let origin = ContractOrigin {
        link_id: "cam_impl".to_string(),
        contract_name: "rgb_camera".to_string(),
        contract_tag: "v1".to_string(),
    };
    let contract = PeppyContractParser::from_content(CAMERA_CONTRACT).expect("contract parses");
    register_contract_members(&mut stub_generator, &contract, &origin);
    let output_config = copy_config_to_output(&user_node_stub, &output_dir_stub);
    stub_generator
        .build(&output_dir_stub, &test_peppy_dirs(), Default::default())
        .expect("build stub peppygen");
    fs::remove_file(output_config).expect("remove staged config");
    config::fingerprint::create_codegen_fingerprint(
        &user_node_stub.join(config::consts::NODE_CONFIG_FILE),
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let stub_runtime_config = RuntimeConfig::new(
        &router_host,
        router_port,
        NodeInstanceConfig::new(Name::new(STUB_INSTANCE_ID).expect("valid instance id")),
        STUB_NODE_NAME,
        "v1",
        TEST_CORE_NODE,
    )
    .expect("stub runtime config builds");
    let stub_runtime_config_path = temp_dir_stub.path().join("peppy_runtime.json5");
    stub_runtime_config
        .save_json5_launch_config(&stub_runtime_config_path)
        .expect("write stub runtime config");

    init_cargo_user_node(&user_node_stub);
    // The stub deliberately never answers `freeze_probe`, so its exposed
    // tool exercises the deadline path. `record_clip` runs short goals to
    // completion and parks long goals on the cancel signal, republishing
    // their first feedback so the MCP side observes progress regardless of
    // sensor-data QoS drops.
    let stub_main = r#"
use peppygen::emitted_topics::cam_impl::{camera_status, video_stream};
use peppygen::exposed_actions::cam_impl::record_clip;
use peppygen::exposed_services::cam_impl::{set_brightness, video_stream_info};
use peppygen::{NodeBuilder, Result};
use std::time::Duration;

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        let runner = node_runner.clone();
        tokio::spawn(async move {
            let mut action = record_clip::ActionHandle::expose(&runner)
                .await
                .expect("expose record_clip");
            loop {
                let maybe_ctx = action
                    .handle_goal_next_request(|_request| -> Result<record_clip::GoalDecision> {
                        Ok(record_clip::GoalDecision::accept())
                    })
                    .await;
                match maybe_ctx {
                    Ok(Some(ctx)) => {
                        let duration = ctx.request().data.duration_frames;
                        if duration >= 1000 {
                            loop {
                                let _ = ctx.publish_feedback(1).await;
                                tokio::select! {
                                    _ = ctx.cancel_signal() => break,
                                    _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                                }
                            }
                            ctx.complete_cancelled(1).await.expect("complete cancelled");
                        } else {
                            for frame in 1..=duration {
                                ctx.publish_feedback(frame).await.expect("publish feedback");
                            }
                            ctx.complete(duration).await.expect("complete");
                        }
                    }
                    _ => break,
                }
            }
        });
        let runner = node_runner.clone();
        tokio::spawn(async move {
            let publisher = video_stream::declare_publisher(&runner)
                .await
                .expect("declare video_stream publisher");
            let frame: Vec<u8> = (0..8u32 * 8 * 3).map(|i| (i % 251) as u8).collect();
            loop {
                let payload = video_stream::build_message(
                    frame.clone(),
                    "rgb8".to_owned(),
                    8,
                    8,
                )
                .expect("build video_stream message");
                publisher.publish(payload).await.expect("publish frame");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        let runner = node_runner.clone();
        tokio::spawn(async move {
            let publisher = camera_status::declare_publisher(&runner)
                .await
                .expect("declare camera_status publisher");
            loop {
                let payload = camera_status::build_message(
                    87,
                    "operational".to_owned(),
                    true,
                )
                .expect("build camera_status message");
                publisher.publish(payload).await.expect("publish status");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        let runner = node_runner.clone();
        tokio::spawn(async move {
            loop {
                video_stream_info::handle_next_request(&runner, |_request| {
                    Ok(video_stream_info::Response::new(
                        640,
                        480,
                        30.0,
                        "/dev/video0".to_owned(),
                    ))
                })
                .await
                .expect("handle video_stream_info");
            }
        });
        let runner = node_runner.clone();
        tokio::spawn(async move {
            loop {
                set_brightness::handle_next_request(&runner, |request| {
                    Ok(set_brightness::Response::new(request.data.value >= 0))
                })
                .await
                .expect("handle set_brightness");
            }
        });
        Ok(())
    })
}
"#;
    fs::write(user_node_stub.join("src").join("main.rs"), stub_main).expect("write stub main");

    compile_mcp_node(&node_dir);
    compile_project(&user_node_stub);

    // --- Boot the MCP node first: before any provider publishes, reads
    // must report the resources unavailable.
    let mcp_config_str = mcp_runtime_config_path
        .to_str()
        .expect("utf-8 path")
        .to_owned();
    let mut mcp_child = spawn_cargo_run(&node_dir, &[(RUNTIME_CONFIG_VAR_NAME, &mcp_config_str)]);
    wait_for_endpoint_or_exit(http_port, &mut mcp_child, &node_dir);

    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("http://127.0.0.1:{http_port}/mcp")),
    );
    let client = ClientInfo::default()
        .serve_with_lifecycle(
            transport,
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await
        .expect("the MCP client negotiates 2026-07-28");

    // Discovery on the real generated server advertises the published
    // identity: the supported revision, the catalog caching hints, and the
    // exposure's server block.
    let discovered = client
        .discover(RequestMetaObject(Default::default()))
        .await
        .expect("server/discover answers");
    assert_eq!(
        discovered.supported_versions,
        vec![ProtocolVersion::V_2026_07_28]
    );
    assert_eq!(discovered.ttl_ms, 3_600_000);
    assert_eq!(discovered.cache_scope, CacheScope::Private);
    assert_eq!(
        discovered.instructions.as_deref(),
        Some("Observe and control the front camera on this robot.")
    );
    let implementation = discovered
        .server_info()
        .expect("the server identity rides in the result _meta");
    assert_eq!(implementation.name, "camera_endpoint_mcp");
    assert_eq!(implementation.version, "v1");
    assert_eq!(implementation.title.as_deref(), Some("OpenArm camera"));

    // The catalog matches the published surface and carries caching hints.
    let tools = client.list_tools(None).await.expect("tools/list answers");
    let mut tool_names: Vec<_> = tools.tools.iter().map(|tool| tool.name.as_ref()).collect();
    tool_names.sort_unstable();
    assert_eq!(
        tool_names,
        [
            "front_camera.freeze_probe",
            "front_camera.info",
            "front_camera.record_clip",
            "front_camera.set_brightness"
        ]
    );
    let record_tool = tools
        .tools
        .iter()
        .find(|tool| tool.name.as_ref() == "front_camera.record_clip")
        .expect("the action tool is listed");
    assert_eq!(
        record_tool
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.destructive_hint),
        Some(true),
        "safety_sensitive surfaces as the destructive hint"
    );
    assert_eq!(tools.cache_scope, Some(CacheScope::Private));
    assert_eq!(tools.ttl_ms, Some(3_600_000));
    let resources = client
        .list_resources(None)
        .await
        .expect("resources/list answers");
    let mut resource_uris: Vec<_> = resources
        .resources
        .iter()
        .map(|resource| resource.uri.as_str())
        .collect();
    resource_uris.sort_unstable();
    assert_eq!(resource_uris, [FRAME_URI, STATUS_URI]);
    assert_eq!(resources.cache_scope, Some(CacheScope::Private));
    assert_eq!(resources.ttl_ms, Some(3_600_000));

    // Nothing has been published yet.
    let error = protocol_error(
        client
            .read_resource(ReadResourceRequestParams::new(STATUS_URI))
            .await
            .expect_err("no provider is running yet"),
    );
    assert_eq!(error.code, ErrorCode::INTERNAL_ERROR);
    assert!(
        error.message.contains("unavailable"),
        "got {}",
        error.message
    );

    // A name outside the exposure is rejected without touching the graph.
    let error = protocol_error(
        client
            .read_resource(ReadResourceRequestParams::new("peppy://resource/absent"))
            .await
            .expect_err("absent resources are refused"),
    );
    assert_eq!(error.code, ErrorCode::INVALID_PARAMS);

    // Subscribe before the provider exists; the notification proves the
    // publish-to-notify path once the stub starts.
    let mut subscription = client
        .listen(
            SubscriptionFilter::builder()
                .resource_subscription(STATUS_URI)
                .build(),
        )
        .await
        .expect("subscriptions/listen is accepted");

    // --- Start the stub provider.
    let stub_config_str = stub_runtime_config_path
        .to_str()
        .expect("utf-8 path")
        .to_owned();
    let mut stub_child = spawn_cargo_run(
        &user_node_stub,
        &[(RUNTIME_CONFIG_VAR_NAME, &stub_config_str)],
    );

    let control_messenger = peppylib::MessengerHandle::connect(&router_host, router_port)
        .await
        .expect("control messenger connects");
    let ctx = WaitContext {
        messenger: &control_messenger,
        bound_core_node: TEST_CORE_NODE,
        caller_instance_id: SHUTDOWN_SENDER_INSTANCE_ID,
        target_core_node: TEST_CORE_NODE,
    };
    wait_for_health_service_reachable_or_exit(
        &ctx,
        STUB_NODE_NAME,
        STUB_INSTANCE_ID,
        &mut stub_child,
        &user_node_stub,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;

    // The subscription delivers a resource-updated notification for the
    // subscribed URI once a publish passes the policies.
    let notification = tokio::time::timeout(DEFAULT_WAIT_TIMEOUT, subscription.next())
        .await
        .expect("a notification arrives before the guard timeout")
        .expect("the subscription stream is healthy")
        .expect("the stream did not end");
    match notification {
        ServerNotification::ResourceUpdatedNotification(updated) => {
            assert_eq!(updated.params.uri, STATUS_URI);
        }
        other => panic!("expected a resource-updated notification, got {other:?}"),
    }
    subscription.cancel().await.expect("subscription cancels");

    // The status snapshot serves the canonical JSON of the published message.
    let read = client
        .read_resource(ReadResourceRequestParams::new(STATUS_URI))
        .await
        .expect("status snapshot serves");
    assert_eq!(read.cache_scope, Some(CacheScope::Private));
    assert!(read.ttl_ms.is_some());
    let rmcp::model::ResourceContents::TextResourceContents {
        text, mime_type, ..
    } = read.contents.first().expect("one content item")
    else {
        panic!("expected text contents");
    };
    assert_eq!(mime_type.as_deref(), Some("application/json"));
    let snapshot: Value = serde_json::from_str(text).expect("snapshot is JSON");
    assert_eq!(
        snapshot,
        json!({ "battery": 87, "note": "operational", "recording": true })
    );

    // The frame resource applies the JPEG default representation: the raw
    // rgb8 frame the stub publishes serves as an mjpeg-labelled JPEG.
    let read = client
        .read_resource(ReadResourceRequestParams::new(FRAME_URI))
        .await
        .expect("frame snapshot serves");
    let rmcp::model::ResourceContents::TextResourceContents { text, .. } =
        read.contents.first().expect("one content item")
    else {
        panic!("expected text contents");
    };
    let snapshot: Value = serde_json::from_str(text).expect("frame snapshot is JSON");
    assert_eq!(snapshot["encoding"], "mjpeg");
    assert_eq!(snapshot["width"], 8);
    assert_eq!(snapshot["height"], 8);
    let jpeg = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        snapshot["frame"].as_str().expect("frame is base64"),
    )
    .expect("frame decodes as base64");
    assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "the served frame is a JPEG");

    // Tool calls round-trip structured output through the typed clients.
    let called = client
        .call_tool(CallToolRequestParams::new("front_camera.info"))
        .await
        .expect("read-only tool answers");
    assert_ne!(called.is_error, Some(true), "got {:?}", called.content);
    assert_eq!(
        called.structured_content,
        Some(json!({ "width": 640, "height": 480, "fps": 30.0, "device": "/dev/video0" }))
    );

    let called = client
        .call_tool(
            CallToolRequestParams::new("front_camera.set_brightness")
                .with_arguments(object(json!({ "value": 12 }))),
        )
        .await
        .expect("mutating tool answers");
    assert_eq!(called.structured_content, Some(json!({ "applied": true })));
    let called = client
        .call_tool(
            CallToolRequestParams::new("front_camera.set_brightness")
                .with_arguments(object(json!({ "value": -12 }))),
        )
        .await
        .expect("mutating tool answers");
    assert_eq!(called.structured_content, Some(json!({ "applied": false })));

    // A value outside the reflected restrict bounds and an unknown tool are
    // both rejected before the Peppy graph sees them.
    let error = protocol_error(
        client
            .call_tool(
                CallToolRequestParams::new("front_camera.set_brightness")
                    .with_arguments(object(json!({ "value": 65 }))),
            )
            .await
            .expect_err("65 is outside the restrict bounds"),
    );
    assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
    let error = protocol_error(
        client
            .call_tool(CallToolRequestParams::new("front_camera.set_gain"))
            .await
            .expect_err("set_gain is not exposed"),
    );
    assert_eq!(error.code, ErrorCode::INVALID_PARAMS);

    // The stub never answers `freeze_probe`, so its 1500 ms deadline turns
    // the call into a readable tool error instead of a hang.
    let called = client
        .call_tool(CallToolRequestParams::new("front_camera.freeze_probe"))
        .await
        .expect("a deadline miss is a tool error, not a protocol error");
    assert_eq!(called.is_error, Some(true), "got {:?}", called.content);

    // Without the tasks extension capability, the action tool refuses the
    // call before any task (or Peppy goal) exists.
    let error = protocol_error(
        client
            .call_tool_once(
                CallToolRequestParams::new("front_camera.record_clip")
                    .with_arguments(object(json!({ "duration_frames": 3 }))),
            )
            .await
            .expect_err("the tasks capability is required"),
    );
    assert_eq!(error.code, ErrorCode::MISSING_REQUIRED_CLIENT_CAPABILITY);

    client.cancel().await.expect("client disconnects");

    // --- The action-backed task walk, on a tasks-capable client.
    let tasks_client = connect_with_tasks(http_port).await;

    // A goal failing the derived schema never materializes a task.
    let error = protocol_error(
        tasks_client
            .call_tool_once(
                CallToolRequestParams::new("front_camera.record_clip")
                    .with_arguments(object(json!({ "duration_frames": "three" }))),
            )
            .await
            .expect_err("a non-integer duration is rejected"),
    );
    assert_eq!(error.code, ErrorCode::INVALID_PARAMS);

    // A task handle that was never created is refused the same way.
    let error = protocol_error(
        tasks_client
            .get_task(GetTaskParams::new("never-created"))
            .await
            .expect_err("the handle does not exist"),
    );
    assert_eq!(error.code, ErrorCode::INVALID_PARAMS);

    // Completion: confirm, let the stub record three frames, and read the
    // structured result off the completed task.
    let task_id = start_confirmed_record_clip(&tasks_client, 3).await;
    let completed = poll_task_until(&tasks_client, &task_id, "a terminal status", |task| {
        task.status().is_terminal()
    })
    .await;
    assert_eq!(completed.status(), TaskStatus::Completed);
    let rmcp::model::TaskPayload::Completed { result } = completed.payload else {
        panic!("expected a completed payload");
    };
    assert_eq!(result["structuredContent"], json!({ "frames_written": 3 }));

    // Cancellation: a long goal parks on the provider; its feedback drives
    // the observable status message, and `tasks/cancel` forwards to the
    // Peppy cancel path, whose cancelled result settles the task.
    let task_id = start_confirmed_record_clip(&tasks_client, 100000).await;
    poll_task_until(
        &tasks_client,
        &task_id,
        "feedback-driven progress",
        |task| {
            task.task
                .status_message
                .as_deref()
                .is_some_and(|message| message.contains("frame"))
        },
    )
    .await;
    tasks_client
        .cancel_task(CancelTaskParams::new(&*task_id))
        .await
        .expect("tasks/cancel acknowledges");
    let cancelled = poll_task_until(&tasks_client, &task_id, "a terminal status", |task| {
        task.status().is_terminal()
    })
    .await;
    assert_eq!(cancelled.status(), TaskStatus::Cancelled);

    // Reconnect-and-resume: drop the client mid-goal and keep driving the
    // same task handle from a fresh connection.
    let task_id = start_confirmed_record_clip(&tasks_client, 100000).await;
    poll_task_until(
        &tasks_client,
        &task_id,
        "feedback-driven progress",
        |task| task.task.status_message.is_some(),
    )
    .await;
    tasks_client
        .cancel()
        .await
        .expect("client disconnects mid-task");
    let reconnected = connect_with_tasks(http_port).await;
    reconnected
        .cancel_task(CancelTaskParams::new(&*task_id))
        .await
        .expect("the reconnected client cancels the same handle");
    let cancelled = poll_task_until(&reconnected, &task_id, "a terminal status", |task| {
        task.status().is_terminal()
    })
    .await;
    assert_eq!(cancelled.status(), TaskStatus::Cancelled);
    reconnected.cancel().await.expect("client disconnects");

    // --- Teardown: cooperative shutdown through the framework service.
    send_shutdown(
        &control_messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        "camera_endpoint_mcp",
        TEST_CORE_NODE,
        MCP_INSTANCE_ID,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;
    send_shutdown(
        &control_messenger,
        TEST_CORE_NODE,
        SHUTDOWN_SENDER_INSTANCE_ID,
        STUB_NODE_NAME,
        TEST_CORE_NODE,
        STUB_INSTANCE_ID,
        DEFAULT_WAIT_TIMEOUT,
    )
    .await;
    let mcp_output = wait_for_child(&mut mcp_child, Some(Duration::from_secs(10)), &node_dir);
    let stub_output = wait_for_child(
        &mut stub_child,
        Some(Duration::from_secs(10)),
        &user_node_stub,
    );
    assert!(
        mcp_output.status.success(),
        "the MCP node exits cleanly:\n{}",
        String::from_utf8_lossy(&mcp_output.stderr)
    );
    assert!(
        stub_output.status.success(),
        "the stub exits cleanly:\n{}",
        String::from_utf8_lossy(&stub_output.stderr)
    );
}
