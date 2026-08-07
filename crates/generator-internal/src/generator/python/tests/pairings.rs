//! Python template snapshots for peer (pairing) topics — twins of the Rust
//! suite: slot-scoped publisher (`link_id=LINK_ID`), the pairing wire
//! target, the `subscribe_peer` seam, `paired()`/`wait_paired()`, and no
//! binding-slot involvement.

use super::*;
use config::node::{Cardinality, NativeEmittedTopic};

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

fn parse_topic(example: &str) -> NativeEmittedTopic {
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
        !code.contains("SenderTarget.node(") && !code.contains("SenderTarget.contract("),
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
    assert_contains_all(
        code,
        &[
            "node_runner.subscribe_peer(",
            "LINK_ID,",
            "PAIRING_NAME,",
            "PAIRING_TAG,",
            "TOPIC_NAME,",
            "class Subscription:",
            // The tagged pair: the same PeerInfo that paired() returns.
            "async def next(self) -> Optional[Tuple[peppylib.PeerInfo, Message]]:",
            "return peer, message",
            "async def wait_paired(",
            "_deserialize_payload",
        ],
    );
    assert!(
        !code.contains("bound_producer") && !code.contains("TopicMessenger.subscribe("),
        "peer subscriptions must ride subscribe_peer, not the binding-slot path:\n{code}"
    );
    assert!(
        !code.contains("ProducerRef"),
        "a peer module tags every message with PeerInfo:\n{code}"
    );
}

#[test]
fn observed_consumed_topic_yields_observed_source_tagged_pairs() {
    let topic = parse_topic(JOINT_STATES);
    let mut generator = PythonGenerator::new();
    generator
        .add_observed_topic(&topic, &peer_context(), Cardinality::ZeroOrMore)
        .unwrap();
    let artifacts = generator.into_artifacts();
    assert_eq!(artifacts.len(), 1);
    let artifact = &artifacts[0];
    assert_eq!(artifact.kind, InterfaceKind::ObservedTopic);

    let code = &artifact.code_output;
    assert_contains_all(
        code,
        &[
            "node_runner.subscribe_observed(",
            "class Subscription:",
            // The tagged pair: the full member identity, so members sharing one
            // instance stay distinct.
            "async def next(self) -> Optional[Tuple[peppylib.ObservedSource, Message]]:",
            "return source, message",
            "async def __anext__(self) -> Tuple[peppylib.ObservedSource, Message]:",
            // The multi-cardinality slot accessor speaks the same type.
            "def sources(",
            "_deserialize_payload",
        ],
    );
    assert!(
        !code.contains("ProducerRef"),
        "an observer module tags every message with ObservedSource:\n{code}"
    );
}

/// The Python mirror of the Rust cardinality matrix. Two things differ, and both
/// are deliberate: the annotation is the only place `one` and `zero_or_one` part
/// ways, since Python has no way to spell "not optional" other than leaving the
/// `Optional` off; and the two multi cardinalities share `List[...]` because
/// Python has no non-empty list type, so `one_or_more` states its guarantee in
/// the docstring. The emitted name and the runtime method it calls also part
/// ways for `one`, which is what pins the name/method split.
#[test]
fn observed_topic_accessor_is_cardinality_typed() {
    let cases = [
        (
            Cardinality::One,
            "def source(node_runner: peppylib.NodeRunner) -> peppylib.ObservedSource:",
            "return node_runner.observation_slot(LINK_ID).sole_source()",
            "cardinality `one`",
            "def sources(",
        ),
        (
            Cardinality::ZeroOrOne,
            "def source(node_runner: peppylib.NodeRunner) -> Optional[peppylib.ObservedSource]:",
            "return node_runner.observation_slot(LINK_ID).source()",
            "cardinality `zero_or_one`",
            "def sources(",
        ),
        (
            Cardinality::OneOrMore,
            "def sources(node_runner: peppylib.NodeRunner) -> List[peppylib.ObservedSource]:",
            "return node_runner.observation_slot_set(LINK_ID).sources()",
            "cardinality `one_or_more`",
            "def source(",
        ),
        (
            Cardinality::ZeroOrMore,
            "def sources(node_runner: peppylib.NodeRunner) -> List[peppylib.ObservedSource]:",
            "return node_runner.observation_slot_set(LINK_ID).sources()",
            "cardinality `zero_or_more`",
            "def source(",
        ),
    ];
    for (cardinality, expected_def, expected_splice, expected_doc, absent_fn) in cases {
        let topic = parse_topic(JOINT_STATES);
        let mut generator = PythonGenerator::new();
        generator
            .add_observed_topic(&topic, &peer_context(), cardinality)
            .unwrap();
        let artifacts = generator.into_artifacts();
        let code = &artifacts[0].code_output;

        assert_contains_all(code, &[expected_def, expected_splice, expected_doc]);
        assert!(
            !code.contains(absent_fn),
            "a {cardinality:?} slot must expose only one accessor name; got: {code}"
        );
        // `zero_or_one` is the only cardinality whose accessor is annotated
        // optional. The module imports `Optional` either way, because the
        // subscription surface returns one, so the annotation is what to pin.
        assert_eq!(
            code.contains("Optional[peppylib.ObservedSource]"),
            cardinality == Cardinality::ZeroOrOne,
            "only `zero_or_one` annotates an optional source; got: {code}"
        );
        assert_eq!(
            code.contains("`[0]` is always valid."),
            cardinality == Cardinality::OneOrMore,
            "the never-empty note belongs to `one_or_more` alone; got: {code}"
        );
    }
}

#[test]
fn peer_consumed_topic_requires_a_message_format() {
    let topic: NativeEmittedTopic =
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
