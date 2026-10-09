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
use peppy::commands::stack::{
    LaunchJoins, LauncherArgs, StackCommand, StackCommands, StackTimeouts, list_nodes_collecting,
};
use peppy::context::AppContext;
use peppy::test_support::ServeCommandEmulation;

use config::consts::{NODE_CONFIG_FILE, PEPPYGEN_OUTPUT_PATH};
use config::runtime::Name;
use core_node_api::encoding::LaunchJoin;
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
    CacheScope, CallToolRequestParams, CallToolResponse, CancelTaskParams, ClientConfig, ErrorCode,
    GetTaskParams, ProgressNotificationParam, ProtocolVersion, ReadResourceRequestParams,
    RequestMetaObject, ServerNotification, SubscriptionFilter, TaskStatus, object,
};
use rmcp::service::{NotificationContext, Subscription};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ClientHandler, ClientLifecycleMode, ClientServiceExt, RoleClient};
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

/// The camera contract: two topics, four services, one action, so every
/// published behaviour has a member behind it. `freeze_probe` is never
/// answered by the provider, which is what exercises the deadline path;
/// `snapshot` answers with a frame, or with none when asked for a blank.
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
            {
                name: "snapshot",
                request_message_format: { blank: "bool" },
                response_message_format: {
                    success: "bool",
                    message: "string",
                    frame: { $type: "array", $items: "u8" },
                    encoding: "string",
                    width: "u16",
                    height: "u16",
                },
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
                { link_id: "camera", name: "snapshot" },
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
/// info, brightness and snapshot services, never an answer to
/// `freeze_probe`, and
/// `record_clip` running short goals to completion while parking long ones
/// on the cancel signal (republishing feedback so progress is observable
/// regardless of sensor-data QoS drops).
const CAMERA_MAIN: &str = r#"
use peppygen::emitted_topics::camera::{camera_status, video_stream};
use peppygen::exposed_actions::camera::record_clip;
use peppygen::exposed_services::camera::{set_brightness, snapshot, video_stream_info};
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
                    Ok(video_stream_info::Response::new(
                        640,
                        480,
                        30.0,
                        format!("/dev/{}", runner.processor().bound_instance_id()),
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
        let runner = node_runner.clone();
        tokio::spawn(async move {
            loop {
                snapshot::handle_next_request(&runner, |request| {
                    if request.data.blank {
                        return Ok(snapshot::Response::new(
                            false,
                            "the shutter is closed".to_owned(),
                            Vec::new(),
                            String::new(),
                            0,
                            0,
                        ));
                    }
                    let frame: Vec<u8> = (0..8u32 * 8 * 3).map(|i| (i % 251) as u8).collect();
                    Ok(snapshot::Response::new(
                        true,
                        String::new(),
                        frame,
                        "rgb8".to_owned(),
                        8,
                        8,
                    ))
                })
                .await
                .expect("handle snapshot");
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
        call_record: {{
            tool: "camera.recent_calls",
            description: "The last state-changing calls of this endpoint, newest first.",
            keep: 50,
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
                        picture: {{
                            tool: "front_camera.look",
                            description: "Look through the front-facing camera: the latest frame as a picture.",
                        }},
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
                    {{
                        member: "snapshot",
                        tool: "front_camera.snapshot",
                        description: "Take one picture with the front camera.",
                        operation: "read_only",
                        deadline_ms: 5000,
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

/// A per-robot surface over the camera contract: one `front_camera` target
/// every robot of the stack fills with its own camera, listed and addressed
/// by the robot's name.
fn fleet_cameras_exposure() -> String {
    format!(
        r#"{{
        peppy_schema: "mcp_exposure/v1",
        manifest: {{ name: "fleet_cameras", tag: "v1" }},
        server: {{
            title: "Fleet cameras",
            instructions: "Call robot.list first; every other tool names a robot it lists.",
        }},
        robots: {{
            list: {{ tool: "robot.list", description: "The robots of the stack, each with its camera." }},
        }},
        targets: {{
            front_camera: {{
                contract: {{ name: "rgb_camera", tag: "v1", sha256: "{}" }},
                topics: [
                    {{
                        member: "video_stream",
                        resource: "front_camera.latest_frame",
                        description: "Latest frame from the robot's front camera, JPEG encoded.",
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
                        picture: {{
                            tool: "front_camera.look",
                            description: "Look through the front-facing camera: the latest frame as a picture.",
                        }},
                    }},
                    {{
                        member: "camera_status",
                        resource: "front_camera.status",
                        description: "Latest status snapshot of the robot's front camera.",
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
                        description: "Report the robot's camera resolution, frame rate, and encoding.",
                        operation: "read_only",
                        deadline_ms: 5000,
                    }},
                    {{
                        member: "set_brightness",
                        tool: "front_camera.set_brightness",
                        description: "Set the robot's camera brightness in device units.",
                        operation: "mutating",
                        deadline_ms: 5000,
                        restrict: {{ value: {{ min: -64, max: 64 }} }},
                    }},
                ],
                actions: [
                    {{
                        member: "record_clip",
                        tool: "front_camera.record_clip",
                        description: "Record a clip on the robot's camera. Long-running; returns a task handle.",
                        operation: "long_running",
                        deadline_ms: 600000,
                    }},
                ],
            }},
        }},
    }}"#,
        camera_sha()
    )
}

/// A second exposure sharing the `front_camera` target (same contract, no
/// author pin) and adding the recorder, with an `info` tool of its own so
/// two endpoints publish one public name, and `record_clip` without a
/// confirmation gate so a client without the tasks extension can run it
/// inside the call.
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
                actions: [
                    {{
                        member: "record_clip",
                        tool: "front_camera.record_clip",
                        description: "Record a short clip to local storage.",
                        operation: "long_running",
                        deadline_ms: 600000,
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

/// Rewrites a staged manifest so its process starts, says why it fails on
/// stderr, and exits before it is ready, with no build step.
fn fail_at_start(node_dir: &Path) {
    let manifest_path = node_dir.join(NODE_CONFIG_FILE);
    let source = fs::read_to_string(&manifest_path).expect("staged manifest exists");
    let mut node_config: config::node::NodeConfig =
        serde_json5::from_str(&source).expect("staged manifest parses");
    node_config.execution.build_cmd = None;
    node_config.execution.run_cmd = Some(
        [
            "sh",
            "-c",
            "echo 'the recorder finds no camera' >&2; exit 3",
        ]
        .map(str::to_owned)
        .to_vec(),
    );
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
    /// The filesystem repository the daemon resolves from.
    hub: tempfile::TempDir,
    nodes_dir: tempfile::TempDir,
    /// Everything the CLI commands log on this thread, the launch output
    /// included.
    log_capture: peppy::test_support::LogCapture,
    _log_guard: tracing::subscriber::DefaultGuard,
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
            ("fleet_cameras.json5", fleet_cameras_exposure()),
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

        let log_capture = peppy::test_support::LogCapture::new();
        let log_guard = log_capture.install();

        Self {
            serve,
            ctx,
            peppy_dirs,
            hub: hub_dir,
            nodes_dir,
            log_capture,
            _log_guard: log_guard,
        }
    }

    /// Launches `deployments` (the launcher's `deployments` array body).
    fn launch(&self, deployments: &str) -> Result<(), peppy::error::Error> {
        self.launch_launcher(
            &format!(
                r#"{{
                    peppy_schema: "launcher/v1",
                    deployments: [{deployments}]
                }}"#
            ),
            Vec::new(),
        )
    }

    /// Launches a whole launcher document, with the copies `joins` names
    /// started beside the ones it lists.
    fn launch_launcher(
        &self,
        launcher: &str,
        joins: Vec<LaunchJoin>,
    ) -> Result<(), peppy::error::Error> {
        let launcher_path = self.nodes_dir.path().join("peppy_launcher.json5");
        fs::write(&launcher_path, launcher).expect("write launcher");
        StackCommand {
            command: StackCommands::Launch(LauncherArgs {
                rebuild: false,
                placement: Default::default(),
                joins: LaunchJoins { joins },
                with: Default::default(),
                launcher_config_path: launcher_path,
                timeouts: stack_timeouts(),
            }),
        }
        .execute(&self.ctx)
    }

    /// Joins a copy of `option` named `name` onto the running stack.
    fn join(&self, option: &str, name: &str) {
        self.try_join(option, name)
            .unwrap_or_else(|error| panic!("join {name} failed: {error:?}\n{}", self.run_logs()));
    }

    /// The refusal a join of `option` named `name` gives.
    fn join_error(&self, option: &str, name: &str) -> String {
        match self.try_join(option, name) {
            Ok(()) => panic!("the join must be refused\n{}", self.run_logs()),
            Err(error) => error.to_string(),
        }
    }

    fn try_join(&self, option: &str, name: &str) -> Result<(), peppy::error::Error> {
        StackCommand {
            command: StackCommands::Join {
                copy: LaunchJoin {
                    option: option.to_owned(),
                    name: copy_name(name),
                },
                with: Default::default(),
                arguments: Vec::new(),
                place: None,
                timeouts: stack_timeouts(),
            },
        }
        .execute(&self.ctx)
    }

    fn remove(&self, name: &str) {
        StackCommand {
            command: StackCommands::Remove {
                name: copy_name(name),
            },
        }
        .execute(&self.ctx)
        .unwrap_or_else(|error| panic!("remove {name} failed: {error:?}\n{}", self.run_logs()));
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

    /// The refusal a whole launcher document gives.
    fn launch_launcher_error(&self, launcher: &str) -> String {
        match self.launch_launcher(launcher, Vec::new()) {
            Ok(()) => panic!("the launch must be refused\n{}", self.run_logs()),
            Err(error) => error.to_string(),
        }
    }

    /// Stages `framework_controls:v1`, whose one target names the daemon
    /// interface `stack_copies:v1`, and `broken_recorder:v1`, a recorder
    /// whose process exits before it is ready, into the hub, and refreshes
    /// the daemon's caches from it.
    fn stage_framework_controls(&self) {
        let hub = self.hub.path();
        fs::write(
            hub.join("exposures/framework_controls.json5"),
            FRAMEWORK_CONTROLS,
        )
        .expect("write exposure");
        let broken = stage_provider(
            hub,
            &self.peppy_dirs,
            "broken_recorder",
            &RECORDER_NODE_CONFIG.replace(r#"name: "mock_recorder""#, r#"name: "broken_recorder""#),
            RECORDER_MAIN,
            RECORDING_CONTRACT,
            "recording",
        );
        fail_at_start(&broken);
        super::common::seed_docs_repo(&self.serve, &self.ctx, hub);
    }

    /// The names of the copies the daemon's own stack list reports.
    async fn daemon_copies(&self) -> Vec<String> {
        let report = peppy::commands::stack::list_nodes_json_collecting(&self.ctx)
            .await
            .expect("stack list answers");
        let listed: Value = serde_json::from_str(&report.output).expect("the list is JSON");
        listed["core_nodes"][0]["copies"]
            .as_array()
            .expect("the coordinator reports its copies")
            .iter()
            .map(|copy| copy["name"].as_str().expect("a copy name").to_owned())
            .collect()
    }

    fn reset(&self) {
        StackCommand {
            command: StackCommands::Reset { federated: false },
        }
        .execute(&self.ctx)
        .expect("stack reset");
    }

    async fn stack_list(&self) -> String {
        list_nodes_collecting(&self.ctx, false, None)
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

fn stack_timeouts() -> StackTimeouts {
    StackTimeouts {
        node_add_idle_timeout_secs: 120,
        node_build_idle_timeout_secs: 120,
        node_run_idle_timeout_secs: 120,
        max_timeout_secs: Some(900),
    }
}

fn copy_name(name: &str) -> Name {
    Name::try_from(name.to_owned()).expect("a copy name")
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

/// Opens a raw `tools/call` of `front_camera.record_clip` for a goal that
/// parks until cancelled, from a client that declares no tasks extension,
/// and returns the connection once the goal's first feedback has arrived
/// on it as a progress event. Dropping the connection is the client going
/// away mid-call.
async fn open_record_clip_until_progress(port: u16, path: &str) -> tokio::net::TcpStream {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "front_camera.record_clip",
            "arguments": { "duration_frames": 100000 },
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientInfo": { "name": "raw", "version": "0" },
                "io.modelcontextprotocol/clientCapabilities": {},
                "progressToken": "call-1"
            }
        }
    })
    .to_string();
    let mut raw = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("raw connect");
    raw.write_all(
        format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nMcp-Protocol-Version: 2026-07-28\r\nMcp-Method: tools/call\r\nMcp-Name: front_camera.record_clip\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    )
    .await
    .expect("send raw call");
    let mut received = Vec::new();
    tokio::time::timeout(WAIT, async {
        let mut chunk = [0u8; 4096];
        loop {
            let read = raw
                .read(&mut chunk)
                .await
                .expect("the call stream is readable");
            assert_ne!(read, 0, "the call ended before any progress arrived");
            received.extend_from_slice(&chunk[..read]);
            let text = String::from_utf8_lossy(&received);
            if text.contains("\r\n") {
                assert!(
                    text.starts_with("HTTP/1.1 200 "),
                    "the call was refused: {text}"
                );
            }
            if text.contains("notifications/progress") && text.contains("frame") {
                return;
            }
        }
    })
    .await
    .expect("the parked goal's feedback arrives as progress on the call");
    raw
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

/// Asserts that `task` ended as a task does when its `record_clip` goal
/// ends cancelled: `cancelled`, its status message saying so with the
/// result the camera provider ends a cancelled clip with.
fn assert_record_clip_ended_cancelled(task: &rmcp::model::DetailedTask) {
    assert_eq!(task.status(), TaskStatus::Cancelled, "{:?}", task.payload);
    assert_eq!(
        task.task.status_message.as_deref(),
        Some(r#"the action was cancelled: {"frames_written":1}"#)
    );
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

/// The document of a read: its first content, as JSON.
fn text_snapshot(read: rmcp::model::ReadResourceResult) -> Value {
    let rmcp::model::ResourceContents::TextResourceContents { text, .. } =
        read.contents.first().expect("one content item")
    else {
        panic!("expected text contents");
    };
    serde_json::from_str(text).expect("snapshot is JSON")
}

fn assert_is_jpeg(base64_text: &str) {
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, base64_text)
        .expect("the frame is base64");
    assert_eq!(&bytes[..2], &[0xFF, 0xD8], "the served frame is a JPEG");
}

/// The document a picture tool answered with, once its result is held to
/// the shape of a picture: a JPEG image block, then the document as text.
fn looked_document(looked: rmcp::model::CallToolResult) -> Value {
    assert_eq!(looked.is_error, Some(false), "got {:?}", looked.content);
    let [image, text] = looked.content.as_slice() else {
        panic!("expected an image and a text, got {:?}", looked.content);
    };
    let image = image.as_image().expect("the first block is the image");
    assert_eq!(image.mime_type, "image/jpeg");
    assert_is_jpeg(&image.data);
    let document = looked.structured_content.expect("a structured document");
    let text = text.as_text().expect("the second block is the document");
    assert_eq!(
        serde_json::from_str::<Value>(&text.text).expect("the text is JSON"),
        document
    );
    document
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

    // --- Operations: the launch printed every endpoint the server announced,
    // labelled `<name>_<tag>`, under `MCP endpoints:`, and `stack list` shows
    // them under one node identity derived from the sorted exposure set.
    let launch_output = stack.log_capture.logs();
    let mcp_block = launch_output
        .find("MCP endpoints:")
        .unwrap_or_else(|| panic!("the launch prints the MCP block:\n{launch_output}"));
    assert!(
        !launch_output.contains("Web pages:"),
        "nothing in this stack serves a page:\n{launch_output}"
    );
    for (label, url) in [
        ("camera_and_recording_v1", &both),
        ("camera_endpoint_v1", &v1),
        ("camera_endpoint_v2", &v2),
    ] {
        let line = format!("    {label:<23}  {url}");
        let at = launch_output
            .find(&line)
            .unwrap_or_else(|| panic!("`{line}` is printed:\n{launch_output}"));
        assert!(at > mcp_block, "`{line}` sits under the MCP heading");
    }
    let listing = stack.stack_list().await;
    for (label, url) in [
        ("camera_and_recording_v1", &both),
        ("camera_endpoint_v1", &v1),
        ("camera_endpoint_v2", &v2),
    ] {
        let row = listing
            .lines()
            .find(|line| line.contains(url.as_str()))
            .unwrap_or_else(|| panic!("a row for {url}:\n{listing}"));
        assert!(
            row.contains("mcp") && row.contains(label),
            "the row carries the kind and the `<name>_<tag>` label: {row}"
        );
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
            "camera.recent_calls",
            "front_camera.freeze_probe",
            "front_camera.info",
            "front_camera.look",
            "front_camera.record_clip",
            "front_camera.set_brightness",
            "front_camera.snapshot"
        ]
    );
    let snapshot_tool = tools
        .tools
        .iter()
        .find(|tool| tool.name.as_ref() == "front_camera.snapshot")
        .expect("the pictured service is listed");
    assert!(
        snapshot_tool
            .output_schema
            .as_ref()
            .is_some_and(|schema| schema["properties"].get("frame").is_none()
                && schema["properties"].get("message").is_some()),
        "the output schema of a pictured service is the schema of its document: {:?}",
        snapshot_tool.output_schema
    );
    let look_tool = tools
        .tools
        .iter()
        .find(|tool| tool.name.as_ref() == "front_camera.look")
        .expect("the picture tool is listed");
    assert_eq!(
        look_tool
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.read_only_hint),
        Some(true),
        "a picture tool only reads"
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
    let frame_resource = resources
        .resources
        .iter()
        .find(|resource| resource.uri == FRAME_URI)
        .expect("the frame resource is listed");
    assert_eq!(
        frame_resource.mime_type.as_deref(),
        Some("image/jpeg"),
        "a resource with a representation is listed under the MIME type of its blob"
    );

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
        .chain(catalog["pictures"].as_array().expect("pictures"))
        .chain(std::iter::once(&catalog["call_record"]))
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
    let read = client
        .read_resource(ReadResourceRequestParams::new(FRAME_URI))
        .await
        .expect("frame snapshot serves");
    let [_document, blob] = read.contents.as_slice() else {
        panic!(
            "a resource with a representation serves its document, then its blob, got {:?}",
            read.contents
        );
    };
    let rmcp::model::ResourceContents::BlobResourceContents {
        blob,
        mime_type,
        uri,
        ..
    } = blob
    else {
        panic!("expected blob contents, got {blob:?}");
    };
    assert_eq!(uri, FRAME_URI);
    assert_eq!(mime_type.as_deref(), Some("image/jpeg"));
    assert_is_jpeg(blob);
    assert_eq!(
        text_snapshot(read),
        json!({ "encoding": "mjpeg", "width": 8, "height": 8 }),
        "the document leaves out the frame, which the blob carries"
    );

    // --- The picture tool: the latest frame as an image a model sees,
    // beside the document, inside the call.
    let looked = client
        .call_tool(CallToolRequestParams::new("front_camera.look"))
        .await
        .expect("the picture tool answers");
    assert_eq!(
        looked_document(looked),
        json!({ "encoding": "mjpeg", "width": 8, "height": 8 })
    );

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
        Some(json!({ "width": 640, "height": 480, "fps": 30.0, "device": "/dev/the_camera" }))
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

    // --- A service that answers with a picture: the frame as an image
    // block beside the document, and a refusal as the document alone.
    let snapped = client
        .call_tool(
            CallToolRequestParams::new("front_camera.snapshot")
                .with_arguments(object(json!({ "blank": false }))),
        )
        .await
        .expect("the pictured service answers");
    assert_eq!(
        looked_document(snapped),
        json!({ "success": true, "message": "", "encoding": "mjpeg", "width": 8, "height": 8 })
    );
    let blank = client
        .call_tool(
            CallToolRequestParams::new("front_camera.snapshot")
                .with_arguments(object(json!({ "blank": true }))),
        )
        .await
        .expect("the pictured service answers");
    assert_eq!(blank.is_error, Some(false), "got {:?}", blank.content);
    assert_eq!(
        blank.structured_content,
        Some(json!({
            "success": false,
            "message": "the shutter is closed",
            "encoding": "",
            "width": 0,
            "height": 0,
        }))
    );
    assert_eq!(
        blank.content.len(),
        1,
        "the document alone: {:?}",
        blank.content
    );

    // --- The record: the three brightness calls, newest first, under the
    // client's identity; the read-only calls and the reads left no entry.
    let recorded = client
        .call_tool(CallToolRequestParams::new("camera.recent_calls"))
        .await
        .expect("the record answers");
    assert_eq!(recorded.is_error, Some(false), "got {:?}", recorded.content);
    let calls = recorded.structured_content.expect("structured")["calls"]
        .as_array()
        .cloned()
        .expect("the calls");
    let summary: Vec<(&str, &str, Value)> = calls
        .iter()
        .map(|call| {
            (
                call["tool"].as_str().expect("tool"),
                call["outcome"].as_str().expect("outcome"),
                call["arguments"]["value"].clone(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("front_camera.set_brightness", "refused", json!(65)),
            ("front_camera.set_brightness", "completed", json!(-12)),
            ("front_camera.set_brightness", "completed", json!(12)),
        ]
    );
    for call in &calls {
        assert_eq!(
            call["client"]["name"], "rmcp",
            "the client's identity as its requests name it: {call:?}"
        );
        assert!(
            call["started_at"]
                .as_str()
                .is_some_and(|at| at.ends_with('Z'))
        );
    }

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
    // Without the tasks capability a confirmation-gated action refuses
    // before any task or goal exists, naming the tool and the extension.
    let error = protocol_error(
        client
            .call_tool_once(
                CallToolRequestParams::new("front_camera.record_clip")
                    .with_arguments(object(json!({ "duration_frames": 3 }))),
            )
            .await
            .expect_err("the confirmation needs a task"),
    );
    assert_eq!(error.code, ErrorCode::MISSING_REQUIRED_CLIENT_CAPABILITY);
    assert!(
        error.message.contains("`front_camera.record_clip`")
            && error.message.contains("io.modelcontextprotocol/tasks"),
        "got {}",
        error.message
    );
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
    assert_eq!(
        both_tools,
        [
            "front_camera.info",
            "front_camera.record_clip",
            "recorder.record_episode"
        ]
    );
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

    // --- In-call actions: without the tasks capability an unconfirmed
    // action runs inside the call, and the provider's terminal result is
    // the call's result.
    let recorded = both_client
        .call_tool(
            CallToolRequestParams::new("front_camera.record_clip")
                .with_arguments(object(json!({ "duration_frames": 3 }))),
        )
        .await
        .expect("the action answers inside the call");
    assert_eq!(recorded.is_error, Some(false));
    assert_eq!(
        recorded.structured_content,
        Some(json!({ "frames_written": 3 }))
    );
    // A parked goal reports its feedback as progress on the call, and the
    // client going away mid-call cancels it on the provider: the provider
    // serves one goal at a time, so the next clip completing is the parked
    // goal having ended.
    let parked = open_record_clip_until_progress(port, "/camera_and_recording/v1/mcp").await;
    drop(parked);
    let recorded = both_client
        .call_tool(
            CallToolRequestParams::new("front_camera.record_clip")
                .with_arguments(object(json!({ "duration_frames": 2 }))),
        )
        .await
        .expect("the provider is free again once the closed call's goal is cancelled");
    assert_eq!(
        recorded.structured_content,
        Some(json!({ "frames_written": 2 }))
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
    assert_record_clip_ended_cancelled(&cancelled);

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
    assert_record_clip_ended_cancelled(&cancelled);
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
    let (_, report) =
        peppy::commands::stack::resolve_rendered(&stack.peppy_dirs, complete, &[], &[])
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
    let error = peppy::commands::stack::resolve_rendered(&stack.peppy_dirs, unknown_slot, &[], &[])
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

/// The loopback port `stack list` reports for the endpoint labelled `label`.
fn listed_port(listing: &str, label: &str) -> u16 {
    const LOOPBACK: &str = "http://127.0.0.1:";
    listing
        .lines()
        .filter(|line| line.contains(label))
        .find_map(|line| {
            let after = &line[line.find(LOOPBACK)? + LOOPBACK.len()..];
            after.split('/').next()?.parse().ok()
        })
        .unwrap_or_else(|| panic!("a loopback URL labelled {label}:\n{listing}"))
}

/// A launcher's port is a preference: of two servers preferring one port,
/// one holds it and the other serves on a port the operating system picked,
/// which is the port the daemon reports and a client reaches.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_mcp_deployments_on_one_port_give_the_second_another_port() {
    let stack = Stack::boot(true).await;
    let preferred = ephemeral_port();
    stack.launch_or_panic(&format!(
        "{PROVIDERS}{}, {}",
        mcp_deployment(&["camera_endpoint:v1"], "mcp_first", preferred),
        mcp_deployment(&["camera_endpoint:v2"], "mcp_second", preferred)
    ));

    let listing = stack.stack_list().await;
    assert_eq!(
        listing.matches("Instance endpoints").count(),
        1,
        "one endpoints section: {listing}"
    );
    for instance_id in ["mcp_first", "mcp_second"] {
        assert!(listing.contains(instance_id), "{listing}");
    }
    let v1_port = listed_port(&listing, "camera_endpoint_v1");
    let v2_port = listed_port(&listing, "camera_endpoint_v2");
    assert_ne!(
        v1_port, v2_port,
        "each server holds its own port:\n{listing}"
    );
    // Which of the two reached the port first is theirs to settle.
    let fallback = match (v1_port == preferred, v2_port == preferred) {
        (true, false) => v2_port,
        (false, true) => v1_port,
        _ => panic!("one of the two holds the preferred port {preferred}:\n{listing}"),
    };

    // The launch printed the ports taken, the fallback included.
    let launch_output = stack.log_capture.logs();
    // Each reported URL reaches the server it is reported for.
    for (port, tag) in [(v1_port, "v1"), (v2_port, "v2")] {
        let url = endpoint(port, &format!("/camera_endpoint/{tag}/mcp"));
        assert!(launch_output.contains(&url), "{url}:\n{launch_output}");
        wait_for_port(port, || stack.run_logs()).await;
        let client = connect(&url).await;
        let discovered = client
            .discover(RequestMetaObject(Default::default()))
            .await
            .expect("server/discover answers");
        let implementation = discovered
            .server_info()
            .expect("the server identity rides in the result _meta");
        assert_eq!(implementation.version, tag, "{url}");
        client.cancel().await.expect("client disconnects");
    }

    let logs = stack.run_logs();
    let warning =
        format!("port {preferred} is held by another process; serving on port {fallback} instead");
    assert_eq!(
        logs.matches(&warning).count(),
        1,
        "the server that fell back says so once: `{warning}`\n{logs}"
    );
    stack.reset();
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

/// A launcher running the per-robot surface once for the stack, beside one
/// `robot` component whose only option is `option`, read from
/// `<option>.json5`. `copies` is the deployment line's own `instances`
/// entry, empty for a fleet the launch starts with no robot. The launcher's
/// entry under the option fills `front_camera` with the copy's `front_camera`
/// ids.
fn fleet_launcher(port: u16, option: &str, front_camera: &[&str], copies: &str) -> String {
    let front_camera = front_camera
        .iter()
        .map(|id| format!("\"{id}\""))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"{{
        peppy_schema: "launcher/v1",
        components: [
            {{
                name: "robot",
                cardinality: "zero_or_more",
                options: {{
                    {option}: {{
                        fragments: ["{option}.json5"],
                        adjustments: [
                            {{ target: "mcp_server", add_links: {{ front_camera: [{front_camera}] }} }},
                        ],
                    }},
                }},
            }},
        ],
        deployments: [
            {{
                source: {{ exposures: ["fleet_cameras:v1"] }},
                instances: [{{ instance_id: "mcp_server", arguments: {{ port: {port} }} }}],
            }},
            {{ robot: "{option}"{copies} }},
        ],
    }}"#
    )
}

/// Writes `fragment` where a launcher's `option` resolves it.
fn write_robot_fragment(stack: &Stack, option: &str, fragment: &str) {
    fs::write(
        stack.nodes_dir.path().join(format!("{option}.json5")),
        fragment,
    )
    .expect("write the robot fragment");
}

/// A robot fragment deploying one camera, which the launcher's entry under
/// the robot's option writes into `front_camera`, a target every robot of
/// the fleet surface fills once.
const CAMERA_ROBOT_FRAGMENT: &str = r#"{
    peppy_schema: "launcher_fragment/v1",
    deployments: [
        {
            source: { name: "mock_uvc_camera:v1" },
            instances: [{ instance_id: "cam" }],
        },
    ],
}"#;

fn robot_uri(robot: &str, resource: &str) -> String {
    format!("peppy://resource/{robot}/{resource}")
}

/// The robots `robot.list` reports, by name, and the entry of `robot`.
async fn listed_robots(client: &mcp_test_support::Client, robot: &str) -> (Vec<String>, Value) {
    let listing = client
        .call_tool(CallToolRequestParams::new("robot.list"))
        .await
        .expect("robot.list answers");
    assert_ne!(listing.is_error, Some(true), "got {:?}", listing.content);
    let robots = listing.structured_content.expect("a structured listing")["robots"]
        .as_array()
        .expect("robots")
        .clone();
    let names = robots
        .iter()
        .map(|entry| entry["robot"].as_str().expect("robot").to_owned())
        .collect();
    let entry = robots
        .into_iter()
        .find(|entry| entry["robot"] == robot)
        .unwrap_or(Value::Null);
    (names, entry)
}

async fn front_camera_info(
    client: &mcp_test_support::Client,
    robot: &str,
) -> Result<rmcp::model::CallToolResult, rmcp::ServiceError> {
    client
        .call_tool(
            CallToolRequestParams::new("front_camera.info")
                .with_arguments(object(json!({ "robot": robot }))),
        )
        .await
}

/// A call routed to `robot`, which must be there.
async fn front_camera_info_answers(client: &mcp_test_support::Client, robot: &str) -> Value {
    let called = front_camera_info(client, robot)
        .await
        .expect("front_camera.info answers");
    assert_ne!(called.is_error, Some(true), "got {:?}", called.content);
    called.structured_content.expect("a structured answer")
}

/// A call naming a robot that is not there is refused as invalid
/// parameters, naming the robots that are.
async fn front_camera_info_refused(client: &mcp_test_support::Client, robot: &str) -> String {
    let error = protocol_error(
        front_camera_info(client, robot)
            .await
            .expect_err("a robot that is not there is refused"),
    );
    assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
    error.message.to_string()
}

async fn listed_resource_uris(client: &mcp_test_support::Client) -> Vec<String> {
    let mut uris: Vec<String> = client
        .list_resources(None)
        .await
        .expect("resources/list answers")
        .resources
        .iter()
        .map(|resource| resource.uri.clone())
        .collect();
    uris.sort_unstable();
    uris
}

async fn await_resource_list_changed(subscription: &mut Subscription) {
    let notification = tokio::time::timeout(WAIT, subscription.next())
        .await
        .unwrap_or_else(|_| panic!("no resources/list_changed within {WAIT:?}"))
        .expect("the subscription stream is healthy")
        .expect("the stream did not end");
    assert!(
        matches!(
            notification,
            ServerNotification::ResourceListChangedNotification(_)
        ),
        "expected resources/list_changed, got {notification:?}"
    );
}

/// The per-robot surface end to end. A launch with no robot serves an
/// empty fleet; the first join is listed when it returns; a second robot
/// joins while a task runs on the first, which finishes undisturbed; each
/// call reaches the robot it names; a removal takes a robot and its
/// resources out; a subscribed client is told of every change; and a
/// relaunch lists the file's robot beside the one `--join` names.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_per_robot_surface_serves_every_robot_of_the_stack_by_name() {
    let stack = Stack::boot(true).await;
    let port = ephemeral_port();
    write_robot_fragment(&stack, "camera_robot", CAMERA_ROBOT_FRAGMENT);

    // --- No robot yet: the endpoint is up with an empty fleet, and a call
    // naming a robot says how one is added.
    stack
        .launch_launcher(
            &fleet_launcher(port, "camera_robot", &["cam"], ""),
            Vec::new(),
        )
        .unwrap_or_else(|error| panic!("launch failed: {error:?}\n{}", stack.run_logs()));
    wait_for_port(port, || stack.run_logs()).await;
    let listing = stack.stack_list().await;
    assert!(
        listing.contains("mcp_fleet_cameras_v1:builtin"),
        "{listing}"
    );
    let client = connect_with_tasks(&endpoint(port, "/fleet_cameras/v1/mcp")).await;
    let (robots, _) = listed_robots(&client, "alpha").await;
    assert!(robots.is_empty(), "{robots:?}");
    assert!(listed_resource_uris(&client).await.is_empty());
    assert_eq!(
        front_camera_info_refused(&client, "alpha").await,
        "`alpha` is not a robot of this stack, which has no robot; `peppy stack join \
         OPTION:NAME` adds one"
    );

    // --- The tool list is the union of the targets' tools plus the
    // listing tool, each taking the robot by name, whatever the fleet.
    let tools = client.list_tools(None).await.expect("tools/list answers");
    let mut tool_names: Vec<_> = tools.tools.iter().map(|tool| tool.name.as_ref()).collect();
    tool_names.sort_unstable();
    assert_eq!(
        tool_names,
        [
            "front_camera.info",
            "front_camera.look",
            "front_camera.record_clip",
            "front_camera.set_brightness",
            "robot.list"
        ]
    );
    let info_schema = tools
        .tools
        .iter()
        .find(|tool| tool.name.as_ref() == "front_camera.info")
        .map(|tool| serde_json::to_value(&tool.input_schema).expect("schema serializes"))
        .expect("the tool is listed");
    assert_eq!(info_schema["required"], json!(["robot"]));
    assert_eq!(info_schema["properties"]["robot"]["type"], "string");

    // --- The first join is listed when it returns, with no wait, and the
    // listening client is told.
    let mut subscription = client
        .listen(
            SubscriptionFilter::builder()
                .resources_list_changed()
                .build(),
        )
        .await
        .expect("subscriptions/listen is accepted");
    stack.join("camera_robot", "alpha");
    let (robots, alpha) = listed_robots(&client, "alpha").await;
    assert_eq!(robots, ["alpha"]);
    assert_eq!(
        alpha,
        json!({
            "robot": "alpha",
            "tools": [
                "front_camera.info",
                "front_camera.look",
                "front_camera.record_clip",
                "front_camera.set_brightness",
            ],
            "resources": ["alpha/front_camera.latest_frame", "alpha/front_camera.status"],
            "members": {},
            "notes": [],
        })
    );
    await_resource_list_changed(&mut subscription).await;

    // --- A task runs on alpha while bravo joins, and finishes undisturbed.
    let response = client
        .call_tool_once(
            CallToolRequestParams::new("front_camera.record_clip")
                .with_arguments(object(json!({ "robot": "alpha", "duration_frames": 1000 }))),
        )
        .await
        .expect("the task-backed tool answers");
    let CallToolResponse::Task(created) = response else {
        panic!("expected a task handle, got {response:?}");
    };
    let task_id = created.task.task_id;
    stack.join("camera_robot", "bravo");
    await_resource_list_changed(&mut subscription).await;
    let (robots, _) = listed_robots(&client, "bravo").await;
    assert_eq!(robots, ["alpha", "bravo"]);
    let running = client
        .get_task(GetTaskParams::new(&*task_id))
        .await
        .expect("tasks/get answers");
    assert_eq!(running.task.status(), TaskStatus::Working);
    client
        .cancel_task(CancelTaskParams::new(&*task_id))
        .await
        .expect("the cancel is delivered");
    let cancelled = poll_task_until(&client, WAIT, &task_id, "a terminal status", |task| {
        task.status().is_terminal()
    })
    .await;
    assert_record_clip_ended_cancelled(&cancelled);

    // --- Resources are published per robot, a call reaches the robot it
    // names, and a robot that is not there is refused naming the ones
    // that are.
    assert_eq!(
        listed_resource_uris(&client).await,
        [
            robot_uri("alpha", "front_camera.latest_frame"),
            robot_uri("alpha", "front_camera.status"),
            robot_uri("bravo", "front_camera.latest_frame"),
            robot_uri("bravo", "front_camera.status"),
        ]
    );
    for (robot, device) in [("alpha", "/dev/alpha_cam"), ("bravo", "/dev/bravo_cam")] {
        assert_eq!(
            front_camera_info_answers(&client, robot).await,
            json!({ "width": 640, "height": 480, "fps": 30.0, "device": device })
        );
    }
    assert_eq!(
        front_camera_info_refused(&client, "charlie").await,
        "`charlie` is not a robot of this stack; the robots are `alpha`, `bravo`"
    );
    // Bravo's status is read once the server announces its first snapshot.
    let bravo_status = robot_uri("bravo", "front_camera.status");
    let mut status_subscription = client
        .listen(
            SubscriptionFilter::builder()
                .resource_subscription(bravo_status.as_str())
                .build(),
        )
        .await
        .expect("subscriptions/listen is accepted");
    await_resource_updates(&mut status_subscription, &[&bravo_status]).await;
    status_subscription
        .cancel()
        .await
        .expect("subscription cancels");
    let read = client
        .read_resource(ReadResourceRequestParams::new(bravo_status.clone()))
        .await
        .expect("bravo's status serves once its camera publishes");
    assert_eq!(
        text_snapshot(read),
        json!({ "battery": 87, "note": "operational", "recording": true })
    );

    // --- A picture tool answers for the robot the call names, once that
    // robot's camera has published a frame, and refuses a robot that is
    // not there like any other tool.
    let bravo_frame = robot_uri("bravo", "front_camera.latest_frame");
    let mut frame_subscription = client
        .listen(
            SubscriptionFilter::builder()
                .resource_subscription(bravo_frame.as_str())
                .build(),
        )
        .await
        .expect("subscriptions/listen is accepted");
    await_resource_updates(&mut frame_subscription, &[&bravo_frame]).await;
    frame_subscription
        .cancel()
        .await
        .expect("subscription cancels");
    let looked = client
        .call_tool(
            CallToolRequestParams::new("front_camera.look")
                .with_arguments(object(json!({ "robot": "bravo" }))),
        )
        .await
        .expect("the picture tool answers for bravo");
    assert_eq!(
        looked_document(looked),
        json!({ "encoding": "mjpeg", "width": 8, "height": 8 })
    );
    let refused = protocol_error(
        client
            .call_tool(
                CallToolRequestParams::new("front_camera.look")
                    .with_arguments(object(json!({ "robot": "charlie" }))),
            )
            .await
            .expect_err("a robot that is not there is refused"),
    );
    assert_eq!(refused.code, ErrorCode::INVALID_PARAMS);
    assert_eq!(
        refused.message,
        "`charlie` is not a robot of this stack; the robots are `alpha`, `bravo`"
    );

    // --- A removal takes the robot and its resources out.
    stack.remove("bravo");
    await_resource_list_changed(&mut subscription).await;
    subscription.cancel().await.expect("subscription cancels");
    let (robots, _) = listed_robots(&client, "alpha").await;
    assert_eq!(robots, ["alpha"]);
    assert_eq!(
        listed_resource_uris(&client).await,
        [
            robot_uri("alpha", "front_camera.latest_frame"),
            robot_uri("alpha", "front_camera.status"),
        ]
    );
    assert_eq!(
        front_camera_info_refused(&client, "bravo").await,
        "`bravo` is not a robot of this stack; the robots are `alpha`"
    );
    // The status snapshot read above sits in the client's cache for its
    // TTL, so the server is asked for a resource this client never read.
    let gone = protocol_error(
        client
            .read_resource(ReadResourceRequestParams::new(robot_uri(
                "bravo",
                "front_camera.latest_frame",
            )))
            .await
            .expect_err("a removed robot's resource is gone"),
    );
    assert_eq!(gone.code, ErrorCode::INVALID_PARAMS);
    assert!(
        gone.message.contains("the robots are `alpha`"),
        "{}",
        gone.message
    );

    // --- A relaunch lists the file's robot beside the one `--join` names,
    // both there when the launch returns.
    stack.reset();
    let port = ephemeral_port();
    stack
        .launch_launcher(
            &fleet_launcher(
                port,
                "camera_robot",
                &["cam"],
                r#", instances: [{ instance_id: "alpha" }]"#,
            ),
            vec![LaunchJoin {
                option: "camera_robot".to_owned(),
                name: copy_name("charlie"),
            }],
        )
        .unwrap_or_else(|error| panic!("relaunch failed: {error:?}\n{}", stack.run_logs()));
    wait_for_port(port, || stack.run_logs()).await;
    let client = connect(&endpoint(port, "/fleet_cameras/v1/mcp")).await;
    let (robots, _) = listed_robots(&client, "alpha").await;
    assert_eq!(robots, ["alpha", "charlie"]);
    stack.reset();
}

/// A robot fragment deploying two cameras, both of which the launcher's
/// entry under the robot's option writes into `front_camera`, a target every
/// robot of the fleet surface fills once.
const TWO_CAMERA_ROBOT_FRAGMENT: &str = r#"{
    peppy_schema: "launcher_fragment/v1",
    deployments: [
        {
            source: { name: "mock_uvc_camera:v1" },
            instances: [{ instance_id: "left_cam" }, { instance_id: "right_cam" }],
        },
    ],
}"#;

/// A robot whose option's entry fills a once-per-robot target with two of
/// its instances is refused while the launch is still a plan, naming the
/// robot and both ids the entry wrote.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_robot_filling_a_once_per_robot_target_twice_is_refused_at_launch() {
    let stack = Stack::boot(false).await;
    write_robot_fragment(&stack, "two_camera_robot", TWO_CAMERA_ROBOT_FRAGMENT);
    let error = stack.launch_launcher_error(&fleet_launcher(
        ephemeral_port(),
        "two_camera_robot",
        &["left_cam", "right_cam"],
        r#", instances: [{ instance_id: "alpha" }]"#,
    ));
    assert!(
        error.contains(
            "copy `alpha` fills `mcp_server.links.front_camera` with 2 instances (`left_cam`, \
             `right_cam`), and `front_camera` holds one member per copy. Name one of them under \
             `front_camera` in the option's `add_links`"
        ),
        "{error}"
    );
}

/// A per-robot target takes the instances of copies, so a camera the
/// launcher deploys beside the server is refused while the launch is still
/// a plan.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_camera_outside_every_copy_cannot_fill_a_per_robot_target() {
    let stack = Stack::boot(false).await;
    let error = stack.launch_error(&format!(
        r#"{PROVIDERS}{{
            source: {{ exposures: ["fleet_cameras:v1"] }},
            instances: [
                {{
                    instance_id: "mcp_server",
                    arguments: {{ port: {} }},
                    links: {{ front_camera: ["the_camera"] }},
                }}
            ]
        }}"#,
        ephemeral_port()
    ));
    assert!(
        error.contains(
            "`the_camera` fills `mcp_server.links.front_camera` from outside any copy, and every \
             member of `front_camera` belongs to a copy. Add the instance from the launcher's \
             `adjustments` under the copy's option, with `add_links: { front_camera: [\"<id>\"] }` \
             on `mcp_server`, and drop it from `mcp_server`'s `links`"
        ),
        "{error}"
    );
}

/// A join carries the same rule: the copy is refused before it starts, and
/// the fleet the running server serves is untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_join_filling_a_once_per_robot_target_twice_is_refused() {
    let stack = Stack::boot(true).await;
    let port = ephemeral_port();
    write_robot_fragment(&stack, "two_camera_robot", TWO_CAMERA_ROBOT_FRAGMENT);
    stack
        .launch_launcher(
            &fleet_launcher(port, "two_camera_robot", &["left_cam", "right_cam"], ""),
            Vec::new(),
        )
        .unwrap_or_else(|error| panic!("launch failed: {error:?}\n{}", stack.run_logs()));
    wait_for_port(port, || stack.run_logs()).await;
    let client = connect(&endpoint(port, "/fleet_cameras/v1/mcp")).await;
    let (robots, _) = listed_robots(&client, "alpha").await;
    assert!(robots.is_empty(), "{robots:?}");

    let error = stack.join_error("two_camera_robot", "alpha");
    assert!(
        error.contains(
            "copy `alpha` fills `mcp_server.links.front_camera` with 2 instances (`left_cam`, \
             `right_cam`)"
        ),
        "{error}"
    );

    // Nothing of the copy reached the stack, and the server still serves an
    // empty fleet.
    let listing = stack.stack_list().await;
    assert!(!listing.contains("alpha_left_cam"), "{listing}");
    let (robots, _) = listed_robots(&client, "alpha").await;
    assert!(robots.is_empty(), "{robots:?}");
    stack.reset();
}

// --- A daemon target: the copies of the stack over MCP.

/// The design's `framework/framework_controls.json5`: one fixed target,
/// `stack`, that names the daemon interface `stack_copies:v1`.
const FRAMEWORK_CONTROLS: &str = include_str!(
    "../../daemon-config-internal/src/daemon_interface/fixtures/framework_controls.json5"
);

/// A copy of the recorder, which starts and stays.
const RECORDER_ROBOT_FRAGMENT: &str = r#"{
    peppy_schema: "launcher_fragment/v1",
    deployments: [
        {
            source: { name: "mock_recorder:v1" },
            instances: [{ instance_id: "recorder" }],
        },
    ],
}"#;

/// A copy of the recorder that exits before it is ready.
const BROKEN_ROBOT_FRAGMENT: &str = r#"{
    peppy_schema: "launcher_fragment/v1",
    deployments: [
        {
            source: { name: "broken_recorder:v1" },
            instances: [{ instance_id: "recorder" }],
        },
    ],
}"#;

/// The framework's endpoint on `port`, whose scope offers two of the three
/// options of the `robot` axis, which runs as copies, up to two copies.
fn framework_launcher(port: u16) -> String {
    format!(
        r#"{{
        peppy_schema: "launcher/v1",
        components: [
            {{
                name: "robot",
                cardinality: "zero_or_more",
                options: {{
                    recorder_robot: "recorder_robot.json5",
                    broken_robot: "broken_robot.json5",
                    camera_robot: "camera_robot.json5",
                }},
            }},
        ],
        deployments: [
            {{
                source: {{ exposures: ["framework_controls:v1"] }},
                instances: [
                    {{ instance_id: "framework_controls_inst", arguments: {{ port: {port} }} }},
                ],
            }},
        ],
        adjustments: [
            {{
                target: "framework_controls_inst",
                set_daemon_scopes: {{ stack: {{
                    max_copies: 2,
                    options: [
                        {{ option: "recorder_robot", description: "A robot that records episodes" }},
                        {{ option: "broken_robot", description: "A robot whose software cannot start" }},
                    ],
                }} }},
            }},
        ],
    }}"#
    )
}

