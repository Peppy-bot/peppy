//! What the Rust and the Python golden tests share: the node-config fixtures
//! that drive both generators, the registration of the scenarios both
//! renderers distinguish in the same way, the layout of a golden document, and
//! the comparison of a rendered document with its golden file.
//!
//! Both goldens drive their generator through [`LanguageGenerator`] with the
//! same config model, so a new interface shape lands here once and both
//! goldens render it.

use super::types::{
    ConsumedActionMessage, ContractOrigin, DependencyContext, InterfaceArtifact, LanguageGenerator,
    PeerContext,
};
use config::node::{
    Cardinality, ConsumedAction, ConsumedService, MessageFormat, NativeEmittedTopic,
    NativeExposedAction, NativeExposedService,
};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

// --- Fixtures for the maximal surface -------------------------------------

pub(crate) const EMITTED: &str = r#"
{
  name: "video_stream",
  qos_profile: "sensor_data",
  message_format: {
    header: { $type: "object", stamp: "time", frame_id: "u32" },
    encoding: "string",
    width: "u32",
    frame: { $type: "array", $items: "u8" }
  }
}
"#;

pub(crate) const EMITTED_NO_FORMAT: &str = r#"
{
  name: "heartbeat",
  qos_profile: "reliable"
}
"#;

pub(crate) const EXPOSED_SERVICE_FULL: &str = r#"
{
  name: "enable_camera",
  request_message_format: { enable: "bool" },
  response_message_format: {
    enabled: "bool",
    error_msg: { $type: "string", $optional: true }
  }
}
"#;

pub(crate) const EXPOSED_SERVICE_REQUEST_ONLY: &str = r#"
{
  name: "set_mode",
  request_message_format: { mode: "u8" }
}
"#;

pub(crate) const EXPOSED_SERVICE_RESPONSE_ONLY: &str = r#"
{
  name: "get_system_status",
  response_message_format: { healthy: "bool" }
}
"#;

pub(crate) const EXPOSED_SERVICE_BARE: &str = r#"
{
  name: "ping"
}
"#;

pub(crate) const EXPOSED_ACTION_FULL: &str = r#"
{
  name: "move_arm",
  goal_service: {
    request_message_format: {
      arm_id: "u16",
      desired_position: { $type: "array", $items: "i32", $length: 3 }
    },
    response_message_format: { accepted: "bool" }
  },
  feedback_topic: {
    qos_profile: "sensor_data",
    message_format: {
      new_position: { $type: "array", $items: "i32", $length: 3 }
    }
  },
  result_service: {
    response_message_format: {
      success: "bool",
      error_msg: { $type: "string", $optional: true }
    }
  }
}
"#;

pub(crate) const EXPOSED_ACTION_BARE: &str = r#"
{
  name: "home_all",
  goal_service: {},
  result_service: {}
}
"#;

pub(crate) const EXPOSED_ACTION_FEEDBACK_ONLY: &str = r#"
{
  name: "track_target",
  goal_service: {},
  feedback_topic: {
    qos_profile: "sensor_data",
    message_format: { bearing: "f32" }
  },
  result_service: {}
}
"#;

/// An action with no result service at all, the one exposed-action branch the
/// three above miss: `complete` / `complete_cancelled` are not generated.
pub(crate) const EXPOSED_ACTION_NO_RESULT: &str = r#"
{
  name: "nudge",
  goal_service: {}
}
"#;

pub(crate) const CONSUMED_TOPIC: &str = r#"
{
  link_id: "uvc_camera",
  name: "video_stream"
}
"#;

pub(crate) const CONSUMED_TOPIC_FORMAT: &str = r#"
{
  header: { $type: "object", stamp: "time", frame_id: "u32" },
  width: "u32"
}
"#;

pub(crate) const CONSUMED_SERVICE: &str = r#"
{
  link_id: "uvc_camera",
  name: "enable_camera"
}
"#;

pub(crate) const CONSUMED_SERVICE_REQUEST: &str = r#"
{
  enable: "bool"
}
"#;

