//! Harness Config rendering per pairing-slot cardinality: a `zero_or_one`
//! slot gets a `<link>_vacant` knob guarding only the peer-pin seeding (the
//! mock still starts, its pinned subscription resolves the readiness
//! barrier); a `one` slot gets none; a multi slot gets a member count and an
//! explicit member list, one mock per member. The generated harness lints
//! clean whether or not the node emits anything.

use super::*;
use crate::generator::testgen::{
    DepLinkSpec, DepTopicSpec, PairingLinkSpec, TargetSpec, TestGenRegistry,
};
use config::node::{Cardinality, MessageFormat, NativeEmittedTopic};

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
    let mut generator = RustGenerator::new();
    super::super::fixtures::render(&mut generator, registry).unwrap();
    generator
        .into_artifacts()
        .into_iter()
        .find(|artifact| artifact.module_path == vec!["harness".to_string()])
        .expect("fixtures::render emits the harness artifact")
        .code_output
}

#[test]
fn optional_pairing_slot_gets_a_vacant_knob_guarding_only_the_pin() {
    let rendered = rendered_harness(&registry_with_pairing(Cardinality::ZeroOrOne));
    assert_contains_all(
        &rendered,
        &[
            "pub backbone_vacant: bool",
            "backbone_vacant: false",
            "if !config.backbone_vacant",
            "with_peer_pin(",
        ],
    );
    assert!(
        !rendered.contains("if config.backbone_vacant"),
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
            "pub motor_health_instances: usize",
            "pub motor_health_instance_ids: Vec<String>",
            "motor_health_instance_ids: Vec::new()",
            "let motor_health_member_ids: Vec<String>",
            "config.motor_health_instance_ids",
        ],
    );
}

#[test]
fn every_harness_carries_the_daemon_clock_stand_in() {
    // The clock is node-invariant, so even a registry with no slots at all
    // gets the full surface: the knob on Config, the stand-in it starts, the
    // standalone seeding, and the readiness-barrier entry.
    let mut registry = TestGenRegistry::default();
    registry.record_node_identity("relay_node", "v1");
    let rendered = rendered_harness(&registry);
    assert_contains_all(
        &rendered,
        &[
            "pub clock: peppylib::testing::HarnessClock",
            "clock: peppylib::testing::HarnessClock::Wall",
            "MockClock::start(",
            "MOCK_CLOCK_INSTANCE_ID",
            "config.clock,",
            ".with_clock(clock.binding()?)",
            "service_readiness.push(clock.readiness()?);",
            "pub clock: peppylib::testing::MockClock",
        ],
    );
}

#[test]
fn multi_pairing_slot_gets_member_knobs_and_one_mock_per_member() {
    let rendered = rendered_harness(&registry_with_pairing(Cardinality::ZeroOrMore));
    assert_contains_all(
        &rendered,
        &[
            "pub struct PeerMemberSpec",
            "pub backbone_instances: usize",
            "backbone_instances: 0",
            "pub backbone_members: Vec<PeerMemberSpec>",
            "backbone_members: Vec::new()",
            "pub backbone: Vec<",
            "Mock::start_as(",
            "&member.instance_id,",
            "for member in &backbone_member_specs",
            "with_peer_pin_in_copy(",
            "with_peer_pin(",
        ],
    );
    assert!(
        !rendered.contains("backbone_vacant"),
        "a multi slot has no vacant boot; an empty member list is its empty state"
    );

    let floored = rendered_harness(&registry_with_pairing(Cardinality::OneOrMore));
    assert!(
        floored.contains("backbone_instances: 1"),
        "a one_or_more slot starts one mock peer by default"
    );
}

#[test]
fn required_pairing_slot_has_no_vacancy_knob() {
    let rendered = rendered_harness(&registry_with_pairing(Cardinality::One));
    assert!(rendered.contains("with_peer_pin("));
    assert!(
        !rendered.contains("backbone_vacant"),
        "a slot the deployment cannot leave unpaired must not offer a vacant boot"
    );
}

#[test]
fn a_member_shadowing_one_of_the_mock_s_own_bindings_is_a_hard_error() {
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
            topics: vec![DepTopicSpec {
                name: "session".to_string(),
                module_link: "camera".to_string(),
                format: MessageFormat::default(),
            }],
            services: Vec::new(),
            actions: Vec::new(),
        },
    );

    let mut generator = RustGenerator::new();
    let error = super::super::mock::render(&mut generator, &registry)
        .expect_err("a member named after the mock's own `session` must not render");
    assert!(
        matches!(
            &error,
            crate::error::Error::ModuleNameCollision { sanitized, second, .. }
                if sanitized == "session" && second == "camera/session"
        ),
        "expected a collision against the mock's own `session`, got: {error}"
    );
}

/// Generates the crate of a node under the stub manifest's identity, with
/// whatever surface `declare_surface` adds, and lints it the way a node's
/// test build compiles it (the `testing`-gated harness and mocks included).
fn assert_generated_node_lints_clean(declare_surface: impl FnOnce(&mut RustGenerator)) {
    let temp_dir = TempDir::new().unwrap();
    let (mut generator, output_dir, user_node, _) = init_test_env::<RustGenerator>(&temp_dir);
    generator.set_node_identity("generated_node", "v1");
    declare_surface(&mut generator);
    let output_config = copy_config_to_output(&user_node, &output_dir);
    generator
        .build(
            &output_dir,
            &daemon_config::consts::PeppyDirs::default(),
            Default::default(),
        )
        .unwrap();
    fs::remove_file(output_config).unwrap();

    run_clippy(&output_dir);
}

/// This is a long running test that verifies the generated code passes clippy.
/// A node emitting nothing has no publisher readiness to push and an empty
/// `Emitted`, so its harness declares the readiness list without `mut` and
/// leaves `Emitted` out of the shutdown teardown.
#[test]
fn harness_of_a_node_emitting_nothing_lints_clean() {
    assert_generated_node_lints_clean(|_| {});
}

/// This is a long running test that verifies the generated code passes clippy.
/// Every emitted topic pushes one publisher readiness entry into the list and
/// adds a subscription to `Emitted`, which shutdown drops ahead of the session.
#[test]
fn harness_of_a_node_emitting_a_topic_lints_clean() {
    assert_generated_node_lints_clean(|generator| {
        let status: NativeEmittedTopic = serde_json5::from_str(
            r#"{ name: "status", qos_profile: "reliable", message_format: { outcome: "string" } }"#,
        )
        .unwrap();
        generator.add_emitted_topic(&status, None).unwrap();
    });
}
