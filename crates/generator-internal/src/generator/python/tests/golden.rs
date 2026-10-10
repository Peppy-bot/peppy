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
//! and read the resulting diff: that diff is the change.

use super::*;
use crate::generator::golden_support::{
    CONSUMED_ACTION, CONSUMED_SERVICE, CONSUMED_SERVICE_REQUEST, CONSUMED_SERVICE_RESPONSE,
    CONSUMED_TOPIC, CONSUMED_TOPIC_FORMAT, GoldenDocument, PEER_TOPIC, assert_matches_golden,
    full_action_messages, parse, peer, push_artifact_section, register_bodyless_dependency,
    register_own_emitted_topics, register_own_services_and_actions,
};
use config::node::{
    Cardinality, ConsumedAction, ConsumedService, ConsumedTopic, MessageFormat, NativeEmittedTopic,
    TopicRetention,
};
use tempfile::TempDir;

const GOLDEN: GoldenDocument = GoldenDocument {
    relative_path: "src/generator/python/tests/golden/python_renderers.txt",
    update_env_var: "UPDATE_PYTHON_GOLDEN",
    test_filter: "python::tests::golden",
};

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

/// Drives the generator across every branch the renderers distinguish: both
/// origins (manifest and contract), every dependency/observer cardinality,
/// optional and required pairing slots, and each present/absent combination
/// of request, response, goal, feedback and result formats.
fn render_everything(node_dir: &Path) -> Vec<InterfaceArtifact> {
    let mut generator = PythonGenerator::new();
    generator.set_node_identity("relay_node", "v1");

    // Own surface, manifest-rooted and contract-rooted.
    register_own_emitted_topics(&mut generator);
    register_own_services_and_actions(&mut generator);

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
                TopicRetention::LiveOnly,
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
            &full_action_messages(),
            &DependencyContext::contract("arm_control", "v1", "left_arm", Cardinality::One),
        )
        .unwrap();

    // Pairing slots: required and optional, in both directions.
    let peer_topic: NativeEmittedTopic = parse(PEER_TOPIC);
    generator
        .add_peer_emitted_topic(&peer_topic, &peer("controller", Cardinality::One))
        .unwrap();
    generator
        .add_peer_consumed_topic(&peer_topic, &peer("sensor_bus", Cardinality::One))
        .unwrap();
    generator
        .add_peer_emitted_topic(
            &peer_topic,
            &peer("spare_controller", Cardinality::ZeroOrOne),
        )
        .unwrap();

    // Observer slots, one per cardinality.
    for (link_id, cardinality) in [
        ("watchers", Cardinality::One),
        ("spare_watcher", Cardinality::ZeroOrOne),
        ("watcher_bank", Cardinality::OneOrMore),
        ("watcher_pool", Cardinality::ZeroOrMore),
    ] {
        generator
            .add_observed_topic(&peer_topic, &peer(link_id, cardinality), cardinality)
            .unwrap();
    }

    let registry = std::mem::take(&mut generator.testgen);
    super::super::mock::render(&mut generator, &registry).unwrap();
    super::super::fixtures::render(&mut generator, &registry, node_dir).unwrap();
    generator.into_artifacts()
}

/// The surface of the dependency slot of [`register_bodyless_dependency`],
/// whose service and action declare no message bodies at all: the consumed
/// service drops its `request` parameter and its `Response`, the consumed
/// action drops its goal, feedback and result bodies, and the mock swaps
/// `captured`/`next_request -> Tuple` for `captured_count`/`next_request ->
/// Responder` and takes the no-argument `respond`/`enqueue_response`. No
/// format anywhere also means no capnp preamble, which nothing else in this
/// file renders.
fn render_bodyless_surface(node_dir: &Path) -> Vec<InterfaceArtifact> {
    let mut generator = PythonGenerator::new();
    generator.set_node_identity("relay_node", "v1");
    register_bodyless_dependency(&mut generator);

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

/// The harness of a tree staged for one build: it bakes no sync-time node
/// directory, so the same sources render the same bytes wherever the daemon
/// stages them.
fn render_staged_harness(node_dir: &Path) -> Vec<InterfaceArtifact> {
    let mut generator = PythonGenerator::new();
    generator.set_node_identity("staged_node", "v1");
    generator.set_node_tree(crate::generator::common::NodeTree::Staged);
    let registry = std::mem::take(&mut generator.testgen);
    super::super::fixtures::render(&mut generator, &registry, node_dir).unwrap();
    generator.into_artifacts()
}

/// The rendered surface as one reviewable document, with the sync-time node
/// directory (a per-run temp path) normalized away.
fn rendered_document(node_dir: &Path, artifacts: Vec<InterfaceArtifact>) -> String {
    let mut document = String::new();
    for artifact in &artifacts {
        push_artifact_section(&mut document, artifact);
    }
    let canonical = std::fs::canonicalize(node_dir)
        .unwrap_or_else(|_| node_dir.to_path_buf())
        .to_string_lossy()
        .into_owned();
    document.replace(&canonical, "<NODE_DIR>")
}

#[test]
fn python_renderers_emit_the_golden_document() {
    let node_dir = TempDir::new().unwrap();
    let mut artifacts = render_everything(node_dir.path());
    artifacts.extend(render_bodyless_surface(node_dir.path()));
    artifacts.extend(render_bare_harness(node_dir.path()));
    artifacts.extend(render_staged_harness(node_dir.path()));
    let rendered = rendered_document(node_dir.path(), artifacts);
    assert_matches_golden(&GOLDEN, &rendered);
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
