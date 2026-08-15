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
const CAMERA_CONTRACT: &str = include_str!("fixtures/camera_observation/rgb_camera.contract.json5");

/// Two action shapes: `record_episode` carries every optional endpoint
/// (goal request, feedback, result data) and declares its feedback topic
/// `sensor_data`, so the golden pins both the data-bearing and the unit
/// `ResultOutcome` bridge codegen plus the contract-derived feedback QoS
/// (`SensorData` from the declaration, against `resume_session`'s
/// default for a feedback-less action).
const RECORDING_CONTRACT: &str =
    include_str!("fixtures/camera_observation/episode_recording.contract.json5");

/// The complete `mcp_exposure/v1` source document whose generated node is
/// committed under `goldens/camera_observation_mcp/`.
const CAMERA_EXPOSURE: &str = include_str!("fixtures/camera_observation/exposure.json5");

fn resolved(contract_json5: &str) -> ResolvedContractDocument {
    ResolvedContractDocument {
        sha256: ManifestFingerprint::of_bytes(contract_json5.as_bytes()),
        document: PeppyContractParser::from_content(contract_json5).expect("fixture parses"),
    }
}

fn sha_of(contract_json5: &str) -> String {
    ManifestFingerprint::of_bytes(contract_json5.as_bytes()).to_string()
}

fn generated_camera_node() -> GeneratedServerNode {
    let exposure = PeppyMcpExposureParser::from_content(CAMERA_EXPOSURE).expect("exposure parses");
    generate_exposure_node(
        &exposure,
        &[resolved(CAMERA_CONTRACT), resolved(RECORDING_CONTRACT)],
    )
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
    assert_eq!(depends_on.contracts.len(), 2);
    assert_eq!(depends_on.contracts[0].link_id, "front_camera");
    assert_eq!(
        depends_on.contracts[0].sha256.as_deref(),
        Some(sha_of(CAMERA_CONTRACT).as_str())
    );
    assert_eq!(depends_on.contracts[1].link_id, "recorder");
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
    let actions = config
        .interfaces
        .actions
        .expect("an actions section is declared");
    assert_eq!(actions.consumes.expect("actions are consumed").len(), 2);
}

#[test]
fn an_action_only_exposure_generates_the_node() {
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
    let node = generate_exposure_node(&exposure, &[resolved(RECORDING_CONTRACT)])
        .expect("an action-only exposure generates");
    assert_eq!(node.node_dir_name, "recording_mcp");
    let bridges = &node
        .files
        .iter()
        .find(|file| file.path == "src/bridges.rs")
        .expect("bridges are emitted")
        .content;
    assert!(
        bridges.contains("fire_goal") && bridges.contains("task_recorder_record_episode"),
        "the action bridge is emitted: {bridges}"
    );
    let main = &node
        .files
        .iter()
        .find(|file| file.path == "src/main.rs")
        .expect("main is emitted")
        .content;
    assert!(
        main.contains("with_task"),
        "the task is registered with the runtime: {main}"
    );
}

#[test]
fn an_invalid_exposure_reports_the_bundle_violations() {
    let exposure_json5 = CAMERA_EXPOSURE.replace("video_stream_info", "video_info");
    let exposure = PeppyMcpExposureParser::from_content(&exposure_json5).expect("exposure parses");
    // Both contracts resolve, so the renamed member is the only thing left
    // to complain about.
    let error = generate_exposure_node(
        &exposure,
        &[resolved(CAMERA_CONTRACT), resolved(RECORDING_CONTRACT)],
    )
    .expect_err("a missing member does not generate");
    assert_eq!(error.violations.len(), 1, "got: {:?}", error.violations);
    assert!(
        error.violations[0].contains("video_info"),
        "got: {:?}",
        error.violations
    );
}
