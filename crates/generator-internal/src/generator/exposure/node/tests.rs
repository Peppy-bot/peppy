use super::super::validate::ResolvedContractDocument;
use super::{GeneratedServerNode, generate_exposure_node};
use daemon_config::contract::PeppyContractParser;
use daemon_config::mcp_exposure::PeppyMcpExposureParser;
use daemon_config::repository::ManifestFingerprint;
use std::path::Path;

/// A camera contract exercising every `message_format` shape the canonical
/// mapping accepts, so the golden node pins the bridge codegen for the whole
/// DSL: scalars, decimal-string integers, time, bytes, optional pointer
/// fields, fixed and variable arrays (including both `u8` renderings),
/// nested objects, and arrays of objects.
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
                    stamp: "time",
                },
            },
            {
                name: "camera_status",
                message_format: {
                    battery: "u8",
                    temperature_c: "f32",
                    frames_captured: "u64",
                    clock_drift_ns: "i64",
                    recording: "bool",
                    note: { $type: "string", $optional: true },
                    calibrated_at: "time",
                    checksum: { $type: "array", $items: "u8", $length: 4 },
                    gains: { $type: "array", $items: "f32", $length: 3 },
                    tags: { $type: "array", $items: "string" },
                    pose: { $type: "object", x_m: "f64", y_m: "f64" },
                    samples: {
                        $type: "array",
                        $items: { $type: "object", offset: "i16", value: "f64" },
                    },
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
                name: "calibrate",
                request_message_format: {
                    exposure_us: "u32",
                    offsets: { $type: "array", $items: "i16", $length: 2 },
                    profile: {
                        $type: "object",
                        gamma: "f64",
                        white_balance: { $type: "object", red: "f32", blue: "f32" },
                    },
                    comment: { $type: "string", $optional: true },
                },
            },
            { name: "ping" },
        ],
    },
}"#;

const RECORDING_CONTRACT: &str = r#"{
    peppy_schema: "contract/v1",
    manifest: { name: "episode_recording", tag: "v1" },
    interfaces: {
        actions: [
            {
                name: "record_episode",
                goal_service: {
                    request_message_format: { task_description: "string" },
                },
                result_service: {
                    response_message_format: { episode_index: "u32" },
                },
            },
        ],
    },
}"#;

fn resolved(contract_json5: &str) -> ResolvedContractDocument {
    ResolvedContractDocument {
        sha256: ManifestFingerprint::of_bytes(contract_json5.as_bytes()),
        document: PeppyContractParser::from_content(contract_json5).expect("fixture parses"),
    }
}

fn sha_of(contract_json5: &str) -> String {
    ManifestFingerprint::of_bytes(contract_json5.as_bytes()).to_string()
}

/// A camera-only surface covering both resource shapes (image-represented
/// and plain telemetry) and all four service shapes (response-only,
/// request+response, request-only, and bare).
fn camera_exposure() -> String {
    format!(
        r#"{{
        peppy_schema: "mcp_exposure/v1",
        manifest: {{ name: "camera_observation", tag: "v1" }},
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
                        freshness: {{ max_age_ms: 2000 }},
                        update: {{ max_hz: 2 }},
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
                        freshness: {{ max_age_ms: 5000 }},
                        update: {{ max_hz: 1 }},
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
                        deadline_ms: 2000,
                    }},
                    {{
                        member: "set_brightness",
                        tool: "front_camera.set_brightness",
                        description: "Set the camera brightness in device units.",
                        operation: "mutating",
                        deadline_ms: 2000,
                        restrict: {{ value: {{ min: -64, max: 64 }} }},
                    }},
                    {{
                        member: "calibrate",
                        tool: "front_camera.calibrate",
                        description: "Recalibrate the camera color profile.",
                        operation: "mutating",
                        deadline_ms: 10000,
                    }},
                    {{
                        member: "ping",
                        tool: "front_camera.ping",
                        description: "Check that the camera answers at all.",
                        operation: "read_only",
                        deadline_ms: 1000,
                    }},
                ],
            }},
        }},
    }}"#,
        camera_sha = sha_of(CAMERA_CONTRACT),
    )
}

fn generated_camera_node() -> GeneratedServerNode {
    let exposure =
        PeppyMcpExposureParser::from_content(&camera_exposure()).expect("exposure parses");
    generate_exposure_node(&exposure, &[resolved(CAMERA_CONTRACT)])
        .expect("the camera exposure generates")
}

