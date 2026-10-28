//! Harness rendering per pairing-slot cardinality, the Python twin of the
//! Rust backend's test: a `zero_or_one` slot gets a `<link>_vacant` kwarg
//! guarding only the peer-pin seeding (the mock still starts); a `one` slot
//! gets none; a multi slot gets a member count and an explicit member dict,
//! one mock per member.

use super::*;
use crate::generator::testgen::{
    DepLinkSpec, DepTopicSpec, PairingLinkSpec, TargetSpec, TestGenRegistry,
};
use config::node::{Cardinality, MessageFormat, NativeEmittedTopic, TopicRetention};
use std::path::Path;
use tempfile::TempDir;

fn registry_with_pairing(cardinality: Cardinality) -> TestGenRegistry {
    let mut registry = TestGenRegistry::default();
    registry.record_node_identity("relay_node", "v1");
    registry.pairings.insert(
        "backbone".to_string(),
        PairingLinkSpec {
            pairing_name: "joint_link".to_string(),
            pairing_tag: "v1".to_string(),
            cardinality,
            node_emits: Vec::new(),
            node_consumes: Vec::new(),
        },
    );
    registry
}

fn rendered_harness(registry: &TestGenRegistry) -> String {
    let node_dir = TempDir::new().unwrap();
    rendered_harness_in(PythonGenerator::new(), registry, node_dir.path())
}

fn rendered_harness_in(
    mut generator: PythonGenerator,
    registry: &TestGenRegistry,
    node_dir: &Path,
) -> String {
    super::super::fixtures::render(&mut generator, registry, node_dir).unwrap();
    generator
        .into_artifacts()
        .into_iter()
        .find(|artifact| artifact.module_path == vec!["harness".to_string()])
        .expect("fixtures::render emits the harness artifact")
        .code_output
}

#[test]
fn optional_pairing_slot_gets_a_vacant_kwarg_guarding_only_the_pin() {
    let rendered = rendered_harness(&registry_with_pairing(Cardinality::ZeroOrOne));
    assert_contains_all(
        &rendered,
        &[
            "backbone_vacant",
            "if not backbone_vacant:",
            "with_peer_pin(",
        ],
    );
    assert!(
        !rendered.contains("if backbone_vacant"),
        "the mock must start unconditionally; only the pin seeding is guarded"
    );
}

#[test]
fn multi_instance_dep_slot_gets_an_instance_id_override() {
    let mut registry = TestGenRegistry::default();
    registry.record_node_identity("relay_node", "v1");
    registry.deps.insert(
        "motor_health".to_string(),
        DepLinkSpec {
            producer_name: "motor_health".to_string(),
            target: TargetSpec::Contract {
                name: "motor_health".to_string(),
                tag: "v1".to_string(),
            },
            cardinality: Cardinality::ZeroOrMore,
            topics: Vec::new(),
            services: Vec::new(),
            actions: Vec::new(),
        },
    );
    let rendered = rendered_harness(&registry);
    assert_contains_all(
        &rendered,
        &[
            "motor_health_instances",
            "motor_health_instance_ids",
            "if motor_health_instance_ids is not None else",
        ],
    );
}

#[test]
fn every_harness_carries_the_daemon_clock_stand_in() {
    // The clock is node-invariant, so even a registry with no slots at all
    // gets the full surface: the kwarg, the stand-in it starts, the
    // standalone seeding, and the readiness-barrier entry.
    let mut registry = TestGenRegistry::default();
    registry.record_node_identity("relay_node", "v1");
    let rendered = rendered_harness(&registry);
    assert_contains_all(
        &rendered,
        &[
            "clock=\"wall\"",
            "MockClock.start(",
            "MOCK_CLOCK_INSTANCE_ID",
            "harness_clock,",
            ".with_clock(clock.binding())",
            "service_readiness = [clock.readiness()]",
            "clock=clock,",
        ],
    );
}