/// A client that keeps every progress notification it receives. The client
/// handles each notification on a task of its own, so a test waits for the
/// ones it expects, and reads them in the order of their `progress`.
struct ProgressClient {
    info: ClientConfig,
    received: tokio::sync::watch::Sender<Vec<ProgressNotificationParam>>,
}

impl ClientHandler for ProgressClient {
    fn get_info(&self) -> ClientConfig {
        self.info.clone()
    }

    async fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.received.send_modify(|received| received.push(params));
    }
}

type FrameworkClient = rmcp::service::RunningService<RoleClient, ProgressClient>;

/// Connects a client without the tasks extension to `endpoint_url`, and
/// returns it with the progress notifications it receives.
async fn connect_recording_progress(
    endpoint_url: &str,
) -> (
    FrameworkClient,
    tokio::sync::watch::Receiver<Vec<ProgressNotificationParam>>,
) {
    let (received, progress) = tokio::sync::watch::channel(Vec::new());
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(endpoint_url.to_owned()),
    );
    let client = ProgressClient {
        info: ClientConfig::default(),
        received,
    }
    .serve_with_lifecycle(
        transport,
        ClientLifecycleMode::Discover {
            preferred_versions: vec![ProtocolVersion::V_2026_07_28],
        },
    )
    .await
    .expect("the MCP client negotiates 2026-07-28");
    (client, progress)
}