pub(crate) const CONSUMED_SERVICE_RESPONSE: &str = r#"
{
  enabled: "bool",
  error_msg: { $type: "string", $optional: true }
}
"#;

pub(crate) const CONSUMED_ACTION: &str = r#"
{
  link_id: "left_arm",
  name: "move_arm"
}
"#;

const CONSUMED_ACTION_GOAL: &str = r#"
{
  arm_id: "u16",
  desired_position: { $type: "array", $items: "i32", $length: 3 }
}
"#;

const CONSUMED_ACTION_GOAL_RESPONSE: &str = r#"
{
  accepted: "bool"
}
"#;

const CONSUMED_ACTION_FEEDBACK: &str = r#"
{
  new_position: { $type: "array", $items: "i32", $length: 3 }
}
"#;

const CONSUMED_ACTION_RESULT: &str = r#"
{
  success: "bool",
  error_msg: { $type: "string", $optional: true }
}
"#;

pub(crate) const PEER_TOPIC: &str = r#"
{
  name: "joint_states",
  qos_profile: "sensor_data",
  message_format: {
    positions: { $type: "array", $items: "f64", $length: 3 },
    timestamp: "time"
  }
}
"#;

pub(crate) fn parse<T: serde::de::DeserializeOwned>(example: &str) -> T {
    serde_json5::from_str(example).unwrap()
}

pub(crate) fn contract_origin(link_id: &str) -> ContractOrigin {
    ContractOrigin {
        link_id: link_id.to_string(),
        contract_name: "camera_feed".to_string(),
        contract_tag: "v1".to_string(),
    }
}

pub(crate) fn peer(link_id: &str, cardinality: Cardinality) -> PeerContext {
    PeerContext {
        link_id: link_id.to_string(),
        pairing_name: "arm_link".to_string(),
        pairing_tag: "v1".to_string(),
        cardinality,
    }
}

/// The messages of a consumed action that declares a goal request, a goal
/// response, feedback and a result.
pub(crate) fn full_action_messages() -> ConsumedActionMessage {
    ConsumedActionMessage {
        goal_request: Some(parse(CONSUMED_ACTION_GOAL)),
        goal_response: Some(parse(CONSUMED_ACTION_GOAL_RESPONSE)),
        feedback: Some(parse(CONSUMED_ACTION_FEEDBACK)),
        result_response: Some(parse(CONSUMED_ACTION_RESULT)),
    }
}

// --- Scenarios both renderers distinguish the same way ----------------------

/// Registers the emitted topics of the node's own surface: one with a format,
/// manifest-rooted and contract-rooted, and one without a format.
pub(crate) fn register_own_emitted_topics(generator: &mut impl LanguageGenerator) {
    let emitted: NativeEmittedTopic = parse(EMITTED);
    generator.add_emitted_topic(&emitted, None).unwrap();
    generator
        .add_emitted_topic(&emitted, Some(&contract_origin("feed")))
        .unwrap();
    let heartbeat: NativeEmittedTopic = parse(EMITTED_NO_FORMAT);
    generator.add_emitted_topic(&heartbeat, None).unwrap();
}

/// Registers the exposed services and actions of the node's own surface: each
/// present/absent combination of request, response, goal, feedback and result
/// formats, manifest-rooted, then the full service and the full action
/// contract-rooted.
pub(crate) fn register_own_services_and_actions(generator: &mut impl LanguageGenerator) {
    for example in [
        EXPOSED_SERVICE_FULL,
        EXPOSED_SERVICE_REQUEST_ONLY,
        EXPOSED_SERVICE_RESPONSE_ONLY,
        EXPOSED_SERVICE_BARE,
    ] {
        let service: NativeExposedService = parse(example);
        generator.add_exposed_service(&service, None).unwrap();
    }
    let service: NativeExposedService = parse(EXPOSED_SERVICE_FULL);
    generator
        .add_exposed_service(&service, Some(&contract_origin("feed")))
        .unwrap();

    for example in [
        EXPOSED_ACTION_FULL,
        EXPOSED_ACTION_BARE,
        EXPOSED_ACTION_FEEDBACK_ONLY,
        EXPOSED_ACTION_NO_RESULT,
    ] {
        let action: NativeExposedAction = parse(example);
        generator.add_exposed_action(&action, None).unwrap();
    }
    let action: NativeExposedAction = parse(EXPOSED_ACTION_FULL);
    generator
        .add_exposed_action(&action, Some(&contract_origin("feed")))
        .unwrap();
}

