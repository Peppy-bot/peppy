//! Full-stack end-to-end for the built-in MCP server: launch to serve.
//!
//! A real launcher lists exposures; a real in-process daemon with a live
//! zenoh router resolves them, registers one `peppy mcp serve` process for
//! the list, and starts it beside the provider nodes it consumes. A real
//! MCP `2026-07-28` client then walks every endpoint over Streamable HTTP.
//!
//! The provider crates are compiled out-of-band before the launch, per the
//! repo's compiled-node fixture precedent: the daemon's copy excludes
//! `target/`, so a daemon-driven build would be a cold release build per
//! run. The staged manifests drop `build_cmd` and run the pre-built
//! binaries by absolute path; the ADD phase still resolves every contract
//! slot and regenerates peppygen in its working copy, so pinned resolution
//! through the daemon stays covered. The MCP server itself is never built:
//! it is the `peppy` binary under test, installed into the emulated Peppy
//! home's bin directory the way the installer places it.

use peppy::commands::Command;
use peppy::commands::mcp::mcp_catalog_rendered;
use peppy::commands::stack::{StackCommand, StackCommands, list_nodes_collecting};
use peppy::context::AppContext;
use peppy::test_support::ServeCommandEmulation;

use config::consts::{NODE_CONFIG_FILE, PEPPYGEN_OUTPUT_PATH};
use daemon_config::consts::PeppyDirs;
use daemon_config::contract::PeppyContractParser;
use daemon_config::mcp_deployment::SPEC_ENV_VAR;
use daemon_config::repository::ManifestFingerprint;
use generator::{ContractOrigin, LanguageGenerator};
use mcp_test_support::{
    compile_node, confirmation_accept, connect, connect_with_tasks, ephemeral_port,
    poll_task_until, protocol_error, register_contract_members,
};
use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, CancelTaskParams, ErrorCode,
    GetTaskParams, ProtocolVersion, ReadResourceRequestParams, RequestMetaObject,
    ServerNotification, SubscriptionFilter, TaskStatus, object,
};
use rmcp::service::Subscription;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Bound for waits that are already response-driven (launch readiness is
/// blocking; everything after polls real endpoints).
const WAIT: Duration = Duration::from_secs(120);

const STATUS_URI: &str = "peppy://resource/front_camera.status";
const FRAME_URI: &str = "peppy://resource/front_camera.latest_frame";

/// The camera contract: two topics, three services, one action, so every
/// published behaviour has a member behind it. `freeze_probe` is never
/// answered by the provider, which is what exercises the deadline path.
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

/// The recording contract. `finish_session` deliberately stays out of every
/// exposure: a running native member the MCP catalog must not reach.
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
                { link_id: "camera", name: "camera_status" },
            ],
        },
        services: {
            exposes: [
                { link_id: "camera", name: "video_stream_info" },
                { link_id: "camera", name: "set_brightness" },
                { link_id: "camera", name: "freeze_probe" },
            ],
        },
        actions: {
            exposes: [
                { link_id: "camera", name: "record_clip" },
            ],
        },
    },
}"#;