/// Calls `tool` with `arguments`. The client sends a progress token of its
/// own with every call.
async fn call(
    client: &FrameworkClient,
    tool: &'static str,
    arguments: Value,
) -> rmcp::model::CallToolResult {
    let params = CallToolRequestParams::new(tool).with_arguments(object(arguments));
    tokio::time::timeout(WAIT, client.call_tool(params))
        .await
        .unwrap_or_else(|_| panic!("{tool} ends"))
        .unwrap_or_else(|error| panic!("{tool} answers: {error}"))
}

/// The messages of one call's progress notifications, in their order: the
/// call whose progress says `first`, once `complete` holds of its messages.
async fn progress_of(
    progress: &mut tokio::sync::watch::Receiver<Vec<ProgressNotificationParam>>,
    first: &str,
    complete: impl Fn(&[String]) -> bool,
) -> Vec<String> {
    let call_progress = |received: &[ProgressNotificationParam]| {
        let token = received
            .iter()
            .find(|params| params.message.as_deref() == Some(first))?
            .progress_token
            .clone();
        let mut ours: Vec<&ProgressNotificationParam> = received
            .iter()
            .filter(|params| params.progress_token == token)
            .collect();
        ours.sort_by(|left, right| left.progress.total_cmp(&right.progress));
        let messages: Vec<String> = ours
            .iter()
            .map(|params| params.message.clone().unwrap_or_default())
            .collect();
        complete(&messages).then_some(messages)
    };
    let waited = tokio::time::timeout(
        WAIT,
        progress.wait_for(|received| call_progress(received).is_some()),
    )
    .await
    .map(|received| received.expect("the client outlives the wait").clone());
    match waited {
        Ok(received) => call_progress(&received).expect("the progress is complete"),
        Err(_) => panic!(
            "the progress of the call that says {first:?} arrives; received: {:?}",
            progress
                .borrow()
                .iter()
                .map(|params| params.message.clone())
                .collect::<Vec<_>>()
        ),
    }
}

