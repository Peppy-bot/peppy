//! Byte-exact golden of every Python renderer's pre-ruff output.
//!
//! The rest of this suite asserts on substrings, which is the right shape for
//! "this behaviour is present" but blind to everything around it: a renderer
//! could lose a blank line, shift an indent level, or drop a docstring and
//! every `assert_contains_all` would still pass. This test pins the whole
//! emitted surface instead, so a refactor of the emission layer (the
//! `PythonCodeBuilder` call sites) is provably output-preserving rather than
//! plausibly so.
//!
//! Regenerate after an intentional change:
//!
//! ```sh
//! UPDATE_PYTHON_GOLDEN=1 cargo test -p generator --lib python::tests::golden
//! ```
//!
//! and read the resulting diff — that diff is the change.

use super::*;
use crate::generator::types::{ConsumedActionMessage, ContractOrigin, PeerContext};
use config::node::{
    Cardinality, ConsumedAction, ConsumedService, ConsumedTopic, MessageFormat, NativeEmittedTopic,
    NativeExposedAction, NativeExposedService,
};
use std::fmt::Write as _;
use tempfile::TempDir;

const GOLDEN_PATH: &str = "src/generator/python/tests/golden/python_renderers.txt";

// --- Fixtures for the maximal surface -------------------------------------

const EMITTED: &str = r#"
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

const EMITTED_NO_FORMAT: &str = r#"
{
  name: "heartbeat",
  qos_profile: "reliable"
}
"#;

const EXPOSED_SERVICE_FULL: &str = r#"
{
  name: "enable_camera",
  request_message_format: { enable: "bool" },
  response_message_format: {
    enabled: "bool",
    error_msg: { $type: "string", $optional: true }
  }
}
"#;

const EXPOSED_SERVICE_REQUEST_ONLY: &str = r#"
{
  name: "set_mode",
  request_message_format: { mode: "u8" }
}
"#;

const EXPOSED_SERVICE_RESPONSE_ONLY: &str = r#"
{
  name: "get_system_status",
  response_message_format: { healthy: "bool" }
}
"#;

const EXPOSED_SERVICE_BARE: &str = r#"
{
  name: "ping"
}
"#;

const EXPOSED_ACTION_FULL: &str = r#"
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

const EXPOSED_ACTION_BARE: &str = r#"
{
  name: "home_all",
  goal_service: {},
  result_service: {}
}
"#;

const EXPOSED_ACTION_FEEDBACK_ONLY: &str = r#"
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
const EXPOSED_ACTION_NO_RESULT: &str = r#"
{
  name: "nudge",
  goal_service: {}
}
"#;

const CONSUMED_TOPIC: &str = r#"
{
  link_id: "uvc_camera",
  name: "video_stream"
}
"#;

const CONSUMED_TOPIC_FORMAT: &str = r#"
{
  header: { $type: "object", stamp: "time", frame_id: "u32" },
  width: "u32"
}
"#;

const CONSUMED_SERVICE: &str = r#"
{
  link_id: "uvc_camera",
  name: "enable_camera"
}
"#;

const CONSUMED_SERVICE_REQUEST: &str = r#"
{
  enable: "bool"
}
"#;

const CONSUMED_SERVICE_RESPONSE: &str = r#"
{
  enabled: "bool",
  error_msg: { $type: "string", $optional: true }
}
"#;

const CONSUMED_ACTION: &str = r#"
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

const PEER_TOPIC: &str = r#"
{
  name: "joint_states",
  qos_profile: "sensor_data",
  message_format: {
    positions: { $type: "array", $items: "f64", $length: 3 },
    timestamp: "time"
  }
}
"#;

/// A schema whose reachable leaves are not all defaulted, so the harness
/// takes the `_DEFAULT_PARAMETERS = None` branch and defers to the node's own
/// boot validation. The defaulted sibling keeps the literal path exercised in
/// the main scenario.
const PARAMETERS_WITH_A_MISSING_DEFAULT: &str = r#"
{
  frame_rate: { $type: "u32", $default: 30 },
  calibration: { $type: "object", offset: { $type: "f64" } }
}
"#;

fn parse<T: serde::de::DeserializeOwned>(example: &str) -> T {
    serde_json5::from_str(example).unwrap()
}

fn contract_origin(link_id: &str) -> ContractOrigin {
    ContractOrigin {
        link_id: link_id.to_string(),
        contract_name: "camera_feed".to_string(),
        contract_tag: "v1".to_string(),
    }
}

