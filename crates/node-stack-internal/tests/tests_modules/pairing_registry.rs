//! Tests for the stack's pairing registry: `pair_slots` validation,
//! exclusivity, clear, dissolve-on-death, reset, the serialized-graph
//! overlay, and the DAG-invisibility of `depends_on.pairings`.

use std::path::PathBuf;

use config::runtime::{Name, PairingSlotBinding};
use node_stack::{NodeStack, NodeStackError, SlotAddr};

/// The core-node name of the test stack's root entity. Slot addresses are
/// core-node-qualified, and every instance in these tests lives on this one
/// daemon.
const TEST_CORE_NODE: &str = "core";

use crate::helpers::config_common::core_node_config;
use crate::helpers::fixtures;
use crate::helpers::real_lifecycle;

fn robot_arm_config() -> config::node::NodeConfig {
    serde_json5::from_str(
        r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "robot_arm",
                tag: "v1",
                depends_on: {
                    pairings: [
                        { name: "arm_link", tag: "v1", role: "arm", link_id: "controller", optional: true }
                    ]
                }
            },
            execution: { language: "rust", run_cmd: ["arm"] }
        }"#,
    )
    .expect("valid robot_arm config")
}

fn arm_controller_config() -> config::node::NodeConfig {
    serde_json5::from_str(
        r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "arm_controller",
                tag: "v1",
                depends_on: {
                    pairings: [
                        { name: "arm_link", tag: "v1", role: "controller", link_id: "arm" }
                    ]
                }
            },
            execution: { language: "rust", run_cmd: ["controller"] }
        }"#,
    )
    .expect("valid arm_controller config")
}

fn name(s: &str) -> Name {
    Name::new(s).expect("valid test name")
}

#[tokio::test]
async fn pair_clear_repair_lifecycle() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let harness = real_lifecycle::lifecycle_harness();
    let _arm =
        fixtures::push_started(&stack, &harness, robot_arm_config(), Some(&name("arm_1"))).await;
    let _ctrl = fixtures::push_started(
        &stack,
        &harness,
        arm_controller_config(),
        Some(&name("ctrl_1")),
    )
    .await;

    let arm_slot = SlotAddr::new(TEST_CORE_NODE, "arm_1", "controller");
    let ctrl_slot = SlotAddr::new(TEST_CORE_NODE, "ctrl_1", "arm");

    // Both slots start unpaired.
    let unpaired = stack.unpaired_pairing_slots();
    assert_eq!(unpaired.len(), 2, "unpaired: {unpaired:?}");

    let pairing = stack
        .pair_slots(&ctrl_slot, &arm_slot)
        .expect("complementary slots should pair");
    assert_eq!(pairing.pairing_name, "arm_link");
    assert_eq!(pairing.a.role, "controller");
    assert_eq!(pairing.b.role, "arm");
    assert_eq!(stack.pairs().len(), 1);
    assert!(stack.unpaired_pairing_slots().is_empty());

    // Exclusivity: a second pair on either slot is rejected.
    let err = stack.pair_slots(&ctrl_slot, &arm_slot).unwrap_err();
    assert!(
        matches!(err, NodeStackError::PairingSlotAlreadyPaired { .. }),
        "expected PairingSlotAlreadyPaired, got {err:?}"
    );

    // Clear releases both slots; re-pairing works.
    let cleared = stack.clear_pair(&arm_slot).expect("pair should exist");
    assert_eq!(cleared, pairing);
    assert!(stack.pairs().is_empty());
    stack
        .pair_slots(&arm_slot, &ctrl_slot)
        .expect("cleared slots should re-pair (argument order is irrelevant)");
    assert_eq!(stack.pairs().len(), 1);
}

#[tokio::test]
async fn pair_slots_validation_matrix() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let harness = real_lifecycle::lifecycle_harness();
    let _arm =
        fixtures::push_started(&stack, &harness, robot_arm_config(), Some(&name("arm_1"))).await;
    let _ctrl = fixtures::push_started(
        &stack,
        &harness,
        arm_controller_config(),
        Some(&name("ctrl_1")),
    )
    .await;

    // Unknown instance.
    let err = stack
        .pair_slots(
            &SlotAddr::new(TEST_CORE_NODE, "ghost", "arm"),
            &SlotAddr::new(TEST_CORE_NODE, "arm_1", "controller"),
        )
        .unwrap_err();
    assert!(
        matches!(err, NodeStackError::PairingInstanceNotRunning { ref instance_id } if instance_id == "ghost"),
        "expected PairingInstanceNotRunning, got {err:?}"
    );

    // Known instance, undeclared slot.
    let err = stack
        .pair_slots(
            &SlotAddr::new(TEST_CORE_NODE, "ctrl_1", "ghost_slot"),
            &SlotAddr::new(TEST_CORE_NODE, "arm_1", "controller"),
        )
        .unwrap_err();
    assert!(
        matches!(err, NodeStackError::PairingSlotNotFound { ref link_id, .. } if link_id == "ghost_slot"),
        "expected PairingSlotNotFound, got {err:?}"
    );

    // Same role on both ends: two controllers cannot pair.
    let _ctrl2 = fixtures::start_instance_in_stack(
        &stack,
        &harness,
        "arm_controller",
        "v1",
        Some(&name("ctrl_2")),
    )
    .await;
    let err = stack
        .pair_slots(
            &SlotAddr::new(TEST_CORE_NODE, "ctrl_1", "arm"),
            &SlotAddr::new(TEST_CORE_NODE, "ctrl_2", "arm"),
        )
        .unwrap_err();
    assert!(
        matches!(err, NodeStackError::PairingRolesNotComplementary { ref role, .. } if role == "controller"),
        "expected PairingRolesNotComplementary, got {err:?}"
    );
}