/// Registers a dependency slot whose service and action declare no message
/// bodies at all. Every consumer-side renderer distinguishes "declared but
/// empty" from "carries a payload", and the maximal scenario only reaches the
/// second half.
pub(crate) fn register_bodyless_dependency(generator: &mut impl LanguageGenerator) {
    let dependency =
        DependencyContext::native("bare_producer", "v1", "bare_link", Cardinality::One);

    let mut service: ConsumedService = parse(CONSUMED_SERVICE);
    service.link_id = "bare_link".to_string();
    service.name = "ping".to_string();
    generator
        .add_consumed_service(
            &service,
            &MessageFormat::default(),
            &MessageFormat::default(),
            &dependency,
        )
        .unwrap();

    let mut action: ConsumedAction = parse(CONSUMED_ACTION);
    action.link_id = "bare_link".to_string();
    action.name = "home_all".to_string();
    generator
        .add_consumed_action(
            &action,
            &ConsumedActionMessage {
                goal_request: None,
                goal_response: None,
                feedback: None,
                result_response: None,
            },
            &dependency,
        )
        .unwrap();
}

// --- Golden documents --------------------------------------------------------

const SECTION_RULE: &str = "================================================================";

/// Appends one section of a golden document: the heading between two rules,
/// then the body and a blank line.
pub(crate) fn push_section(document: &mut String, heading: &str, body: &str) {
    writeln!(document, "{SECTION_RULE}\n{heading}\n{SECTION_RULE}").unwrap();
    document.push_str(body);
    document.push_str("\n\n");
}

/// Appends the section of one rendered artifact, headed by its kind and its
/// module path.
pub(crate) fn push_artifact_section(document: &mut String, artifact: &InterfaceArtifact) {
    let heading = format!(
        "{kind:?} :: {path}",
        kind = artifact.kind,
        path = artifact.module_path.join("/"),
    );
    push_section(document, &heading, &artifact.code_output);
}

/// A golden file of this crate and the command that regenerates it.
pub(crate) struct GoldenDocument {
    /// Path of the golden file, relative to the crate root.
    pub(crate) relative_path: &'static str,
    /// Environment variable that, when set, makes the test write the rendered
    /// document to the golden file instead of comparing the two.
    pub(crate) update_env_var: &'static str,
    /// The `cargo test` filter that runs the test of the golden file.
    pub(crate) test_filter: &'static str,
}

/// Asserts that `rendered` is byte-identical to the golden file, or writes
/// `rendered` to the golden file when the update variable of `golden` is set.
pub(crate) fn assert_matches_golden(golden: &GoldenDocument, rendered: &str) {
    let golden_path = Path::new(env!("CARGO_MANIFEST_DIR")).join(golden.relative_path);
    if std::env::var_os(golden.update_env_var).is_some() {
        fs::create_dir_all(golden_path.parent().unwrap()).unwrap();
        fs::write(&golden_path, rendered).unwrap();
        return;
    }

    let regenerate_command = format!(
        "{}=1 cargo test -p generator --lib {}",
        golden.update_env_var, golden.test_filter
    );
    let expected = fs::read_to_string(&golden_path).unwrap_or_else(|error| {
        panic!(
            "missing golden at {}: {error}\nregenerate with {regenerate_command}",
            golden_path.display()
        )
    });
    assert_eq!(
        rendered,
        expected,
        "the renderers' output drifted from the golden at {}; if the change was \
         intended, regenerate with {regenerate_command} and review the diff",
        golden_path.display()
    );
}