fn peer(link_id: &str, optional: bool) -> PeerContext {
    PeerContext {
        link_id: link_id.to_string(),
        pairing_name: "arm_link".to_string(),
        pairing_tag: "v1".to_string(),
        optional,
    }
}

/// Drives the generator across every branch the renderers distinguish: both
/// origins (manifest and contract), every dependency/observer cardinality,
/// optional and required pairing slots, and each present/absent combination
/// of request, response, goal, feedback and result formats.
fn render_everything(node_dir: &Path) -> Vec<InterfaceArtifact> {
    let mut generator = PythonGenerator::new();
    generator.set_node_identity("relay_node", "v1");

    // Own surface, manifest-rooted and contract-rooted.
    let emitted: NativeEmittedTopic = parse(EMITTED);
    generator.add_emitted_topic(&emitted, None).unwrap();
    generator
        .add_emitted_topic(&emitted, Some(&contract_origin("feed")))
        .unwrap();
    let heartbeat: NativeEmittedTopic = parse(EMITTED_NO_FORMAT);
    generator.add_emitted_topic(&heartbeat, None).unwrap();

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

    // Dependency slots, one per cardinality, each carrying every interface
    // kind so the mock and the harness both render their full shape.
    for (link_id, cardinality) in [
        ("uvc_camera", Cardinality::One),
        ("spare_camera", Cardinality::ZeroOrOne),
        ("camera_bank", Cardinality::OneOrMore),
        ("camera_pool", Cardinality::ZeroOrMore),
    ] {
        let mut topic: ConsumedTopic = parse(CONSUMED_TOPIC);
        topic.link_id = link_id.to_string();
        generator
            .add_consumed_topic(
                &topic,
                parse(CONSUMED_TOPIC_FORMAT),
                &DependencyContext::native("uvc_camera", "v1", link_id, cardinality),
            )
            .unwrap();

        let mut service: ConsumedService = parse(CONSUMED_SERVICE);
        service.link_id = link_id.to_string();
        generator
            .add_consumed_service(
                &service,
                &parse::<MessageFormat>(CONSUMED_SERVICE_REQUEST),
                &parse::<MessageFormat>(CONSUMED_SERVICE_RESPONSE),
                &DependencyContext::native("uvc_camera", "v1", link_id, cardinality),
            )
            .unwrap();
    }

    // A contract-routed dependency with an action, exercising the other
    // target flavour on the mock side.
    let mut action: ConsumedAction = parse(CONSUMED_ACTION);
    action.link_id = "left_arm".to_string();
    generator
        .add_consumed_action(
            &action,
            &ConsumedActionMessage {
                goal_request: Some(parse(CONSUMED_ACTION_GOAL)),
                goal_response: Some(parse(CONSUMED_ACTION_GOAL_RESPONSE)),
                feedback: Some(parse(CONSUMED_ACTION_FEEDBACK)),
                result_response: Some(parse(CONSUMED_ACTION_RESULT)),
            },
            &DependencyContext::contract("arm_control", "v1", "left_arm", Cardinality::One),
        )
        .unwrap();

    // Pairing slots: required and optional, in both directions.
    let peer_topic: NativeEmittedTopic = parse(PEER_TOPIC);
    generator
        .add_peer_emitted_topic(&peer_topic, &peer("controller", false))
        .unwrap();
    generator
        .add_peer_consumed_topic(&peer_topic, &peer("sensor_bus", false))
        .unwrap();
    generator
        .add_peer_emitted_topic(&peer_topic, &peer("spare_controller", true))
        .unwrap();

    // Observer slots, one per cardinality.
    for (link_id, cardinality) in [
        ("watchers", Cardinality::One),
        ("spare_watcher", Cardinality::ZeroOrOne),
        ("watcher_bank", Cardinality::OneOrMore),
        ("watcher_pool", Cardinality::ZeroOrMore),
    ] {
        generator
            .add_observed_topic(&peer_topic, &peer(link_id, false), cardinality)
            .unwrap();
    }

    let registry = std::mem::take(&mut generator.testgen);
    super::super::mock::render(&mut generator, &registry).unwrap();
    super::super::fixtures::render(&mut generator, &registry, node_dir).unwrap();
    generator.into_artifacts()
}