/// Calls a tool that takes no argument and answers structured content.
async fn structured(client: &FrameworkClient, tool: &'static str) -> Value {
    client
        .call_tool(CallToolRequestParams::new(tool))
        .await
        .unwrap_or_else(|error| panic!("{tool} answers: {error}"))
        .structured_content
        .unwrap_or_else(|| panic!("{tool} answers structured content"))
}

/// The text of a tool error.
fn tool_error_text(result: &rmcp::model::CallToolResult) -> String {
    assert_eq!(result.is_error, Some(true), "{result:?}");
    serde_json::to_value(&result.content).expect("content serializes")[0]["text"]
        .as_str()
        .expect("a text block")
        .to_owned()
}

/// The names of the copies `stack.list` reports.
async fn listed_copies(client: &FrameworkClient) -> Vec<String> {
    structured(client, "stack.list").await["copies"]
        .as_array()
        .expect("copies")
        .iter()
        .map(|copy| copy["name"].as_str().expect("a name").to_owned())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_adds_and_removes_copies_through_a_daemon_target() {
    let stack = Stack::boot(true).await;
    stack.stage_framework_controls();
    write_robot_fragment(&stack, "recorder_robot", RECORDER_ROBOT_FRAGMENT);
    write_robot_fragment(&stack, "broken_robot", BROKEN_ROBOT_FRAGMENT);
    write_robot_fragment(&stack, "camera_robot", CAMERA_ROBOT_FRAGMENT);
    let port = ephemeral_port();
    stack
        .launch_launcher(&framework_launcher(port), Vec::new())
        .unwrap_or_else(|error| panic!("launch failed: {error:?}\n{}", stack.run_logs()));
    wait_for_port(port, || stack.run_logs()).await;
    let (client, mut progress) =
        connect_recording_progress(&endpoint(port, "/framework_controls/v1/mcp")).await;

    // --- The tools of the target, with the schema of `join` narrowed to
    // the scope.
    let tools = client.list_tools(None).await.expect("tools/list answers");
    let mut names: Vec<&str> = tools.tools.iter().map(|tool| tool.name.as_ref()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "stack.join",
            "stack.list",
            "stack.recent_calls",
            "stack.remove"
        ]
    );
    let join_schema = tools
        .tools
        .iter()
        .find(|tool| tool.name.as_ref() == "stack.join")
        .map(|tool| serde_json::to_value(&tool.input_schema).expect("schema serializes"))
        .expect("the tool is listed");
    assert_eq!(
        join_schema["properties"]["option"]["enum"],
        json!(["recorder_robot", "broken_robot"])
    );
    assert_eq!(
        structured(&client, "stack.list").await,
        json!({
            "options": [
                { "option": "recorder_robot", "description": "A robot that records episodes" },
                { "option": "broken_robot", "description": "A robot whose software cannot start" },
            ],
            "copies": [],
            "max_copies": 2,
        })
    );

    // --- Two copies join, each reporting its acceptance first, then the
    // lines of the join, the start of its instance included.
    for copy_name in ["alpha", "bravo"] {
        let joined = call(
            &client,
            "stack.join",
            json!({ "name": copy_name, "option": "recorder_robot" }),
        )
        .await;
        assert_eq!(
            joined.structured_content,
            Some(json!({
                "success": true,
                "message": format!("{copy_name} (recorder_robot) is on the stack"),
            })),
            "{joined:?}\n{}",
            stack.run_logs()
        );
        let accepted =
            format!("launch: the daemon accepted the addition of {copy_name} (recorder_robot)");
        let lines = progress_of(&mut progress, &accepted, |lines| {
            lines.iter().any(|line| line.starts_with("run: "))
        })
        .await;
        assert_eq!(lines[0], accepted, "the acceptance comes first: {lines:?}");
    }
    assert_eq!(listed_copies(&client).await, ["alpha", "bravo"]);
    assert_eq!(stack.daemon_copies().await, ["alpha", "bravo"]);

    // --- Refusals: the limit, a name in use (checked first), and an option
    // outside the scope, which the narrowed schema refuses.
    let at_limit = client
        .call_tool(
            CallToolRequestParams::new("stack.join").with_arguments(object(
                json!({ "name": "charlie", "option": "recorder_robot" }),
            )),
        )
        .await
        .expect("stack.join answers");
    assert_eq!(
        tool_error_text(&at_limit),
        "the action failed: the stack holds 2 copies of the options recorder_robot and \
         broken_robot (alpha, bravo), the most the scope of this endpoint allows; remove one \
         first"
    );
    let in_use = client
        .call_tool(
            CallToolRequestParams::new("stack.join").with_arguments(object(
                json!({ "name": "alpha", "option": "recorder_robot" }),
            )),
        )
        .await
        .expect("stack.join answers");
    assert_eq!(
        tool_error_text(&in_use),
        "the action failed: a copy `alpha` is already on the stack; choose another name"
    );
    let outside = client
        .call_tool(
            CallToolRequestParams::new("stack.join")
                .with_arguments(object(json!({ "name": "echo", "option": "camera_robot" }))),
        )
        .await
        .expect_err("an option outside the scope is outside the schema");
    assert_eq!(protocol_error(outside).code, ErrorCode::INVALID_PARAMS);

    // --- A removal reports each instance it stops.
    let removed = call(&client, "stack.remove", json!({ "name": "bravo" })).await;
    assert_eq!(
        removed.structured_content,
        Some(json!({ "success": true, "message": "bravo is off the stack" }))
    );
    let accepted = "launch: the daemon accepted the removal of bravo";
    let lines = progress_of(&mut progress, accepted, |lines| {
        lines
            .iter()
            .any(|line| line == "launch: Stopping instance `bravo_recorder` of copy `bravo`")
    })
    .await;
    assert_eq!(lines[0], accepted, "the acceptance comes first: {lines:?}");

    // --- A copy whose node fails its start is undone: no copy of it stays.
    let failed = call(
        &client,
        "stack.join",
        json!({ "name": "delta", "option": "broken_robot" }),
    )
    .await;
    let failure = failed
        .structured_content
        .clone()
        .unwrap_or_else(|| panic!("a join that ran answers its result: {failed:?}"));
    assert_eq!(failure["success"], false, "{failure}");
    let message = failure["message"].as_str().expect("a message");
    let (error, logs) = message
        .strip_prefix("the addition of delta failed: ")
        .and_then(|rest| rest.split_once(". Log files: "))
        .unwrap_or_else(|| panic!("the failure says what failed and names its logs: {message}"));
    assert!(!error.is_empty(), "{message}");
    let logs: Vec<&str> = logs.split(", ").collect();
    assert_eq!(
        logs.len(),
        2,
        "the stack log and the node's run log: {message}"
    );
    assert!(
        logs[1].ends_with("delta_recorder.log")
            && fs::read_to_string(logs[1])
                .unwrap_or_default()
                .contains("the recorder finds no camera"),
        "the second log is the run log of the instance that failed: {message}"
    );
    assert_eq!(listed_copies(&client).await, ["alpha"]);
    assert_eq!(stack.daemon_copies().await, ["alpha"]);

    // --- The call record: each change, each refusal by its kind.
    let record = structured(&client, "stack.recent_calls").await;
    let mut calls: Vec<(String, Value, Value)> = record["calls"]
        .as_array()
        .expect("calls")
        .iter()
        .map(|call| {
            (
                call["tool"].as_str().expect("a tool").to_owned(),
                call["outcome"].clone(),
                call["success"].clone(),
            )
        })
        .collect();
    calls.reverse();
    assert_eq!(
        calls,
        [
            ("stack.join".to_owned(), json!("completed"), json!(true)),
            ("stack.join".to_owned(), json!("completed"), json!(true)),
            ("stack.join".to_owned(), json!("failed"), Value::Null),
            ("stack.join".to_owned(), json!("failed"), Value::Null),
            ("stack.join".to_owned(), json!("refused"), Value::Null),
            ("stack.remove".to_owned(), json!("completed"), json!(true)),
            ("stack.join".to_owned(), json!("completed"), json!(false)),
        ],
        "{record}"
    );
    client.cancel().await.expect("the client closes");
}
