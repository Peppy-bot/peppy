//! Harness Config rendering for pairing-slot vacancy: an optional slot gets
//! a `<link>_vacant` knob guarding only the peer-pin seeding (the mock still
//! starts — its pinned subscription resolves the readiness barrier); a
//! required slot gets none.

use super::*;
use crate::generator::testgen::{PairingLinkSpec, TestGenRegistry};

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
    let rendered = rendered_harness(&registry_with_pairing(true));
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
fn required_pairing_slot_has_no_vacancy_knob() {
    let rendered = rendered_harness(&registry_with_pairing(false));
    assert!(rendered.contains("with_peer_pin("));
    assert!(
        !rendered.contains("backbone_vacant"),
        "a slot the deployment cannot leave unpaired must not offer a vacant boot"
    );
}
