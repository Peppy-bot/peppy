//! Full-stack end-to-end for the MCP exposure design: launch to serve.
//!
//! A real launcher deploys a mock UVC camera, a recorder implementing the
//! `episode_recording:v1` fixture contract, and the MCP server node that
//! `peppy repo exposure` publication generated, all on a real in-process
//! daemon with a live zenoh router. A real MCP `2026-07-28` client on the
//! same machine then reads the frame resource, calls a tool, and completes
//! one action task through the whole pipeline: repository seeding,
//! exposure publication, pinned resolution, launcher links, and the
//! generated bridges over the router.
//!
//! Because the running stack carries real capabilities the exposure did not
//! select (`finish_session` stays native), this test also backs the design's
//! security criterion: the messaging layer is never reachable through the
//! MCP endpoint. Unselected members are refused by name, and the HTTP
//! server answers nothing outside the MCP path.
//!
//! The three node crates are compiled out-of-band before the launch, per
//! the repo's compiled-node fixture precedent (`core-node-internal`'s
//! `fixtures.rs`): the daemon's copy excludes `target/`, so a daemon-driven
//! build would be a cold release build per run, and building in the shared
//! test target dir requires the per-run-unique `peppygen` package rename
//! that only an out-of-band build can apply. The staged manifests drop
//! `build_cmd` and run the pre-built binaries by absolute path; the ADD
//! phase still resolves every contract slot and regenerates peppygen in its
//! working copy, so pinned resolution through the daemon stays covered.

use peppy::commands::Command;
use peppy::commands::node::{NodeCommand, NodeCommands};
use peppy::commands::stack::{StackCommand, StackCommands};
use peppy::context::AppContext;
use peppy::test_support::ServeCommandEmulation;

use config::consts::{NODE_CONFIG_FILE, PEPPYGEN_OUTPUT_PATH};
use daemon_config::consts::PeppyDirs;
use daemon_config::contract::PeppyContractParser;
use daemon_config::mcp_exposure::PeppyMcpExposureParser;
use daemon_config::repository::ManifestFingerprint;
use generator::{
    ContractOrigin, DeploymentInterface, LanguageGenerator, ResolvedContractDocument,
    generate_peppygen_lib,
};
use mcp_test_support::{
    compile_node, confirmation_accept, connect_with_tasks, consumed_interfaces, ephemeral_port,
    poll_task_until, protocol_error, register_contract_members,
};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, ErrorCode, ReadResourceRequestParams, TaskStatus,
    object,
};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Bound for waits that are already response-driven (launch readiness is
/// blocking; everything after polls real endpoints).
const WAIT: Duration = Duration::from_secs(120);

const FRAME_URI: &str = "peppy://resource/front_camera.latest_frame";

/// The camera role contract, in the fixture shape the generator's own
/// endpoint e2e uses (no header, so the mock's publish loop stays small).
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
                    width: "u32",
                    height: "u32",
                },
            },
        ],
        services: [
            {
                name: "video_stream_info",
                response_message_format: {
                    width: "u32",
                    height: "u32",
                    frames_per_second: "u8",
                    encoding: "string",
                },
            },
        ],
    },
}"#;

/// The recording contract. `finish_session` deliberately stays out of the
/// exposure below: a running native member the MCP catalog must not reach.
const RECORDING_CONTRACT: &str = r#"{
    peppy_schema: "contract/v1",
    manifest: { name: "episode_recording", tag: "v1" },
    interfaces: {
        actions: [
            {
                name: "record_episode",
                goal_service: {
                    request_message_format: { episode_name: "string" },
                },
                feedback_topic: {
                    message_format: { frame: "u32" },
                },
                result_service: {
                    response_message_format: { frames: "u32" },
                },
            },
        ],
        services: [
            {
                name: "finish_session",
                response_message_format: { episodes_recorded: "u32" },
            },
        ],
    },
}"#;

