//! Python template snapshots for peer (pairing) topics — twins of the Rust
//! suite: slot-scoped publisher (`link_id=LINK_ID`), the pairing wire
//! target, the `subscribe_peer` seam, `paired()`/`wait_paired()`, and no
//! binding-slot involvement.

use super::*;
use config::node::EmittedTopic;

const JOINT_STATES: &str = r#"
{
  name: "joint_states",
  qos_profile: "sensor_data",
  message_format: {
    positions: { $type: "array", $items: "f64", $length: 3 },
    timestamp: "time"
  }
}
"#;

fn peer_context() -> crate::generator::types::PeerContext {
    crate::generator::types::PeerContext {
        link_id: "controller".to_string(),
        pairing_name: "arm_link".to_string(),
        pairing_tag: "v1".to_string(),
    }
}

fn parse_topic(example: &str) -> EmittedTopic {
    serde_json5::from_str(example).unwrap()
}

#[test]
fn peer_emitted_topic_publishes_slot_scoped_under_pairing_target() {
    let topic = parse_topic(JOINT_STATES);
    let mut generator = PythonGenerator::new();
    generator
        .add_peer_emitted_topic(&topic, &peer_context())
        .unwrap();
    let artifacts = generator.into_artifacts();
    assert_eq!(artifacts.len(), 1);
    let artifact = &artifacts[0];
    assert_eq!(artifact.kind, InterfaceKind::PeerEmittedTopic);
    assert_eq!(
        artifact.module_path,
        vec!["controller".to_string(), "joint_states".to_string()]
    );

    let code = &artifact.code_output;
    for needle in [
        "TOPIC_NAME = \"joint_states\"",
        "LINK_ID = \"controller\"",
        "PAIRING_NAME = \"arm_link\"",
        "PAIRING_TAG = \"v1\"",
        "peppylib.SenderTarget.pairing(PAIRING_NAME, PAIRING_TAG)",
        "link_id=LINK_ID",
        "def build_message(",
        "async def declare_publisher(",
        "def paired(",
        "async def wait_paired(",
        "node_runner.peer(LINK_ID)",
    ] {
        assert!(code.contains(needle), "missing `{needle}` in:\n{code}");
    }
    assert!(
        !code.contains("SenderTarget.node(") && !code.contains("SenderTarget.interface("),
        "peer topics must use the pairing target only:\n{code}"
    );
}

#[test]
fn peer_consumed_topic_wraps_subscribe_peer_without_binding_slots() {
    let topic = parse_topic(JOINT_STATES);
    let mut generator = PythonGenerator::new();
    generator
        .add_peer_consumed_topic(&topic, &peer_context())
        .unwrap();
    let artifacts = generator.into_artifacts();
    assert_eq!(artifacts.len(), 1);
    let artifact = &artifacts[0];
    assert_eq!(artifact.kind, InterfaceKind::PeerConsumedTopic);

    let code = &artifact.code_output;
    for needle in [
        "node_runner.subscribe_peer(",
        "LINK_ID,",
        "PAIRING_NAME,",
        "PAIRING_TAG,",
        "TOPIC_NAME,",
        "class Subscription:",
        "async def wait_paired(",
        "_deserialize_payload",
    ] {
        assert!(code.contains(needle), "missing `{needle}` in:\n{code}");
    }
    assert!(
        !code.contains("bound_producers_for") && !code.contains("TopicMessenger.subscribe("),
        "peer subscriptions must ride subscribe_peer, not the binding-slot path:\n{code}"
    );
}

#[test]
fn peer_consumed_topic_requires_a_message_format() {
    let topic: EmittedTopic =
        serde_json5::from_str(r#"{ name: "opaque", qos_profile: "reliable" }"#).unwrap();
    let mut generator = PythonGenerator::new();
    let err = generator
        .add_peer_consumed_topic(&topic, &peer_context())
        .unwrap_err();
    assert!(
        matches!(
            err,
            crate::error::Error::PeerTopicMissingMessageFormat { .. }
        ),
        "expected PeerTopicMissingMessageFormat, got {err:?}"
    );
}

#[test]
fn duplicate_peer_module_path_is_a_collision_backstop() {
    let topic = parse_topic(JOINT_STATES);
    let mut generator = PythonGenerator::new();
    generator
        .add_peer_emitted_topic(&topic, &peer_context())
        .unwrap();
    let err = generator
        .add_peer_consumed_topic(&topic, &peer_context())
        .unwrap_err();
    assert!(
        matches!(err, crate::error::Error::PeerTopicNameCollision { .. }),
        "expected PeerTopicNameCollision, got {err:?}"
    );
}