/// A dependency slot whose service and action declare no message bodies at
/// all. Every consumer-side renderer distinguishes "declared but empty" from
/// "carries a payload", and the maximal scenario above only ever reaches the
/// second half: here the consumed service drops its `request` parameter and
/// its `Response`, the consumed action drops its goal, feedback and result
/// bodies, and the mock swaps `captured`/`next_request -> Tuple` for
/// `captured_count`/`next_request -> Responder` and takes the no-argument
/// `respond`/`enqueue_response`. No format anywhere also means no capnp
/// preamble, which nothing else in this file renders.
fn render_bodyless_surface(node_dir: &Path) -> Vec<InterfaceArtifact> {
    let mut generator = PythonGenerator::new();
    generator.set_node_identity("relay_node", "v1");
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

    let registry = std::mem::take(&mut generator.testgen);
    super::super::mock::render(&mut generator, &registry).unwrap();
    super::super::fixtures::render(&mut generator, &registry, node_dir).unwrap();
    generator.into_artifacts()
}

/// The two harness branches no dependency-bearing node reaches: a node with
/// no slots and no own surface at all, and a parameter schema with an
/// undefaulted leaf.
fn render_bare_harness(node_dir: &Path) -> Vec<InterfaceArtifact> {
    let mut generator = PythonGenerator::new();
    generator.set_node_identity("bare_node", "v1");
    generator.set_parameters(parse(PARAMETERS_WITH_A_MISSING_DEFAULT));
    let registry = std::mem::take(&mut generator.testgen);
    super::super::fixtures::render(&mut generator, &registry, node_dir).unwrap();
    generator.into_artifacts()
}

/// The rendered surface as one reviewable document, with the sync-time node
/// directory (a per-run temp path) normalized away.
fn rendered_document(node_dir: &Path, artifacts: Vec<InterfaceArtifact>) -> String {
    let mut out = String::new();
    for artifact in artifacts {
        writeln!(
            out,
            "================================================================\n\
             {kind:?} :: {path}\n\
             ================================================================",
            kind = artifact.kind,
            path = artifact.module_path.join("/"),
        )
        .unwrap();
        out.push_str(&artifact.code_output);
        out.push_str("\n\n");
    }
    let canonical = std::fs::canonicalize(node_dir)
        .unwrap_or_else(|_| node_dir.to_path_buf())
        .to_string_lossy()
        .into_owned();
    out.replace(&canonical, "<NODE_DIR>")
}

#[test]
fn python_renderers_emit_the_golden_document() {
    let node_dir = TempDir::new().unwrap();
    let mut artifacts = render_everything(node_dir.path());
    artifacts.extend(render_bodyless_surface(node_dir.path()));
    artifacts.extend(render_bare_harness(node_dir.path()));
    let rendered = rendered_document(node_dir.path(), artifacts);

    let golden_path = Path::new(env!("CARGO_MANIFEST_DIR")).join(GOLDEN_PATH);
    if std::env::var_os("UPDATE_PYTHON_GOLDEN").is_some() {
        std::fs::create_dir_all(golden_path.parent().unwrap()).unwrap();
        std::fs::write(&golden_path, &rendered).unwrap();
        return;
    }

    let expected = std::fs::read_to_string(&golden_path).unwrap_or_else(|error| {
        panic!(
            "missing golden at {}: {error}\nregenerate with \
             UPDATE_PYTHON_GOLDEN=1 cargo test -p generator --lib python::tests::golden",
            golden_path.display()
        )
    });
    assert_eq!(
        rendered, expected,
        "the Python renderers' output drifted from the golden; if the change was \
         intended, regenerate with UPDATE_PYTHON_GOLDEN=1 and review the diff"
    );
}

/// No emitted line carries trailing whitespace. Standing on its own because
/// it is the one property a raw-string emission layer could lose silently:
/// an editor that trims on save would rewrite the generated Python, and the
/// golden alone would only catch it after the fact.
#[test]
fn no_emitted_line_has_trailing_whitespace() {
    let node_dir = TempDir::new().unwrap();
    let mut artifacts = render_everything(node_dir.path());
    artifacts.extend(render_bodyless_surface(node_dir.path()));
    artifacts.extend(render_bare_harness(node_dir.path()));
    for artifact in artifacts {
        for (index, line) in artifact.code_output.lines().enumerate() {
            assert_eq!(
                line.trim_end(),
                line,
                "{:?} {} line {} carries trailing whitespace: {line:?}",
                artifact.kind,
                artifact.module_path.join("/"),
                index + 1,
            );
        }
    }
}