const CAMERA_NODE_CONFIG: &str = r#"{
    peppy_schema: "node/v1",
    manifest: {
        name: "mock_uvc_camera",
        tag: "v1",
        implements: [
            { name: "rgb_camera", tag: "v1", link_id: "camera" },
        ],
    },
    execution: {
        language: "rust",
        build_cmd: ["cargo", "build", "--release"],
        run_cmd: ["./target/release/mock_uvc_camera"],
    },
    interfaces: {
        topics: {
            emits: [
                { link_id: "camera", name: "video_stream" },
            ],
        },
        services: {
            exposes: [
                { link_id: "camera", name: "video_stream_info" },
            ],
        },
    },
}"#;

const CAMERA_MAIN: &str = r#"
use peppygen::emitted_topics::camera::video_stream;
use peppygen::exposed_services::camera::video_stream_info;
use peppygen::{NodeBuilder, Result};
use std::time::Duration;

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        let runner = node_runner.clone();
        tokio::spawn(async move {
            let publisher = video_stream::declare_publisher(&runner)
                .await
                .expect("declare video_stream publisher");
            let frame: Vec<u8> = (0..16u32 * 16 * 3).map(|i| (i % 251) as u8).collect();
            loop {
                let payload =
                    video_stream::build_message(frame.clone(), "rgb8".to_owned(), 16, 16)
                        .expect("build video_stream message");
                publisher.publish(payload).await.expect("publish frame");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        });
        let runner = node_runner.clone();
        tokio::spawn(async move {
            loop {
                video_stream_info::handle_next_request(&runner, |_request| {
                    Ok(video_stream_info::Response::new(16, 16, 5, "rgb8".to_owned()))
                })
                .await
                .expect("handle video_stream_info");
            }
        });
        Ok(())
    })
}
"#;

const RECORDER_NODE_CONFIG: &str = r#"{
    peppy_schema: "node/v1",
    manifest: {
        name: "mock_recorder",
        tag: "v1",
        implements: [
            { name: "episode_recording", tag: "v1", link_id: "recording" },
        ],
    },
    execution: {
        language: "rust",
        build_cmd: ["cargo", "build", "--release"],
        run_cmd: ["./target/release/mock_recorder"],
    },
    interfaces: {
        actions: {
            exposes: [
                { link_id: "recording", name: "record_episode" },
            ],
        },
        services: {
            exposes: [
                { link_id: "recording", name: "finish_session" },
            ],
        },
    },
}"#;

const RECORDER_MAIN: &str = r#"
use peppygen::exposed_actions::recording::record_episode;
use peppygen::exposed_services::recording::finish_session;
use peppygen::{NodeBuilder, Result};

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        let runner = node_runner.clone();
        tokio::spawn(async move {
            let mut action = record_episode::ActionHandle::expose(&runner)
                .await
                .expect("expose record_episode");
            loop {
                let maybe_ctx = action
                    .handle_goal_next_request(|_request| -> Result<record_episode::GoalDecision> {
                        Ok(record_episode::GoalDecision::accept())
                    })
                    .await;
                match maybe_ctx {
                    Ok(Some(ctx)) => {
                        for frame in 1..=5u32 {
                            ctx.publish_feedback(frame).await.expect("publish feedback");
                        }
                        ctx.complete(5).await.expect("complete record_episode");
                    }
                    _ => break,
                }
            }
        });
        let runner = node_runner.clone();
        tokio::spawn(async move {
            loop {
                finish_session::handle_next_request(&runner, |_request| {
                    Ok(finish_session::Response::new(1))
                })
                .await
                .expect("handle finish_session");
            }
        });
        Ok(())
    })
}
"#;

const PROVIDER_CARGO_DEPS: &str = r#"
[dependencies]
tokio = { version = "1", features = ["macros", "rt-multi-thread", "time"] }
peppygen = { path = ".peppy/libs/peppygen" }
"#;