/// The camera: 8x8 rgb8 frames and a status snapshot every 100 ms, the
/// info and brightness services, never an answer to `freeze_probe`, and
/// `record_clip` running short goals to completion while parking long ones
/// on the cancel signal (republishing feedback so progress is observable
/// regardless of sensor-data QoS drops).
const CAMERA_MAIN: &str = r#"
use peppygen::emitted_topics::camera::{camera_status, video_stream};
use peppygen::exposed_actions::camera::record_clip;
use peppygen::exposed_services::camera::{set_brightness, video_stream_info};
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
                let payload = video_stream::build_message(frame.clone(), "rgb8".to_owned(), 8, 8)
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
                let payload = camera_status::build_message(87, "operational".to_owned(), true)
                    .expect("build camera_status message");
                publisher.publish(payload).await.expect("publish status");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        let runner = node_runner.clone();
        tokio::spawn(async move {
            loop {
                video_stream_info::handle_next_request(&runner, |_request| {
                    Ok(video_stream_info::Response::new(640, 480, 30.0, "/dev/video0".to_owned()))
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

fn camera_sha() -> String {
    ManifestFingerprint::of_bytes(CAMERA_CONTRACT.as_bytes()).to_string()
}

fn recording_sha() -> String {
    ManifestFingerprint::of_bytes(RECORDING_CONTRACT.as_bytes()).to_string()
}

/// The camera surface: every member of the camera contract, with the
/// policies the endpoint behaviours exercise. `tag` and `title` vary so two
/// tags of one exposure can be served side by side and told apart.
fn camera_endpoint_exposure(tag: &str, title: &str, sha256: Option<&str>) -> String {
    let pin = sha256
        .map(|sha| format!(", sha256: \"{sha}\""))
        .unwrap_or_default();
    format!(
        r#"{{
        peppy_schema: "mcp_exposure/v1",
        manifest: {{ name: "camera_endpoint", tag: "{tag}" }},
        server: {{
            title: "{title}",
            instructions: "Observe and control the front camera on this robot.",
        }},
        targets: {{
            front_camera: {{
                contract: {{ name: "rgb_camera", tag: "v1"{pin} }},
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

/// A second exposure sharing the `front_camera` target (same contract, no
/// author pin) and adding the recorder, with an `info` tool of its own so
/// two endpoints publish one public name.
fn camera_and_recording_exposure() -> String {
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
                contract: {{ name: "rgb_camera", tag: "v1" }},
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
                contract: {{ name: "episode_recording", tag: "v1", sha256: "{}" }},
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
    }}"#,
        recording_sha()
    )
}

/// An exposure binding the `front_camera` target name to the recording
/// contract: legal alone, a slot conflict beside the camera exposures.
fn conflicting_exposure() -> String {
    r#"{
        peppy_schema: "mcp_exposure/v1",
        manifest: { name: "conflicting", tag: "v1" },
        server: { title: "Conflicting" },
        targets: {
            front_camera: {
                contract: { name: "episode_recording", tag: "v1" },
                actions: [
                    {
                        member: "record_episode",
                        tool: "front_camera.record",
                        description: "Record.",
                        operation: "long_running",
                        deadline_ms: 60000,
                    },
                ],
            },
        },
    }"#
    .to_owned()
}

/// An exposure pinning the camera contract at bytes that are not the
/// contract's.
fn mispinned_exposure() -> String {
    format!(
        r#"{{
        peppy_schema: "mcp_exposure/v1",
        manifest: {{ name: "mispinned", tag: "v1" }},
        server: {{ title: "Mispinned" }},
        targets: {{
            front_camera: {{
                contract: {{ name: "rgb_camera", tag: "v1", sha256: "{}" }},
                services: [
                    {{
                        member: "video_stream_info",
                        tool: "front_camera.info",
                        description: "Report.",
                        operation: "read_only",
                        deadline_ms: 5000,
                    }},
                ],
            }},
        }},
    }}"#,
        "a".repeat(64)
    )
}

/// An exposure selecting members the camera contract does not declare.
fn broken_exposure() -> String {
    r#"{
        peppy_schema: "mcp_exposure/v1",
        manifest: { name: "broken", tag: "v1" },
        server: { title: "Broken" },
        targets: {
            front_camera: {
                contract: { name: "rgb_camera", tag: "v1" },
                services: [
                    {
                        member: "no_such_service",
                        tool: "front_camera.nothing",
                        description: "Nothing.",
                        operation: "read_only",
                        deadline_ms: 5000,
                    },
                ],
                topics: [
                    {
                        member: "no_such_topic",
                        resource: "front_camera.nothing_either",
                        description: "Nothing either.",
                        freshness: { max_age_ms: 1000 },
                        update: { max_hz: 10 },
                        max_result_bytes: 1024,
                        on_oversize: "reject",
                    },
                ],
            },
        },
    }"#
    .to_owned()
}

/// Writes one provider node crate into the hub and generates its peppygen
/// from the contract it implements, the way the daemon's sync would.
fn stage_provider(
    hub: &Path,
    peppy_dirs: &PeppyDirs,
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
        .build(&output_dir, peppy_dirs, Default::default())
        .expect("build provider peppygen");
    fs::remove_file(staged_config).expect("remove staged config");
    node_dir
}

/// Rewrites a staged manifest so its build fails at once, for launches
/// expected to refuse before any node runs.
fn refuse_build(node_dir: &Path) {
    let manifest_path = node_dir.join(NODE_CONFIG_FILE);
    let source = fs::read_to_string(&manifest_path).expect("staged manifest exists");
    let mut node_config: config::node::NodeConfig =
        serde_json5::from_str(&source).expect("staged manifest parses");
    node_config.execution.build_cmd = Some(vec!["false".to_string()]);
    node_config.execution.run_cmd = Some(vec!["true".to_string()]);
    fs::write(
        &manifest_path,
        serde_json5::to_string(&node_config).expect("staged manifest serializes"),
    )
    .expect("rewrite staged manifest");
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

/// A launched stack: the emulated daemon, its CLI context, and the hub it
/// resolves from.
struct Stack {
    serve: ServeCommandEmulation,
    ctx: Arc<AppContext>,
    peppy_dirs: PeppyDirs,
    _hub: tempfile::TempDir,
    nodes_dir: tempfile::TempDir,
}

impl Stack {
    /// Boots the daemon on a live router, stages the contracts, every
    /// exposure and both providers into a hub, seeds the daemon's caches
    /// from it, and installs the `peppy` binary under test where the daemon
    /// looks for the built-in server. With `compile`, the providers are
    /// built so a launch can start them; without, the launch is expected
    /// to refuse before any node starts.
    async fn boot(compile: bool) -> Self {
        let serve = ServeCommandEmulation::with_zenoh()
            .await
            .expect("zenoh serve emulation starts");
        let nodes_dir = tempfile::tempdir().expect("temp nodes dir");
        let ctx = Arc::new(
            AppContext::with_messenger(nodes_dir.path(), Arc::clone(&serve.messenger()))
                .with_daemon_state_file(serve.daemon_state_path()),
        );
        let peppy_dirs = PeppyDirs::new(serve.temp_dir());

        // The daemon runs the built-in server from the installed `peppy`;
        // in this emulation that is the binary under test, linked where the
        // installer places it (a debug binary is large, and every test
        // boots its own home).
        let bin_dir = peppy_dirs.bin_dir();
        fs::create_dir_all(&bin_dir).expect("create bin dir");
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_peppy"), bin_dir.join("peppy"))
            .expect("install the peppy binary under test");

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
        for (file, content) in [
            (
                "camera_endpoint_v1.json5",
                camera_endpoint_exposure("v1", "OpenArm camera", Some(&camera_sha())),
            ),
            (
                "camera_endpoint_v2.json5",
                camera_endpoint_exposure("v2", "OpenArm camera, second tag", None),
            ),
            (
                "camera_and_recording.json5",
                camera_and_recording_exposure(),
            ),
            ("conflicting.json5", conflicting_exposure()),
            ("mispinned.json5", mispinned_exposure()),
            ("broken.json5", broken_exposure()),
        ] {
            fs::write(hub.join("exposures").join(file), content).expect("write exposure");
        }

        let camera_dir = stage_provider(
            hub,
            &peppy_dirs,
            "mock_uvc_camera",
            CAMERA_NODE_CONFIG,
            CAMERA_MAIN,
            CAMERA_CONTRACT,
            "camera",
        );
        let recorder_dir = stage_provider(
            hub,
            &peppy_dirs,
            "mock_recorder",
            RECORDER_NODE_CONFIG,
            RECORDER_MAIN,
            RECORDING_CONTRACT,
            "recording",
        );
        if compile {
            let camera_binary = compile_node(&camera_dir, "mock_uvc_camera", "mock_uvc_camera");
            let recorder_binary = compile_node(&recorder_dir, "mock_recorder", "mock_recorder");
            point_manifest_at_binary(&camera_dir, &camera_binary);
            point_manifest_at_binary(&recorder_dir, &recorder_binary);
        } else {
            // A launch that gets past planning must not build anything
            // here: the providers refuse their build, quickly and by name.
            for dir in [&camera_dir, &recorder_dir] {
                refuse_build(dir);
            }
        }
        super::common::seed_docs_repo(&serve, &ctx, hub);

        Self {
            serve,
            ctx,
            peppy_dirs,
            _hub: hub_dir,
            nodes_dir,
        }
    }

    /// Launches `deployments` (the launcher's `deployments` array body).
    fn launch(&self, deployments: &str) -> Result<(), peppy::error::Error> {
        let launcher_path = self.nodes_dir.path().join("peppy_launcher.json5");
        fs::write(
            &launcher_path,
            format!(
                r#"{{
                    peppy_schema: "launcher/v1",
                    deployments: [{deployments}]
                }}"#
            ),
        )
        .expect("write launcher");
        StackCommand {
            command: StackCommands::Launch {
                place: Vec::new(),
                local: false,
                with: Default::default(),
                launcher_config_path: launcher_path,
                node_add_idle_timeout_secs: 120,
                node_build_idle_timeout_secs: 120,
                node_run_idle_timeout_secs: 120,
                max_timeout_secs: Some(900),
            },
        }
        .execute(&self.ctx)
    }

    fn launch_or_panic(&self, deployments: &str) {
        self.launch(deployments)
            .unwrap_or_else(|error| panic!("launch failed: {error:?}\n{}", self.run_logs()));
    }

    fn launch_error(&self, deployments: &str) -> String {
        match self.launch(deployments) {
            Ok(()) => panic!("the launch must be refused\n{}", self.run_logs()),
            Err(error) => error.to_string(),
        }
    }

    fn reset(&self) {
        StackCommand {
            command: StackCommands::Reset { federated: false },
        }
        .execute(&self.ctx)
        .expect("stack reset");
    }

    async fn stack_list(&self) -> String {
        list_nodes_collecting(&self.ctx, false)
            .await
            .expect("stack list answers")
            .output
    }

    /// The run logs of every instance, for readable panics.
    fn run_logs(&self) -> String {
        let dir = self.serve.temp_dir().join("logs/run");
        let mut logs = String::new();
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                logs.push_str(&format!(
                    "--- {} ---\n{}\n",
                    entry.path().display(),
                    fs::read_to_string(entry.path()).unwrap_or_default()
                ));
            }
        }
        logs
    }
}

/// The two provider deployments every launcher here starts.
const PROVIDERS: &str = r#"
    {
        source: { name: "mock_uvc_camera:v1" },
        instances: [{ instance_id: "the_camera" }]
    },
    {
        source: { name: "mock_recorder:v1" },
        instances: [{ instance_id: "episode_recorder_inst" }]
    },
"#;

/// One `exposures` deployment. The links follow the targets the listed
/// exposures declare: every set here draws on `front_camera`, and only
/// `camera_and_recording` adds `recorder`.
fn mcp_deployment(exposures: &[&str], instance_id: &str, port: u16) -> String {
    let listed: Vec<String> = exposures.iter().map(|e| format!("\"{e}\"")).collect();
    let recorder = if exposures.contains(&"camera_and_recording:v1") {
        r#"recorder: "episode_recorder_inst","#
    } else {
        ""
    };
    format!(
        r#"{{
            source: {{ exposures: [{}] }},
            instances: [
                {{
                    instance_id: "{instance_id}",
                    arguments: {{ port: {port} }},
                    links: {{
                        front_camera: "the_camera",
                        {recorder}
                    }},
                }}
            ]
        }}"#,
        listed.join(", ")
    )
}