#[test]
fn multi_pairing_slot_gets_member_kwargs_and_one_mock_per_member() {
    let rendered = rendered_harness(&registry_with_pairing(Cardinality::ZeroOrMore));
    assert_contains_all(
        &rendered,
        &[
            "backbone_instances=0",
            "backbone_members=None",
            "for member_instance_id, _member_copy in pair_backbone_member_specs:",
            "Mock.start(router, instance_id, member_instance_id)",
            "for member_instance_id, member_copy in pair_backbone_member_specs:",
            "with_peer_pin_in_copy(",
            "with_peer_pin(",
        ],
    );
    assert!(
        !rendered.contains("backbone_vacant"),
        "a multi slot has no vacant boot; an empty member dict is its empty state"
    );

    let floored = rendered_harness(&registry_with_pairing(Cardinality::OneOrMore));
    assert!(
        floored.contains("backbone_instances=1"),
        "a one_or_more slot starts one mock peer by default"
    );
}

#[test]
fn required_pairing_slot_has_no_vacancy_kwarg() {
    let rendered = rendered_harness(&registry_with_pairing(Cardinality::One));
    assert!(rendered.contains("with_peer_pin("));
    assert!(
        !rendered.contains("backbone_vacant"),
        "a slot the deployment cannot leave unpaired must not offer a vacant boot"
    );
}

/// A dependency slot `camera` whose producer emits one topic per name.
fn registry_with_camera_topics(names: &[&str]) -> TestGenRegistry {
    let mut registry = TestGenRegistry::default();
    registry.record_node_identity("relay_node", "v1");
    registry.deps.insert(
        "camera".to_string(),
        DepLinkSpec {
            producer_name: "uvc_camera".to_string(),
            target: TargetSpec::Node {
                name: "uvc_camera".to_string(),
                tag: "v1".to_string(),
            },
            cardinality: Cardinality::One,
            topics: names
                .iter()
                .map(|name| DepTopicSpec {
                    name: (*name).to_string(),
                    module_link: "camera".to_string(),
                    format: MessageFormat::default(),
                    retention: TopicRetention::LiveOnly,
                })
                .collect(),
            services: Vec::new(),
            actions: Vec::new(),
        },
    );
    registry
}

#[test]
fn a_member_named_after_one_of_the_mock_s_own_bindings_takes_its_link_qualified_name() {
    let mut generator = PythonGenerator::new();
    super::super::mock::render(&mut generator, &registry_with_camera_topics(&["session"]))
        .expect("a member named after the mock's own `session` renders");
    let camera = generator
        .into_artifacts()
        .into_iter()
        .find(|artifact| artifact.module_path == ["deps", "camera"])
        .expect("the camera mock is rendered");
    assert!(
        camera
            .code_output
            .contains("self.camera_session = camera_session"),
        "the member is reached as `camera_session`:\n{}",
        camera.code_output
    );
    assert!(
        camera.code_output.contains("self._session = session"),
        "the mock keeps its own `session`:\n{}",
        camera.code_output
    );
}

#[test]
fn a_link_qualified_name_still_collides_with_a_member_that_carries_it() {
    let mut generator = PythonGenerator::new();
    let error = super::super::mock::render(
        &mut generator,
        &registry_with_camera_topics(&["session", "camera_session"]),
    )
    .expect_err("two members on one attribute must not render");
    assert!(
        matches!(
            &error,
            crate::error::Error::ModuleNameCollision { sanitized, first, second, .. }
                if sanitized == "camera_session"
                    && first == "camera/session"
                    && second == "camera/camera_session"
        ),
        "expected `session` and `camera_session` to collide on `camera_session`, got: {error}"
    );
}

