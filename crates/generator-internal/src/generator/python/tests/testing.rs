//! Harness rendering for pairing-slot vacancy, the Python twin of the Rust
//! backend's test: an optional slot gets a `<link>_vacant` kwarg guarding
//! only the peer-pin seeding (the mock still starts); a required slot gets
//! none.

use super::*;
use crate::generator::testgen::{
    DepLinkSpec, DepTopicSpec, PairingLinkSpec, TargetSpec, TestGenRegistry,
};
use config::node::{Cardinality, MessageFormat};
use tempfile::TempDir;

fn registry_with_pairing(optional: bool) -> TestGenRegistry {
    let mut registry = TestGenRegistry::default();
    registry.record_node_identity("relay_node", "v1");
    registry.pairings.insert(
        "backbone".to_string(),
        PairingLinkSpec {
            pairing_name: "joint_link".to_string(),
            pairing_tag: "v1".to_string(),
            optional,
            node_emits: Vec::new(),
            node_consumes: Vec::new(),
        },
    );
    registry
}

fn rendered_harness(registry: &TestGenRegistry) -> String {
    let mut generator = PythonGenerator::new();
    let node_dir = TempDir::new().unwrap();
    super::super::fixtures::render(&mut generator, registry, node_dir.path()).unwrap();
    generator
        .into_artifacts()
        .into_iter()
        .find(|artifact| artifact.module_path == vec!["harness".to_string()])
        .expect("fixtures::render emits the harness artifact")
        .code_output
}

#[test]
fn optional_pairing_slot_gets_a_vacant_kwarg_guarding_only_the_pin() {
    let rendered = rendered_harness(&registry_with_pairing(true));
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
    // gets the full surface: the sim kwarg, both start modes, the standalone
    // seeding, and the readiness-barrier entry.
    let mut registry = TestGenRegistry::default();
    registry.record_node_identity("relay_node", "v1");
    let rendered = rendered_harness(&registry);
    assert_contains_all(
        &rendered,
        &[
            "use_sim_time=False",
            "if use_sim_time:",
            "MockClock.start_sim(",
            "MockClock.start_wall(",
            "MOCK_CLOCK_INSTANCE_ID",
            ".with_use_sim_time(use_sim_time)",
            "service_readiness = [clock.readiness()]",
            "clock=clock,",
        ],
    );
}

#[test]
fn required_pairing_slot_has_no_vacancy_kwarg() {
    let rendered = rendered_harness(&registry_with_pairing(false));
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

    let mut generator = PythonGenerator::new();
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
