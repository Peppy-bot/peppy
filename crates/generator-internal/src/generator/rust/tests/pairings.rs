//! Template snapshots for peer (pairing) topics: the slot-scoped publisher
//! splice, the `pairing` wire target, the `subscribe_peer` seam, the
//! module-level slot consts + `paired()`/`wait_paired()`, and the absence of
//! any binding-slot involvement.

use super::*;
use config::node::NativeEmittedTopic;

const JOINT_COMMANDS: &str = r#"
{
  name: "joint_commands",
  qos_profile: "reliable",
  message_format: {
    target_positions: { $type: "array", $items: "f64", $length: 3 },
    max_velocity: "f64"
  }
}
"#;

fn peer_context() -> crate::generator::types::PeerContext {
    crate::generator::types::PeerContext {
        link_id: "arm".to_string(),
        pairing_name: "arm_link".to_string(),
        pairing_tag: "v1".to_string(),
    }
}

fn parse_topic(example: &str) -> NativeEmittedTopic {
    serde_json5::from_str(example).unwrap()
}

#[test]
fn peer_emitted_topic_publishes_slot_scoped_under_pairing_target() {
    let topic = parse_topic(JOINT_COMMANDS);
    let mut generator = RustGenerator::new();
    generator
        .add_peer_emitted_topic(&topic, &peer_context())
        .unwrap();
    let artifacts = generator.into_artifacts();
    assert_eq!(artifacts.len(), 1);
    let artifact = &artifacts[0];
    assert_eq!(artifact.kind, InterfaceKind::PeerEmittedTopic);
    assert_eq!(
        artifact.module_path,
        vec!["arm".to_string(), "joint_commands".to_string()],
        "peer artifacts nest flat under pairings/<link_id>/<topic>"
    );

    let rendered = &artifact.code_output;
    assert_contains_all(
        rendered,
        &[
            // Slot consts.
            "pub const TOPIC_NAME: &str = \"joint_commands\"",
            "pub const LINK_ID: &str = \"arm\"",
            "pub const PAIRING_NAME: &str = \"arm_link\"",
            "pub const PAIRING_TAG: &str = \"v1\"",
            // The pairing wire target + the OWN slot link_id splice.
            "SenderTarget::pairing(",
            "PAIRING_NAME",
            "Some(LINK_ID)",
            // Pin-state helpers.
            "pub fn paired(",
            "pub async fn wait_paired(",
            "node_runner.peer(LINK_ID)",
            // Standard emit surface.
            "pub fn build_message(",
            "pub async fn declare_publisher(",
        ],
    );
    // Pairing publishers never use node/interface targets or the default
    // link_id sentinel.
    assert!(
        !rendered.contains("SenderTarget::node(") && !rendered.contains("SenderTarget::contract("),
        "peer topics must use the pairing target only:\n{rendered}"
    );
}

#[test]
fn peer_consumed_topic_wraps_subscribe_peer_without_binding_slots() {
    let topic = parse_topic(JOINT_COMMANDS);
    let mut generator = RustGenerator::new();
    generator
        .add_peer_consumed_topic(&topic, &peer_context())
        .unwrap();
    let artifacts = generator.into_artifacts();
    assert_eq!(artifacts.len(), 1);
    let artifact = &artifacts[0];
    assert_eq!(artifact.kind, InterfaceKind::PeerConsumedTopic);
    assert_eq!(
        artifact.module_path,
        vec!["arm".to_string(), "joint_commands".to_string()]
    );

    let rendered = &artifact.code_output;
    assert_contains_all(
        rendered,
        &[
            // The subscribe_peer seam with the slot consts spliced.
            "peppylib::runtime::subscribe_peer(",
            "LINK_ID",
            "PAIRING_NAME",
            "PAIRING_TAG",
            "TOPIC_NAME",
            // The held-subscription surface, yielding (producer, Message).
            "pub struct Subscription",
            "peppylib::runtime::PeerSubscription",
            "peppylib::messaging::ProducerRef",
            "pub async fn wait_paired(",
        ],
    );
    // No binding-slot machinery: pairing subscriptions are pinned by the
    // live peer_update channel, never by the consumer-filter path.
    assert!(
        !rendered.contains("ConsumerFilter") && !rendered.contains("TopicMessenger::subscribe("),
        "peer subscriptions must ride subscribe_peer, not the binding-slot path:\n{rendered}"
    );
}

#[test]
fn peer_consumed_topic_requires_a_message_format() {
    let topic: NativeEmittedTopic =
        serde_json5::from_str(r#"{ name: "opaque", qos_profile: "reliable" }"#).unwrap();
    let mut generator = RustGenerator::new();
    let err = generator
        .add_peer_consumed_topic(&topic, &peer_context())
        .unwrap_err();
    assert!(
        matches!(err, Error::PeerTopicMissingMessageFormat { .. }),
        "expected PeerTopicMissingMessageFormat, got {err:?}"
    );
}

#[test]
fn duplicate_peer_module_path_is_a_collision_backstop() {
    // Parse-time flat topic-name uniqueness on the pairing doc is the real
    // gate; this pins the generator-side backstop.
    let topic = parse_topic(JOINT_COMMANDS);
    let mut generator = RustGenerator::new();
    generator
        .add_peer_emitted_topic(&topic, &peer_context())
        .unwrap();
    let err = generator
        .add_peer_consumed_topic(&topic, &peer_context())
        .unwrap_err();
    assert!(
        matches!(err, Error::PeerTopicNameCollision { .. }),
        "expected PeerTopicNameCollision, got {err:?}"
    );
}

#[test]
fn two_slots_of_the_same_pairing_generate_isolated_modules() {
    // The two-arm commander shape: one contract, two slots, wire-isolated
    // streams (the slot IS the identity).
    let topic = parse_topic(JOINT_COMMANDS);
    let left = crate::generator::types::PeerContext {
        link_id: "left_arm".to_string(),
        ..peer_context()
    };
    let right = crate::generator::types::PeerContext {
        link_id: "right_arm".to_string(),
        ..peer_context()
    };
    let mut generator = RustGenerator::new();
    generator.add_peer_emitted_topic(&topic, &left).unwrap();
    generator.add_peer_emitted_topic(&topic, &right).unwrap();
    let artifacts = generator.into_artifacts();
    assert_eq!(artifacts.len(), 2);
    assert_eq!(artifacts[0].module_path[0], "left_arm");
    assert_eq!(artifacts[1].module_path[0], "right_arm");
    assert!(
        artifacts[0]
            .code_output
            .contains("pub const LINK_ID: &str = \"left_arm\"")
    );
    assert!(
        artifacts[1]
            .code_output
            .contains("pub const LINK_ID: &str = \"right_arm\"")
    );
}