/// The exposure under test: the design walk's surface. The frame resource,
/// one read-only tool, one confirmation-gated action task; everything else
/// on the running providers stays native.
fn stack_exposure() -> String {
    let camera_sha = ManifestFingerprint::of_bytes(CAMERA_CONTRACT.as_bytes()).to_string();
    let recording_sha = ManifestFingerprint::of_bytes(RECORDING_CONTRACT.as_bytes()).to_string();
    format!(
        r#"{{
        peppy_schema: "mcp_exposure/v1",
        manifest: {{ name: "camera_and_recording", tag: "v1" }},
        server: {{
            title: "OpenArm camera and recording",
            instructions: "Observe the front camera and record episodes on this robot.",
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
                ],
                services: [
                    {{
                        member: "video_stream_info",
                        tool: "front_camera.info",
                        description: "Report the camera's resolution, frame rate, and encoding.",
                        operation: "read_only",
                        deadline_ms: 5000,
                    }},
                ],
            }},
            recorder: {{
                contract: {{ name: "episode_recording", tag: "v1", sha256: "{recording_sha}" }},
                actions: [
                    {{
                        member: "record_episode",
                        tool: "recorder.record_episode",
                        description: "Record one teleoperation episode to the local dataset.",
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

/// The consumed interfaces the daemon resolves for the generated node's two
/// contract slots, restricted to the members the exposure selects (which is
/// exactly what the generated manifest consumes): derived from the same
/// bundle publication builds, not hand-listed.
fn mcp_consumed_interfaces() -> Vec<DeploymentInterface> {
    let camera = PeppyContractParser::from_content(CAMERA_CONTRACT).expect("contract parses");
    let recording = PeppyContractParser::from_content(RECORDING_CONTRACT).expect("contract parses");
    let exposure =
        PeppyMcpExposureParser::from_content(&stack_exposure()).expect("exposure parses");
    let bundle = generator::build_exposure_bundle(
        &exposure,
        &[
            ResolvedContractDocument {
                sha256: ManifestFingerprint::of_bytes(CAMERA_CONTRACT.as_bytes()),
                document: PeppyContractParser::from_content(CAMERA_CONTRACT)
                    .expect("contract parses"),
            },
            ResolvedContractDocument {
                sha256: ManifestFingerprint::of_bytes(RECORDING_CONTRACT.as_bytes()),
                document: PeppyContractParser::from_content(RECORDING_CONTRACT)
                    .expect("contract parses"),
            },
        ],
    )
    .expect("the exposure validates");
    consumed_interfaces(
        &bundle,
        &[(&camera, "front_camera"), (&recording, "recorder")],
    )
}

/// Writes one provider node crate into the hub and generates its peppygen
/// from the contract it implements, the way the daemon's sync would.
fn stage_provider(
    hub: &Path,
    node_name: &str,
    node_config: &str,
    main_rs: &str,
    contract_json5: &str,
    link_id: &str,
) -> PathBuf {
    let node_dir = hub.join("nodes").join(node_name);
    fs::create_dir_all(node_dir.join("src")).expect("create provider src dir");
    fs::write(node_dir.join(NODE_CONFIG_FILE), node_config).expect("write provider manifest");
    fs::write(node_dir.join("src").join("main.rs"), main_rs).expect("write provider main");
    fs::write(
        node_dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{node_name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n{PROVIDER_CARGO_DEPS}"
        ),
    )
    .expect("write provider Cargo.toml");

    let contract = PeppyContractParser::from_content(contract_json5).expect("contract parses");
    let origin = ContractOrigin {
        link_id: link_id.to_string(),
        contract_name: contract.manifest.name.to_string(),
        contract_tag: contract.manifest.tag.to_string(),
    };
    let mut generator = generator::RustGenerator::default();
    register_contract_members(&mut generator, &contract, &origin);
    let output_dir = node_dir.join(PEPPYGEN_OUTPUT_PATH);
    fs::create_dir_all(&output_dir).expect("create peppygen output dir");
    let staged_config = output_dir.join(NODE_CONFIG_FILE);
    fs::copy(node_dir.join(NODE_CONFIG_FILE), &staged_config).expect("stage provider config");
    generator
        .build(&output_dir, &PeppyDirs::default(), Default::default())
        .expect("build provider peppygen");
    fs::remove_file(staged_config).expect("remove staged config");
    node_dir
}

/// Rewrites a staged manifest for the pre-built binary: no build step, the
/// absolute binary path as `run_cmd`.
fn point_manifest_at_binary(node_dir: &Path, binary: &Path) {
    let manifest_path = node_dir.join(NODE_CONFIG_FILE);
    let source = fs::read_to_string(&manifest_path).expect("staged manifest exists");
    let mut node_config: config::node::NodeConfig =
        serde_json5::from_str(&source).expect("staged manifest parses");
    node_config.execution.build_cmd = None;
    node_config.execution.run_cmd = Some(vec![binary.to_str().expect("utf-8 path").to_string()]);
    fs::write(
        &manifest_path,
        serde_json5::to_string(&node_config).expect("staged manifest serializes"),
    )
    .expect("rewrite staged manifest");
}

/// The run logs of the three instances, for readable panics when a wait on
/// the running stack fails.
fn run_logs(peppy_root: &Path) -> String {
    ["the_camera", "episode_recorder_inst", "mcp_server"]
        .iter()
        .map(|instance| {
            let path = peppy_root.join("logs/run").join(format!("{instance}.log"));
            format!(
                "--- {instance} ---\n{}",
                fs::read_to_string(&path).unwrap_or_else(|_| "(no log)".to_string())
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_launcher_deploys_the_exposure_and_a_client_walks_it() {
    let serve = ServeCommandEmulation::with_zenoh()
        .await
        .expect("zenoh serve emulation starts");

    let nodes_dir = tempfile::tempdir().expect("temp nodes dir");
    let ctx = Arc::new(
        AppContext::with_messenger(nodes_dir.path(), Arc::clone(&serve.messenger()))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    // --- The hub: contracts, both provider crates, and the exposure.
    let hub_dir = tempfile::tempdir().expect("temp hub dir");
    let hub = hub_dir.path();
    fs::create_dir_all(hub.join("contracts")).expect("create contracts dir");
    fs::create_dir_all(hub.join("exposures")).expect("create exposures dir");
    fs::write(hub.join("contracts/rgb_camera.json5"), CAMERA_CONTRACT).expect("write contract");
    fs::write(
        hub.join("contracts/episode_recording.json5"),
        RECORDING_CONTRACT,
    )
    .expect("write contract");
    let exposure_path = hub.join("exposures/camera_and_recording.json5");
    fs::write(&exposure_path, stack_exposure()).expect("write exposure");
    let camera_dir = stage_provider(
        hub,
        "mock_uvc_camera",
        CAMERA_NODE_CONFIG,
        CAMERA_MAIN,
        CAMERA_CONTRACT,
        "camera",
    );
    let recorder_dir = stage_provider(
        hub,
        "mock_recorder",
        RECORDER_NODE_CONFIG,
        RECORDER_MAIN,
        RECORDING_CONTRACT,
        "recording",
    );

    // First refresh: the contract caches the publication resolves pins from.
    super::common::seed_docs_repo(&serve, &ctx, hub);

    // --- Publication: the same entry point `peppy repo exposure` runs,
    // against the daemon's caches. The node lands as a sibling of the
    // exposure document.
    let published = core_node::publish_exposure(
        &exposure_path,
        &PeppyDirs::new(serve.temp_dir()),
        &|_feedback| {},
    )
    .expect("the exposure publishes");
    let mcp_dir = hub.join("exposures/camera_and_recording_mcp");
    assert_eq!(published.node_dir, mcp_dir, "the node is published in-repo");

    // --- Out-of-band codegen and compilation of all three nodes.
    generate_peppygen_lib(
        config::node::PeppygenLanguage::Rust,
        &mcp_dir,
        mcp_consumed_interfaces(),
        "test-git-hash",
        &PeppyDirs::default(),
        Default::default(),
        None,
    )
    .expect("peppygen generates for the published node");

    let camera_binary = compile_node(&camera_dir, "mock_uvc_camera", "mock_uvc_camera");
    let recorder_binary = compile_node(&recorder_dir, "mock_recorder", "mock_recorder");
    let mcp_binary = compile_node(
        &mcp_dir,
        "camera_and_recording_mcp",
        "camera_and_recording_mcp",
    );
    point_manifest_at_binary(&camera_dir, &camera_binary);
    point_manifest_at_binary(&recorder_dir, &recorder_binary);
    point_manifest_at_binary(&mcp_dir, &mcp_binary);

    // Second refresh: the index now carries the generated node, and every
    // cache fingerprint is of the final staged bytes.
    super::common::seed_docs_repo(&serve, &ctx, hub);

    // --- The launcher, exactly as the design documents it: providers by
    // instance, the MCP server filling its logical targets through links.
    let http_port = ephemeral_port();
    let launcher_path = nodes_dir.path().join("peppy_launcher.json5");
    let launcher_json5 = format!(
        r#"{{
            peppy_schema: "launcher/v1",
            deployments: [
                {{
                    source: {{ name: "mock_uvc_camera:v1" }},
                    instances: [{{ instance_id: "the_camera" }}]
                }},
                {{
                    source: {{ name: "mock_recorder:v1" }},
                    instances: [{{ instance_id: "episode_recorder_inst" }}]
                }},
                {{
                    source: {{ name: "camera_and_recording_mcp:v1" }},
                    instances: [
                        {{
                            instance_id: "mcp_server",
                            arguments: {{ port: {http_port} }},
                            links: {{
                                front_camera: "the_camera",
                                recorder: "episode_recorder_inst",
                            }},
                        }}
                    ]
                }},
            ]
        }}"#
    );
    fs::write(&launcher_path, launcher_json5).expect("write launcher");

    StackCommand {
        command: StackCommands::Launch {
            place: Vec::new(),
            local: false,
            launcher_config_path: launcher_path,
            node_add_idle_timeout_secs: 120,
            node_build_idle_timeout_secs: 120,
            node_run_idle_timeout_secs: 120,
            max_timeout_secs: Some(900),
        },
    }
    .execute(&ctx)
    .unwrap_or_else(|error| panic!("launch failed: {error:?}\n{}", run_logs(serve.temp_dir())));

    // Launch blocks until every instance is ready and healthy; the endpoint
    // itself binds inside the node's run loop, so wait for the socket.
    tokio::time::timeout(WAIT, async {
        loop {
            if tokio::net::TcpStream::connect(("127.0.0.1", http_port))
                .await
                .is_ok()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "the MCP endpoint never accepted connections\n{}",
            run_logs(serve.temp_dir())
        )
    });

    let client = connect_with_tasks(http_port).await;

    // The catalog is exactly the exposure's selection. The running stack
    // has more (`finish_session` is live on the recorder); none of it is
    // visible here.
    let tools = client.list_tools(None).await.expect("tools/list answers");
    let mut tool_names: Vec<_> = tools.tools.iter().map(|tool| tool.name.as_ref()).collect();
    tool_names.sort_unstable();
    assert_eq!(tool_names, ["front_camera.info", "recorder.record_episode"]);
    let resources = client
        .list_resources(None)
        .await
        .expect("resources/list answers");
    let resource_uris: Vec<_> = resources
        .resources
        .iter()
        .map(|resource| resource.uri.as_str())
        .collect();
    assert_eq!(resource_uris, [FRAME_URI]);

    // The frame resource serves the camera's frames as JPEG once the pump
    // has seen a publish.
    let read = tokio::time::timeout(WAIT, async {
        loop {
            match client
                .read_resource(ReadResourceRequestParams::new(FRAME_URI))
                .await
            {
                Ok(read) => return read,
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "the frame resource never served\n{}",
            run_logs(serve.temp_dir())
        )
    });
    let rmcp::model::ResourceContents::TextResourceContents { text, .. } =
        read.contents.first().expect("one content item")
    else {
        panic!("expected text contents");
    };
    let snapshot: Value = serde_json::from_str(text).expect("snapshot is JSON");
    assert_eq!(snapshot["encoding"], "mjpeg");
    assert_eq!(snapshot["width"], 16);
    let jpeg = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        snapshot["frame"].as_str().expect("frame is base64"),
    )
    .expect("frame decodes");
    assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "JPEG magic bytes");

    // The tool call crosses the generated bridge to the real camera.
    let called = client
        .call_tool(CallToolRequestParams::new("front_camera.info"))
        .await
        .expect("the info tool answers");
    assert_ne!(called.is_error, Some(true));
    assert_eq!(
        called.structured_content,
        Some(json!({ "width": 16, "height": 16, "frames_per_second": 5, "encoding": "rgb8" }))
    );

    // One action task, launch to terminal: confirmation through
    // input_required, the recorder's five feedback frames, the structured
    // result off the completed task.
    let response = client
        .call_tool_once(
            CallToolRequestParams::new("recorder.record_episode")
                .with_arguments(object(json!({ "episode_name": "demo" }))),
        )
        .await
        .expect("the task-backed tool answers");
    let CallToolResponse::Task(created) = response else {
        panic!("expected a task handle, got {response:?}");
    };
    let task_id = created.task.task_id;
    poll_task_until(&client, WAIT, &task_id, "input_required", |task| {
        task.status() == TaskStatus::InputRequired
    })
    .await;
    client
        .update_task(confirmation_accept(&task_id))
        .await
        .expect("the confirmation is delivered");
    let completed = poll_task_until(&client, WAIT, &task_id, "a terminal status", |task| {
        task.status().is_terminal()
    })
    .await;
    assert_eq!(completed.status(), TaskStatus::Completed);
    let rmcp::model::TaskPayload::Completed { result } = completed.payload else {
        panic!("expected a completed payload");
    };
    assert_eq!(result["structuredContent"], json!({ "frames": 5 }));

    // --- The security criterion: the messaging layer is not reachable
    // through the endpoint. `finish_session` is running on the recorder
    // right now; the exposure did not select it, so its name does not
    // resolve, and neither does any unselected resource.
    let error = protocol_error(
        client
            .call_tool(CallToolRequestParams::new("recorder.finish_session"))
            .await
            .expect_err("an unselected native service is unreachable"),
    );
    assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
    let error = protocol_error(
        client
            .read_resource(ReadResourceRequestParams::new(
                "peppy://resource/recorder.session",
            ))
            .await
            .expect_err("an unselected resource name is unreachable"),
    );
    assert_eq!(error.code, ErrorCode::INVALID_PARAMS);

    // Outside the MCP path the HTTP server serves nothing at all.
    let mut raw = tokio::net::TcpStream::connect(("127.0.0.1", http_port))
        .await
        .expect("raw connect");
    raw.write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .await
        .expect("send raw request");
    let mut response = String::new();
    raw.read_to_string(&mut response)
        .await
        .expect("read raw response");
    assert!(
        response.starts_with("HTTP/1.1 404"),
        "expected 404 outside the MCP path, got: {}",
        response.lines().next().unwrap_or("")
    );

    client.cancel().await.expect("client disconnects");

    // --- Teardown in reverse dependency order.
    for instance_id in ["mcp_server", "episode_recorder_inst", "the_camera"] {
        let _ = NodeCommand {
            command: NodeCommands::Stop {
                instance_id: instance_id.to_string(),
            },
        }
        .execute(&ctx);
    }
}