/// The mock publisher of a consumed topic and the fixture subscription of an
/// emitted topic each declare their topic's retention.
#[test]
fn a_retaining_topic_reaches_its_mock_publisher_and_its_fixture_subscription() {
    let registry = TestGenRegistry::with_retaining_topics();
    const DECLARED: &str = "retention=peppylib.TopicRetention.latest(3),";

    let declares_in = |artifacts: Vec<InterfaceArtifact>, call: &str| {
        artifacts.iter().any(|artifact| {
            artifact.code_output.contains(call) && artifact.code_output.contains(DECLARED)
        })
    };

    let mut generator = PythonGenerator::new();
    super::super::mock::render(&mut generator, &registry).unwrap();
    assert!(
        declares_in(
            generator.into_artifacts(),
            "peppylib.testing.TestTopicPublisher.declare("
        ),
        "the mock publisher declares the retention"
    );

    let mut generator = PythonGenerator::new();
    let node_dir = TempDir::new().unwrap();
    super::super::fixtures::render(&mut generator, &registry, node_dir.path()).unwrap();
    assert!(
        declares_in(
            generator.into_artifacts(),
            "peppylib.TopicMessenger.subscribe("
        ),
        "the fixture subscription declares the retention"
    );
}

/// Production keys an emitted topic's schema on the raw topic name, so a
/// name that is a Rust keyword stays `loop`. The fixture subscription decodes
/// on that same key: both land on one capnp file instead of colliding on the
/// shared `loop_message` stem.
#[test]
fn an_emitted_topic_named_after_a_rust_keyword_shares_its_production_schema() {
    let mut generator = PythonGenerator::new();
    generator.set_node_identity("node", "v1");
    let topic: NativeEmittedTopic =
        serde_json5::from_str(r#"{ name: "loop", message_format: { x: "u8" } }"#).unwrap();
    generator.add_emitted_topic(&topic, None).unwrap();

    let registry = std::mem::take(&mut generator.testgen);
    let node_dir = TempDir::new().unwrap();
    let rendered = super::super::fixtures::render(&mut generator, &registry, node_dir.path());

    assert!(
        rendered.is_ok(),
        "the fixture codec must reuse the production schema, got: {rendered:?}"
    );
    assert_eq!(generator.schemas.len(), 1);
}

/// The baked node directory is the one arbitrary user string in the harness.
/// A path holding a combining character (an NFD name, common on macOS) must
/// still be a valid Python string literal: Rust's `{:?}` writes it as
/// `\u{301}`, an escape Python rejects with a SyntaxError.
#[test]
fn a_node_directory_with_a_combining_character_bakes_as_a_python_literal() {
    let mut registry = TestGenRegistry::default();
    registry.record_node_identity("relay_node", "v1");
    let temp = TempDir::new().unwrap();
    let node_dir = temp.path().join("jose\u{301}");
    std::fs::create_dir(&node_dir).unwrap();
    let canonical = std::fs::canonicalize(&node_dir).unwrap();

    let rendered = rendered_harness_in(PythonGenerator::new(), &registry, &node_dir);

    let expected = format!("_SYNC_TIME_NODE_DIR = \"{}\"", canonical.display());
    assert!(
        rendered.contains(&expected),
        "the raw path must be baked verbatim, expected `{expected}` in:\n{rendered}"
    );
    assert!(
        !rendered.contains("\\u{"),
        "no Rust-only `\\u{{...}}` escape may reach the Python source"
    );
}

/// A schema whose every leaf has a default, nested group included, bakes
/// the defaults as one dict literal in which the group is a nested dict.
#[test]
fn fully_defaulted_parameters_bake_a_nested_dict_literal() {
    let mut registry = TestGenRegistry::default();
    registry.record_node_identity("relay_node", "v1");
    let parameters: config::ParameterSchema = serde_json5::from_str(
        r#"{
            frame_rate: { $type: "u32", $default: 30 },
            calibration: {
                $type: "object",
                offset: { $type: "f64", $default: 0.5 },
                enabled: { $type: "bool", $default: true }
            }
        }"#,
    )
    .unwrap();
    let mut generator = PythonGenerator::new();
    generator.set_parameters(parameters);
    let node_dir = TempDir::new().unwrap();

    let rendered = rendered_harness_in(generator, &registry, node_dir.path());

    let expected = r#"_DEFAULT_PARAMETERS = {"calibration": {"enabled": True, "offset": 0.5}, "frame_rate": 30}"#;
    assert!(
        rendered.contains(expected),
        "expected `{expected}` in:\n{rendered}"
    );
}