fn endpoint(port: u16, path: &str) -> String {
    format!("http://127.0.0.1:{port}{path}")
}

async fn wait_for_port(port: u16, logs: impl Fn() -> String) {
    tokio::time::timeout(WAIT, async {
        loop {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("port {port} never accepted connections\n{}", logs()));
}

/// The status line of a raw HTTP request, for the paths no MCP client
/// would send to.
async fn raw_status(port: u16, method: &str, path: &str) -> String {
    let mut raw = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("raw connect");
    raw.write_all(
        format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
        )
        .as_bytes(),
    )
    .await
    .expect("send raw request");
    let mut response = String::new();
    let _ = raw.read_to_string(&mut response).await;
    response.lines().next().unwrap_or("").to_owned()
}

async fn await_resource_updates(subscription: &mut Subscription, expected: &[&str]) {
    let mut pending: Vec<&str> = expected.to_vec();
    while !pending.is_empty() {
        let notification = tokio::time::timeout(WAIT, subscription.next())
            .await
            .unwrap_or_else(|_| panic!("{pending:?} announced no snapshot within {WAIT:?}"))
            .expect("the subscription stream is healthy")
            .expect("the stream did not end");
        match notification {
            ServerNotification::ResourceUpdatedNotification(updated) => {
                assert!(
                    expected.contains(&updated.params.uri.as_str()),
                    "the filter subscribes {expected:?}, got {}",
                    updated.params.uri
                );
                pending.retain(|uri| *uri != updated.params.uri);
            }
            other => panic!("expected a resource-updated notification, got {other:?}"),
        }
    }
}

/// Fires `record_clip` as a task and walks the confirmation gate.
async fn start_confirmed_record_clip(
    client: &mcp_test_support::Client,
    duration_frames: u32,
    step: &str,
) -> String {
    let response = client
        .call_tool_once(
            CallToolRequestParams::new("front_camera.record_clip")
                .with_arguments(object(json!({ "duration_frames": duration_frames }))),
        )
        .await
        .unwrap_or_else(|error| panic!("{step}: the task-backed tool answers: {error:?}"));
    let CallToolResponse::Task(created) = response else {
        panic!("{step}: expected a task handle, got {response:?}");
    };
    assert_eq!(created.task.status, TaskStatus::Working);
    assert_eq!(
        created.task.ttl_ms,
        Some(600000 + 1000),
        "the advertised TTL is the whole-goal deadline plus the runtime's grace"
    );
    let task_id = created.task.task_id;
    let parked = poll_task_until(client, WAIT, &task_id, "input_required", |task| {
        task.status() == TaskStatus::InputRequired
    })
    .await;
    let rmcp::model::TaskPayload::InputRequired { input_requests } = parked.payload else {
        panic!("expected input_required, got {:?}", parked.payload);
    };
    assert!(input_requests.contains_key("confirmation"));
    client
        .update_task(confirmation_accept(&task_id))
        .await
        .expect("the confirmation is delivered");
    task_id
}

fn text_snapshot(read: rmcp::model::ReadResourceResult) -> Value {
    let rmcp::model::ResourceContents::TextResourceContents { text, .. } =
        read.contents.first().expect("one content item")
    else {
        panic!("expected text contents");
    };
    serde_json::from_str(text).expect("snapshot is JSON")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_launcher_deploys_three_exposures_on_one_process_and_a_client_walks_them() {
    let stack = Stack::boot(true).await;
    let port = ephemeral_port();

    // --- Composition: three exposures, one deployment, one process. Two
    // tags of one exposure serve side by side; the third shares the camera
    // target (without an author pin) and adds the recorder.
    stack.launch_or_panic(&format!(
        "{PROVIDERS}{}",
        mcp_deployment(
            &[
                "camera_endpoint:v1",
                "camera_and_recording:v1",
                "camera_endpoint:v2"
            ],
            "mcp_server",
            port
        )
    ));
    wait_for_port(port, || stack.run_logs()).await;

    let v1 = endpoint(port, "/camera_endpoint/v1/mcp");
    let v2 = endpoint(port, "/camera_endpoint/v2/mcp");
    let both = endpoint(port, "/camera_and_recording/v1/mcp");

    // --- Operations: `stack list` shows every endpoint of the instance,
    // under one node identity derived from the sorted exposure set.
    let listing = stack.stack_list().await;
    for url in [&v1, &v2, &both] {
        assert!(listing.contains(url.as_str()), "{listing}");
    }
    assert!(
        listing
            .contains("mcp_camera_and_recording_v1_camera_endpoint_v1_camera_endpoint_v2:builtin"),
        "{listing}"
    );
    assert_eq!(
        listing.matches("Instance endpoints").count(),
        1,
        "one endpoints section: {listing}"
    );

    // --- Each endpoint has its own discovery and catalog.
    let client = connect(&v1).await;
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
    assert_eq!(implementation.version, "v1");
    assert_eq!(implementation.title.as_deref(), Some("OpenArm camera"));

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

    // --- Catalog suite: what `peppy mcp catalog` prints is what the
    // endpoint advertises.
    let rendered =
        mcp_catalog_rendered(&stack.peppy_dirs, "camera_endpoint:v1").expect("the catalog derives");
    let catalog: Value = serde_json::from_str(&rendered).expect("the catalog is JSON");
    assert_eq!(catalog["bundle_format"], 1);
    assert_eq!(catalog["schema_mapping_version"], 1);
    assert_eq!(
        catalog["exposure"],
        json!({ "name": "camera_endpoint", "tag": "v1" })
    );
    assert_eq!(catalog["server"]["title"], "OpenArm camera");
    let mut catalog_tools: Vec<&str> = catalog["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .chain(catalog["tasks"].as_array().expect("tasks"))
        .map(|entry| entry["name"].as_str().expect("name"))
        .collect();
    catalog_tools.sort_unstable();
    assert_eq!(catalog_tools, tool_names);
    let mut catalog_resources: Vec<&str> = catalog["resources"]
        .as_array()
        .expect("resources")
        .iter()
        .map(|entry| entry["uri"].as_str().expect("uri"))
        .collect();
    catalog_resources.sort_unstable();
    assert_eq!(catalog_resources, resource_uris);
    let listed_schema = tools
        .tools
        .iter()
        .find(|tool| tool.name.as_ref() == "front_camera.set_brightness")
        .map(|tool| serde_json::to_value(&tool.input_schema).expect("schema serializes"))
        .expect("the tool is listed");
    let catalog_schema = catalog["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == "front_camera.set_brightness")
        .map(|entry| entry["input_schema"].clone())
        .expect("the tool is in the catalog");
    assert_eq!(
        listed_schema, catalog_schema,
        "the served schema is the catalog's"
    );

    // --- Resources: subscribe, then read the snapshots the notifications
    // announce. The status resource serves canonical JSON; the frame
    // resource applies the JPEG representation.
    let mut subscription = client
        .listen(
            SubscriptionFilter::builder()
                .resource_subscription(STATUS_URI)
                .resource_subscription(FRAME_URI)
                .build(),
        )
        .await
        .expect("subscriptions/listen is accepted");
    await_resource_updates(&mut subscription, &[STATUS_URI, FRAME_URI]).await;
    subscription.cancel().await.expect("subscription cancels");

    let read = client
        .read_resource(ReadResourceRequestParams::new(STATUS_URI))
        .await
        .expect("status snapshot serves");
    assert_eq!(read.cache_scope, Some(CacheScope::Private));
    assert_eq!(
        text_snapshot(read),
        json!({ "battery": 87, "note": "operational", "recording": true })
    );
    let snapshot = text_snapshot(
        client
            .read_resource(ReadResourceRequestParams::new(FRAME_URI))
            .await
            .expect("frame snapshot serves"),
    );
    assert_eq!(snapshot["encoding"], "mjpeg");
    assert_eq!(snapshot["width"], 8);
    assert_eq!(snapshot["height"], 8);
    let jpeg = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        snapshot["frame"].as_str().expect("frame is base64"),
    )
    .expect("frame decodes");
    assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "the served frame is a JPEG");

    // --- Tools: structured results through the runtime codec, restrict
    // bounds and unknown names refused before the graph, a deadline miss as
    // a readable tool error.
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
    let called = client
        .call_tool(CallToolRequestParams::new("front_camera.freeze_probe"))
        .await
        .expect("a deadline miss is a tool error, not a protocol error");
    assert_eq!(called.is_error, Some(true), "got {:?}", called.content);

    // --- Privacy: a live but unselected member, an unknown tool, an
    // unknown resource, and every path but the endpoints.
    for unselected in ["recorder.finish_session", "front_camera.set_gain"] {
        let error = protocol_error(
            client
                .call_tool(CallToolRequestParams::new(unselected))
                .await
                .expect_err("an unselected or unknown name is unreachable"),
        );
        assert_eq!(error.code, ErrorCode::INVALID_PARAMS, "{unselected}");
    }
    let error = protocol_error(
        client
            .read_resource(ReadResourceRequestParams::new("peppy://resource/absent"))
            .await
            .expect_err("absent resources are refused"),
    );
    assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
    // Without the tasks capability the action tool refuses before any
    // task or goal exists.
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

    for path in [
        "/",
        "/mcp",
        "/camera_endpoint",
        "/camera_endpoint/v1",
        "/camera_endpoint/v3/mcp",
        "/other/v1/mcp",
        "/camera_endpoint/v1/mcp/extra",
    ] {
        for method in ["GET", "POST"] {
            let status = raw_status(port, method, path).await;
            assert!(
                status.starts_with("HTTP/1.1 404"),
                "{method} {path} answered {status}"
            );
        }
    }

    // --- Isolation: the same public name resolves per endpoint, a task
    // handle from one endpoint is unknown to the other, and the second tag
    // carries its own prose.
    let both_client = connect(&both).await;
    let mut both_tools: Vec<String> = both_client
        .list_tools(None)
        .await
        .expect("tools/list answers")
        .tools
        .iter()
        .map(|tool| tool.name.to_string())
        .collect();
    both_tools.sort_unstable();
    assert_eq!(both_tools, ["front_camera.info", "recorder.record_episode"]);
    let called = both_client
        .call_tool(CallToolRequestParams::new("front_camera.info"))
        .await
        .expect("the shared public name answers on its own endpoint");
    assert_eq!(
        called
            .structured_content
            .as_ref()
            .map(|v| v["width"].clone()),
        Some(json!(640))
    );
    assert!(
        both_client
            .list_resources(None)
            .await
            .expect("answers")
            .resources
            .is_empty(),
        "the second exposure selects no resource"
    );
    both_client.cancel().await.expect("client disconnects");

    let v2_client = connect(&v2).await;
    let discovered = v2_client
        .discover(RequestMetaObject(Default::default()))
        .await
        .expect("server/discover answers");
    let implementation = discovered.server_info().expect("identity");
    assert_eq!(implementation.version, "v2");
    assert_eq!(
        implementation.title.as_deref(),
        Some("OpenArm camera, second tag")
    );
    v2_client.cancel().await.expect("client disconnects");

    // --- Tasks: confirmation, feedback, completion, cancellation,
    // reconnection, and the recorder's task through the shared slot.
    let tasks = connect_with_tasks(&v1).await;
    let mut tasks_view: Vec<String> = tasks
        .list_tools(None)
        .await
        .expect("tools/list answers the tasks session")
        .tools
        .iter()
        .map(|tool| tool.name.to_string())
        .collect();
    tasks_view.sort_unstable();
    assert_eq!(
        tasks_view, tool_names,
        "the tasks-capable session sees the same catalog on the same endpoint"
    );
    let error = protocol_error(
        tasks
            .call_tool_once(
                CallToolRequestParams::new("front_camera.record_clip")
                    .with_arguments(object(json!({ "duration_frames": "three" }))),
            )
            .await
            .expect_err("a non-integer duration is rejected"),
    );
    assert_eq!(error.code, ErrorCode::INVALID_PARAMS);

    let task_id = start_confirmed_record_clip(&tasks, 3, "completion").await;
    let completed = poll_task_until(&tasks, WAIT, &task_id, "a terminal status", |task| {
        task.status().is_terminal()
    })
    .await;
    assert_eq!(completed.status(), TaskStatus::Completed);
    let rmcp::model::TaskPayload::Completed { result } = completed.payload else {
        panic!("expected a completed payload");
    };
    assert_eq!(result["structuredContent"], json!({ "frames_written": 3 }));

    // The handle is unknown to the other endpoints.
    let other = connect_with_tasks(&v2).await;
    let error = protocol_error(
        other
            .get_task(GetTaskParams::new(&*task_id))
            .await
            .expect_err("another endpoint never created this task"),
    );
    assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
    other.cancel().await.expect("client disconnects");

    let task_id = start_confirmed_record_clip(&tasks, 100000, "cancellation").await;
    poll_task_until(&tasks, WAIT, &task_id, "feedback-driven progress", |task| {
        task.task
            .status_message
            .as_deref()
            .is_some_and(|message| message.contains("frame"))
    })
    .await;
    tasks
        .cancel_task(CancelTaskParams::new(&*task_id))
        .await
        .expect("tasks/cancel acknowledges");
    let cancelled = poll_task_until(&tasks, WAIT, &task_id, "a terminal status", |task| {
        task.status().is_terminal()
    })
    .await;
    assert_eq!(cancelled.status(), TaskStatus::Cancelled);

    let task_id = start_confirmed_record_clip(&tasks, 100000, "reconnection").await;
    poll_task_until(&tasks, WAIT, &task_id, "feedback-driven progress", |task| {
        task.task.status_message.is_some()
    })
    .await;
    tasks.cancel().await.expect("client disconnects mid-task");
    let reconnected = connect_with_tasks(&v1).await;
    reconnected
        .cancel_task(CancelTaskParams::new(&*task_id))
        .await
        .expect("the reconnected client cancels the same handle");
    let cancelled = poll_task_until(&reconnected, WAIT, &task_id, "a terminal status", |task| {
        task.status().is_terminal()
    })
    .await;
    assert_eq!(cancelled.status(), TaskStatus::Cancelled);
    reconnected.cancel().await.expect("client disconnects");

    let recorder = connect_with_tasks(&both).await;
    let response = recorder
        .call_tool_once(
            CallToolRequestParams::new("recorder.record_episode")
                .with_arguments(object(json!({ "episode_name": "demo" }))),
        )
        .await
        .expect("the recorder task starts");
    let CallToolResponse::Task(created) = response else {
        panic!("expected a task handle, got {response:?}");
    };
    let task_id = created.task.task_id;
    poll_task_until(&recorder, WAIT, &task_id, "input_required", |task| {
        task.status() == TaskStatus::InputRequired
    })
    .await;
    recorder
        .update_task(confirmation_accept(&task_id))
        .await
        .expect("the confirmation is delivered");
    let completed = poll_task_until(&recorder, WAIT, &task_id, "a terminal status", |task| {
        task.status().is_terminal()
    })
    .await;
    assert_eq!(completed.status(), TaskStatus::Completed);
    let rmcp::model::TaskPayload::Completed { result } = completed.payload else {
        panic!("expected a completed payload");
    };
    assert_eq!(result["structuredContent"], json!({ "frames": 5 }));

    recorder.cancel().await.expect("client disconnects");

    // --- Operations: a parked task on an endpoint, then the stack is torn
    // down: the endpoints close, the task with them, and a relaunch
    // restores the endpoints.
    let parking = connect_with_tasks(&v1).await;
    let parked = start_confirmed_record_clip(&parking, 100000, "parked before reset").await;
    poll_task_until(&parking, WAIT, &parked, "progress", |task| {
        task.task.status_message.is_some()
    })
    .await;
    stack.reset();
    tokio::time::timeout(WAIT, async {
        loop {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_err()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the endpoints close when the stack stops");
    assert!(
        parking
            .get_task(GetTaskParams::new(&*parked))
            .await
            .is_err(),
        "the running task went down with the server"
    );
    drop(parking);
    let listing = stack.stack_list().await;
    assert!(!listing.contains("Instance endpoints"), "{listing}");

    stack.launch_or_panic(&format!(
        "{PROVIDERS}{}",
        mcp_deployment(&["camera_endpoint:v1"], "mcp_server", port)
    ));
    wait_for_port(port, || stack.run_logs()).await;
    let client = connect(&v1).await;
    assert!(
        !client
            .list_tools(None)
            .await
            .expect("answers")
            .tools
            .is_empty()
    );
    client.cancel().await.expect("client disconnects");
    let listing = stack.stack_list().await;
    assert!(listing.contains(&v1), "{listing}");
    assert!(!listing.contains(&both), "{listing}");
    stack.reset();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_target_bound_to_two_contracts_is_refused_naming_both_exposures() {
    let stack = Stack::boot(false).await;
    let error = stack.launch_error(&format!(
        "{PROVIDERS}{}",
        mcp_deployment(
            &["camera_endpoint:v1", "conflicting:v1"],
            "mcp_server",
            8900
        )
    ));
    assert!(error.contains("target `front_camera`"), "{error}");
    assert!(error.contains("`camera_endpoint:v1`"), "{error}");
    assert!(error.contains("`conflicting:v1`"), "{error}");
    assert!(
        error.contains("rgb_camera:v1") && error.contains("episode_recording:v1"),
        "{error}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mismatching_author_pin_is_refused_and_an_absent_one_plans() {
    let stack = Stack::boot(false).await;
    let error = stack.launch_error(&format!(
        "{PROVIDERS}{}",
        mcp_deployment(&["mispinned:v1"], "mcp_server", 8900)
    ));
    assert!(error.contains("rgb_camera:v1"), "{error}");
    assert!(error.contains("sha256"), "{error}");
    assert!(error.contains(&"a".repeat(64)), "{error}");

    // Without an author pin the exposure plans against the deployment's
    // bytes; the refusal here comes later, from a provider whose build this
    // stack refuses, which proves resolution and planning passed.
    let error = stack.launch_error(&format!(
        "{PROVIDERS}{}",
        mcp_deployment(&["camera_endpoint:v2"], "mcp_server", 8900)
    ));
    assert!(error.contains("failed to build node"), "{error}");
    assert!(!error.contains("sha256"), "{error}");
    assert!(!error.contains("camera_endpoint"), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_exposure_that_does_not_validate_refuses_the_deployment_with_the_full_report() {
    let stack = Stack::boot(false).await;
    let error = stack.launch_error(&format!(
        "{PROVIDERS}{}",
        mcp_deployment(&["camera_endpoint:v1", "broken:v1"], "mcp_server", 8900)
    ));
    assert!(error.contains("`broken:v1`"), "{error}");
    assert!(error.contains("no_such_service"), "{error}");
    assert!(error.contains("no_such_topic"), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_deployments_naming_one_exposure_set_are_refused() {
    let stack = Stack::boot(false).await;
    let error = stack.launch_error(&format!(
        "{PROVIDERS}{}, {}",
        mcp_deployment(&["camera_endpoint:v1", "camera_endpoint:v2"], "mcp_a", 8900),
        mcp_deployment(&["camera_endpoint:v2", "camera_endpoint:v1"], "mcp_b", 8901)
    ));
    assert!(error.contains("duplicate deployment"), "{error}");
    assert!(error.contains("camera_endpoint:v1"), "{error}");
    assert!(error.contains("camera_endpoint:v2"), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missing_link_is_refused() {
    let stack = Stack::boot(false).await;
    let error = stack.launch_error(&format!(
        r#"{PROVIDERS}{{
            source: {{ exposures: ["camera_and_recording:v1"] }},
            instances: [
                {{
                    instance_id: "mcp_server",
                    links: {{ front_camera: "the_camera" }},
                }}
            ]
        }}"#
    ));
    assert!(error.contains("recorder"), "{error}");
}

/// `peppy stack resolve` holds an exposure deployment to the link rules
/// through the same derivation a launch uses, without a daemon round trip.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stack_resolve_checks_the_links_of_an_exposure_deployment() {
    let stack = Stack::boot(false).await;
    let write = |deployments: &str| -> PathBuf {
        let path = stack.nodes_dir.path().join("resolve_launcher.json5");
        fs::write(
            &path,
            format!(
                r#"{{
                    peppy_schema: "launcher/v1",
                    deployments: [{deployments}]
                }}"#
            ),
        )
        .expect("write launcher");
        path
    };

    let complete = write(&format!(
        "{PROVIDERS}{}",
        mcp_deployment(
            &["camera_and_recording:v1", "camera_endpoint:v1"],
            "mcp_server",
            8900
        )
    ));
    let (_, report) = peppy::commands::stack::resolve_rendered(&stack.peppy_dirs, complete, &[])
        .expect("a complete deployment resolves");
    assert!(
        !report.iter().any(|line| line.contains("not checked")),
        "{report:?}"
    );

    let unknown_slot = write(&format!(
        r#"{PROVIDERS}{{
            source: {{ exposures: ["camera_endpoint:v1"] }},
            instances: [
                {{
                    instance_id: "mcp_server",
                    links: {{ front_camera: "the_camera", recorder: "episode_recorder_inst" }},
                }}
            ]
        }}"#
    ));
    let error = peppy::commands::stack::resolve_rendered(&stack.peppy_dirs, unknown_slot, &[])
        .expect_err("a link naming no slot of the synthesized manifest is refused")
        .to_string();
    assert!(error.contains("recorder"), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_generated_mcp_node_name_fails_through_ordinary_resolution() {
    let stack = Stack::boot(false).await;
    let error = stack.launch_error(&format!(
        r#"{PROVIDERS}{{
            source: {{ name: "camera_endpoint_mcp:v1" }},
            instances: [{{ instance_id: "mcp_server" }}]
        }}"#
    ));
    assert!(error.contains("camera_endpoint_mcp:v1"), "{error}");
    assert!(error.contains("cache"), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_mcp_deployments_on_one_port_fail_the_second_by_name() {
    let stack = Stack::boot(true).await;
    let port = ephemeral_port();
    let error = stack.launch_error(&format!(
        "{PROVIDERS}{}, {}",
        mcp_deployment(&["camera_endpoint:v1"], "mcp_first", port),
        mcp_deployment(&["camera_endpoint:v2"], "mcp_second", port)
    ));
    assert!(error.contains("instance mcp_second"), "{error}");
    assert!(!error.contains("instance mcp_first"), "{error}");
    assert!(
        error.contains("exited"),
        "the second process refused to start: {error}"
    );
    let logs = stack.run_logs();
    assert!(
        logs.contains(&format!("cannot bind 127.0.0.1:{port}")),
        "{logs}"
    );
}

/// `peppy mcp serve` alone: refused without the daemon's spec, and refused
/// with the full validation report when the spec's exposure does not
/// validate against its pinned contract, before any connection is made.
#[test]
fn peppy_mcp_serve_refuses_without_a_spec_and_with_an_invalid_one() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_peppy"))
        .args(["mcp", "serve"])
        .env_remove(SPEC_ENV_VAR)
        .env_remove(config::consts::RUNTIME_CONFIG_VAR_NAME)
        .output()
        .expect("peppy runs");
    assert!(!output.status.success());
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(printed.contains(SPEC_ENV_VAR), "{printed}");

    let home = tempfile::tempdir().expect("temp home");
    let pin = |kind: &str, name: &str, content: &str| {
        json!({
            "kind": kind,
            "name": name,
            "tag": "v1",
            "sha256": ManifestFingerprint::of_bytes(content.as_bytes()).as_str(),
            "origin": { "source_type": "fs", "path": home.path().join(format!("{name}.json5")) },
        })
    };
    let broken = broken_exposure();
    let spec = json!({
        "exposures": [{ "pin": pin("mcp_exposure", "broken", &broken), "content": broken }],
        "contracts": [{ "pin": pin("contract", "rgb_camera", CAMERA_CONTRACT), "content": CAMERA_CONTRACT }],
    });
    let spec_path = home.path().join("mcp_serve.json5");
    fs::write(&spec_path, spec.to_string()).expect("write spec");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_peppy"))
        .args(["mcp", "serve"])
        .env(SPEC_ENV_VAR, &spec_path)
        .env(config::consts::PEPPY_HOME_ENV, home.path())
        .env_remove(config::consts::RUNTIME_CONFIG_VAR_NAME)
        .output()
        .expect("peppy runs");
    assert!(!output.status.success());
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(printed.contains("broken:v1"), "{printed}");
    assert!(printed.contains("no_such_service"), "{printed}");
    assert!(printed.contains("no_such_topic"), "{printed}");
}