/// The committed goldens under `goldens/camera_observation_mcp/`, keyed by
/// the generated path. `/` becomes `__` and the `.gitignore` loses its dot
/// so the golden itself is not an active ignore file. Regenerate with
/// `UPDATE_EXPOSURE_GOLDENS=1 cargo test -p generator --lib exposure` and
/// review the diff before committing.
const GOLDENS: &[(&str, &str)] = &[
    (
        "peppy.json5",
        include_str!("goldens/camera_observation_mcp/peppy.json5"),
    ),
    (
        "Cargo.toml",
        include_str!("goldens/camera_observation_mcp/Cargo.toml"),
    ),
    (
        ".gitignore",
        include_str!("goldens/camera_observation_mcp/gitignore"),
    ),
    (
        "src/main.rs",
        include_str!("goldens/camera_observation_mcp/src__main.rs"),
    ),
    (
        "src/bridges.rs",
        include_str!("goldens/camera_observation_mcp/src__bridges.rs"),
    ),
    (
        "src/bundle.json",
        include_str!("goldens/camera_observation_mcp/src__bundle.json"),
    ),
];

fn golden_file_name(path: &str) -> String {
    path.replace('/', "__").replace(".gitignore", "gitignore")
}

#[test]
fn golden_node_matches_committed_output() {
    let node = generated_camera_node();
    assert_eq!(node.node_dir_name, "camera_observation_mcp");

    let update = std::env::var_os("UPDATE_EXPOSURE_GOLDENS").is_some();
    if update {
        for file in &node.files {
            let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
                "src/generator/exposure/node/goldens/camera_observation_mcp/{}",
                golden_file_name(&file.path)
            ));
            std::fs::write(&path, &file.content).expect("write golden");
        }
        return;
    }

    let generated_paths: Vec<&str> = node.files.iter().map(|file| file.path.as_str()).collect();
    let golden_paths: Vec<&str> = GOLDENS.iter().map(|(path, _)| *path).collect();
    assert_eq!(
        generated_paths, golden_paths,
        "the generated file set changed"
    );

    for (path, committed) in GOLDENS {
        let generated = node
            .files
            .iter()
            .find(|file| file.path == *path)
            .expect("path checked above");
        assert_eq!(
            &generated.content, committed,
            "generated `{path}` diverged from its golden; regenerate with \
             UPDATE_EXPOSURE_GOLDENS=1 and review the diff"
        );
    }
}

#[test]
fn the_generated_peppy_json5_parses_as_a_node_config() {
    let node = generated_camera_node();
    let peppy_json5 = &node
        .files
        .iter()
        .find(|file| file.path == "peppy.json5")
        .expect("the node carries a peppy.json5")
        .content;
    let config: config::node::NodeConfig =
        serde_json5::from_str(peppy_json5).expect("the generated document parses as node/v1");

    assert_eq!(config.manifest.name.as_str(), "camera_observation_mcp");
    assert_eq!(config.manifest.tag, "v1");
    let depends_on = config
        .manifest
        .depends_on
        .expect("contract slots are declared");
    assert_eq!(depends_on.contracts.len(), 1);
    assert_eq!(depends_on.contracts[0].link_id, "front_camera");
    assert_eq!(
        depends_on.contracts[0].sha256.as_deref(),
        Some(sha_of(CAMERA_CONTRACT).as_str())
    );
    let topics = config
        .interfaces
        .topics
        .expect("a topics section is declared");
    assert_eq!(topics.consumes.expect("topics are consumed").len(), 2);
    let services = config
        .interfaces
        .services
        .expect("a services section is declared");
    assert_eq!(services.consumes.expect("services are consumed").len(), 4);
}

#[test]
fn exposures_selecting_actions_are_refused() {
    let exposure_json5 = format!(
        r#"{{
        peppy_schema: "mcp_exposure/v1",
        manifest: {{ name: "recording", tag: "v1" }},
        server: {{ title: "Recorder" }},
        targets: {{
            recorder: {{
                contract: {{ name: "episode_recording", tag: "v1", sha256: "{sha}" }},
                actions: [
                    {{
                        member: "record_episode",
                        tool: "recorder.record_episode",
                        description: "Record one teleoperation episode.",
                        operation: "long_running",
                        safety_sensitive: true,
                        confirmation_required: true,
                        deadline_ms: 900000,
                    }},
                ],
            }},
        }},
    }}"#,
        sha = sha_of(RECORDING_CONTRACT),
    );
    let exposure = PeppyMcpExposureParser::from_content(&exposure_json5).expect("exposure parses");
    let error = generate_exposure_node(&exposure, &[resolved(RECORDING_CONTRACT)])
        .expect_err("actions are not supported yet");
    assert_eq!(error.violations.len(), 1);
    assert!(
        error.violations[0].contains("does not support action-backed tasks yet"),
        "got: {}",
        error.violations[0]
    );
}

#[test]
fn an_invalid_exposure_reports_the_bundle_violations() {
    let exposure_json5 = camera_exposure().replace("video_stream_info", "video_info");
    let exposure = PeppyMcpExposureParser::from_content(&exposure_json5).expect("exposure parses");
    let error = generate_exposure_node(&exposure, &[resolved(CAMERA_CONTRACT)])
        .expect_err("a missing member does not generate");
    assert!(
        error
            .violations
            .iter()
            .any(|violation| violation.contains("video_info")),
        "got: {:?}",
        error.violations
    );
}