#[tokio::test]
async fn death_dissolves_pairs_and_reads_prune_lazily() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let harness = real_lifecycle::lifecycle_harness();
    let _arm =
        fixtures::push_started(&stack, &harness, robot_arm_config(), Some(&name("arm_1"))).await;
    let ctrl = fixtures::push_started(
        &stack,
        &harness,
        arm_controller_config(),
        Some(&name("ctrl_1")),
    )
    .await;

    let arm_slot = SlotAddr::new(TEST_CORE_NODE, "arm_1", "controller");
    let ctrl_slot = SlotAddr::new(TEST_CORE_NODE, "ctrl_1", "arm");
    stack.pair_slots(&ctrl_slot, &arm_slot).expect("pair");

    // Eager path: the stop paths call dissolve_pairs_for_instance and
    // live-notify the survivor with what it returns.
    let dissolved = stack.dissolve_pairs_for_instance("ctrl_1");
    assert_eq!(dissolved.len(), 1);
    assert_eq!(
        dissolved[0].peer_of(&ctrl_slot).map(|e| e.slot.clone()),
        Some(arm_slot.clone())
    );
    assert!(stack.pairs().is_empty());

    // Lazy path: re-pair, then stop the controller WITHOUT dissolving.
    stack.pair_slots(&ctrl_slot, &arm_slot).expect("re-pair");
    drop(ctrl); // stops the instance; nothing calls dissolve
    assert!(
        stack.pairs().is_empty(),
        "registry reads must prune pairs whose endpoint died"
    );
    // And the survivor's slot is claimable again.
    assert!(
        stack
            .unpaired_pairing_slots()
            .iter()
            .any(|(slot, _)| slot == &arm_slot),
        "the survivor's slot must be released"
    );
}

#[tokio::test]
async fn reset_clears_the_registry() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let harness = real_lifecycle::lifecycle_harness();
    let _arm =
        fixtures::push_started(&stack, &harness, robot_arm_config(), Some(&name("arm_1"))).await;
    let _ctrl = fixtures::push_started(
        &stack,
        &harness,
        arm_controller_config(),
        Some(&name("ctrl_1")),
    )
    .await;
    stack
        .pair_slots(
            &SlotAddr::new(TEST_CORE_NODE, "ctrl_1", "arm"),
            &SlotAddr::new(TEST_CORE_NODE, "arm_1", "controller"),
        )
        .expect("pair");
    stack.reset();
    assert!(stack.pairs().is_empty());
}

#[tokio::test]
async fn serialized_graph_overlays_pairing_slots() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    let harness = real_lifecycle::lifecycle_harness();
    let _arm =
        fixtures::push_started(&stack, &harness, robot_arm_config(), Some(&name("arm_1"))).await;
    let _ctrl = fixtures::push_started(
        &stack,
        &harness,
        arm_controller_config(),
        Some(&name("ctrl_1")),
    )
    .await;

    // Unpaired: both slots surface with Unpaired bindings + manifest metadata.
    let graph = stack.to_serialized_graph();
    let arm_node = graph.find_node("robot_arm", "v1").expect("arm in graph");
    let arm_slots = &arm_node.instances[0].pairing_slots;
    let slot = arm_slots.get("controller").expect("declared slot surfaces");
    assert_eq!(slot.pairing_name, "arm_link");
    assert_eq!(slot.role, "arm");
    assert!(slot.optional);
    assert_eq!(slot.binding, PairingSlotBinding::Unpaired);

    // Paired: the binding carries the peer's full address + slot link_id.
    stack
        .pair_slots(
            &SlotAddr::new(TEST_CORE_NODE, "ctrl_1", "arm"),
            &SlotAddr::new(TEST_CORE_NODE, "arm_1", "controller"),
        )
        .expect("pair");
    let graph = stack.to_serialized_graph();
    let ctrl_node = graph
        .find_node("arm_controller", "v1")
        .expect("controller in graph");
    let ctrl_slots = &ctrl_node.instances[0].pairing_slots;
    let slot = ctrl_slots.get("arm").expect("declared slot surfaces");
    assert!(!slot.optional);
    let PairingSlotBinding::Paired { peer, peer_link_id } = &slot.binding else {
        panic!("expected Paired, got {:?}", slot.binding);
    };
    assert_eq!(peer.instance_id, "arm_1");
    assert_eq!(
        peer.core_node, "core",
        "stamped with the daemon's own core node"
    );
    assert_eq!(peer_link_id, "controller");
}

/// `depends_on.pairings` contributes no DAG edges: two nodes joined only by
/// a pairing have no dependency relationship, launch in any order, and the
/// serialized graph shows no edge between them.
#[test]
fn pairings_are_dag_invisible() {
    let stack = NodeStack::new(core_node_config(), None, PathBuf::from("/tmp"));
    // The controller declares a pairing but NOT the arm node as a dep, so it
    // can be pushed with strict dependency validation while the arm is
    // entirely absent from the stack.
    stack
        .push_config(
            arm_controller_config(),
            false,
            PathBuf::from("/tmp/ctrl.json5"),
        )
        .expect("a pairing-only node pushes with no dependencies present");
    stack
        .push_config(robot_arm_config(), false, PathBuf::from("/tmp/arm.json5"))
        .expect("push arm");

    let graph = stack.to_serialized_graph();
    let between_pair_nodes = graph.edges.iter().any(|e| {
        let names = [e.from.name.as_str(), e.to.name.as_str()];
        names.contains(&"robot_arm") && names.contains(&"arm_controller")
    });
    assert!(
        !between_pair_nodes,
        "pairing must not create a graph edge: {:?}",
        graph.edges
    );
}
