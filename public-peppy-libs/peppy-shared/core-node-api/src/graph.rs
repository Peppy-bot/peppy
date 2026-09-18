//! Serializable graph types shared across every stack-returning service.
//!
//! These are the JSON payload that every `*_response.graph_json` field
//! contains. Producer: `node_stack::NodeStack::to_serialized_graph`.
//! Consumer: any caller that parses `graph_json` (peppylib wrappers, the
//! peppy CLI, tests).

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use config::node::{Cardinality, EndpointKind};
use config::runtime::{ClockBinding, PairedPeer, SlotBindings};
use serde::{Deserialize, Serialize};

/// One endpoint an instance serves, as every listing reports it: the label
/// the manifest declares it under, the kind the declaration gives it, and
/// the URLs the daemon hosting the instance expanded the bound socket into,
/// one per host interface address, loopback first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceEndpoint {
    pub label: String,
    pub kind: EndpointKind,
    pub urls: Vec<String>,
}

/// Per-instance lifecycle state. Wire representation is the lowercase variant
/// name (`"starting"`, `"running"`, `"finished"`, `"failed"`).
///
/// `Starting` and `Running` are live states; `Finished` and `Failed` are
/// terminal: the node's OS process has exited on its own (not via an explicit
/// stop or daemon teardown) and the instance will not run again. `Finished` is
/// a clean exit (status code 0), the expected end state of a one-shot node that
/// completes its work and shuts itself down; `Failed` is a non-zero or
/// signal-driven exit, i.e. the node crashed. Terminal instances stay visible
/// in the stack until the stack is cleared or the instance is stopped, and the
/// health monitor never probes them (a finished node has no health to report).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstanceState {
    Starting,
    Running,
    Finished,
    Failed,
}

impl InstanceState {
    pub fn as_str(self) -> &'static str {
        match self {
            InstanceState::Starting => "starting",
            InstanceState::Running => "running",
            InstanceState::Finished => "finished",
            InstanceState::Failed => "failed",
        }
    }

    /// `true` for the terminal states (`Finished`, `Failed`): the process has
    /// exited and the instance will not transition again. Callers use this to
    /// skip live-only work such as health probing and to render an exited
    /// instance without a (meaningless) health verdict.
    pub fn is_terminal(self) -> bool {
        matches!(self, InstanceState::Finished | InstanceState::Failed)
    }
}

impl fmt::Display for InstanceState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for InstanceState {
    type Err = UnknownInstanceState;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "starting" => Ok(InstanceState::Starting),
            "running" => Ok(InstanceState::Running),
            "finished" => Ok(InstanceState::Finished),
            "failed" => Ok(InstanceState::Failed),
            other => Err(UnknownInstanceState(other.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownInstanceState(pub String);

impl fmt::Display for UnknownInstanceState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown instance state `{}`", self.0)
    }
}

impl std::error::Error for UnknownInstanceState {}

/// Label-only view of a `NodeEntity`'s lifecycle stage. The rich internal
/// variant lives in `node_stack::NodeStage`; this one is the shape written
/// to the wire (JSON / capnp text field) and read by external consumers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NodeStage {
    Added,
    Building,
    Ready,
    Root,
}

impl NodeStage {
    pub fn as_str(self) -> &'static str {
        match self {
            NodeStage::Added => "Added",
            NodeStage::Building => "Building",
            NodeStage::Ready => "Ready",
            NodeStage::Root => "Root",
        }
    }
}

impl fmt::Display for NodeStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for NodeStage {
    type Err = UnknownNodeStage;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Added" => Ok(NodeStage::Added),
            "Building" => Ok(NodeStage::Building),
            "Ready" => Ok(NodeStage::Ready),
            "Root" => Ok(NodeStage::Root),
            other => Err(UnknownNodeStage(other.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownNodeStage(pub String);

impl fmt::Display for UnknownNodeStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown node stage `{}`", self.0)
    }
}

impl std::error::Error for UnknownNodeStage {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializedInstance {
    pub instance_id: String,
    pub state: InstanceState,
    /// Liveness from the most recent `node_health` probe the `stack list`
    /// service ran for this instance: `true` if it answered its `node_health`
    /// service within the probe timeout, `false` otherwise. Defaulted to `true`
    /// on decode so a `graph_json` payload from a producer that predates this
    /// field is not read as spuriously unhealthy.
    #[serde(default = "default_instance_healthy")]
    pub healthy: bool,
    /// The one clock this instance reads, mirroring
    /// [`config::runtime::NodeInstanceConfig`]'s resolved framework. Wall time
    /// is the default and is omitted, so a payload from a producer that
    /// predates clock domains reads as the wall-time instance it was.
    #[serde(default, skip_serializing_if = "ClockBinding::is_wall")]
    pub clock: ClockBinding,
    /// Validator-resolved producers bound to each of this instance's
    /// `depends_on` slots, keyed by the consumer manifest's link id.
    /// Mirrors [`config::runtime::NodeInstanceConfig::slot_bindings`] and
    /// the `TrackedNodeInstance` this is produced from. Empty for
    /// instances whose manifest declares no `depends_on` slots. Defaulted
    /// on decode so `graph_json` payloads from producers that predate this
    /// field still parse.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub slot_bindings: SlotBindings,
    /// Live pairing-slot state for every `depends_on.pairings` entry, keyed
    /// by the slot's link_id. Overlaid by the daemon from the manifest plus
    /// its pairing registry when serializing the graph — this is the
    /// observability surface behind `peppy stack list`'s pairing rows.
    /// Empty for instances whose manifest declares no pairings; defaulted on
    /// decode for payloads that predate the field.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pairing_slots: BTreeMap<String, SerializedPairingSlot>,
    /// The endpoints this instance serves, in label order: what `peppy stack
    /// list` prints so an operator finds the URLs without reading the node.
    /// Empty for a node whose manifest declares none, and always on the wire
    /// so a reader sees the set the daemon holds.
    #[serde(default)]
    pub endpoints: Vec<InstanceEndpoint>,
}

/// One pairing slot of a [`SerializedInstance`]: the declaring manifest's
/// pairing identity plus the slot's live binding from the daemon's pairing
/// registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializedPairingSlot {
    pub pairing_name: String,
    pub pairing_tag: String,
    /// The role this instance plays in the pairing.
    pub role: String,
    /// The pairs the slot holds right now, in establishment order; empty
    /// while unpaired.
    pub peers: Vec<PairedPeer>,
    /// The slot's declared cardinality, so a reader can tell a slot holding
    /// no pair by design from one whose peer is missing. Defaulted on decode
    /// for payloads from a producer that predates the field, which reads them
    /// as `one` and so as missing a peer.
    #[serde(default, skip_serializing_if = "Cardinality::is_one")]
    pub cardinality: Cardinality,
}

/// Decode default for [`SerializedInstance::healthy`]: assume healthy when the
/// field is absent (an older producer) rather than flagging every instance
/// unhealthy on version skew.
fn default_instance_healthy() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializedNode {
    pub name: String,
    pub tag: String,
    /// Core node whose stack owns this node.
    pub core_node: String,
    pub config_path: String,
    pub artifact_path: Option<String>,
    /// Lifecycle stage name.
    pub stage: NodeStage,
    /// All tracked instances with their per-instance state, including
    /// in-flight `Starting` instances.
    pub instances: Vec<SerializedInstance>,
}

impl SerializedNode {
    pub fn label(&self) -> String {
        format!("{}:{}", self.name, self.tag)
    }

    /// Externally visible instance ids — the subset of `instances` that have
    /// reached `Running`. In-flight `Starting` instances are intentionally
    /// hidden: the externally-visible meaning is "currently running and
    /// reachable via messenger services".
    pub fn running_instance_ids(&self) -> Vec<&str> {
        self.running_instances()
            .map(|i| i.instance_id.as_str())
            .collect()
    }

    /// Count of `Running` instances. Matches `running_instance_ids().len()`.
    pub fn instance_count(&self) -> usize {
        self.running_instances().count()
    }

    fn running_instances(&self) -> impl Iterator<Item = &SerializedInstance> {
        self.instances
            .iter()
            .filter(|i| i.state == InstanceState::Running)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializedEdge {
    pub from: SerializedNode,
    pub to: SerializedNode,
    /// `Some("name:tag")` when this edge is a dependency resolved through
    /// contract implementation (the consumer declares `depends_on.contracts` and
    /// `to` is a node whose `manifest.implements` names that contract); `None` for a direct
    /// `depends_on.nodes` edge. Defaulted on decode so payloads from producers
    /// that predate this field still parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via_contract: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializedNodeGraph {
    pub nodes: Vec<SerializedNode>,
    pub edges: Vec<SerializedEdge>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeNotFound {
    label: String,
}

impl NodeNotFound {
    pub fn new(name: &str, tag: &str) -> Self {
        Self {
            label: format!("{name}:{tag}"),
        }
    }

    /// `name:tag` of the missing node.
    pub fn label(&self) -> &str {
        &self.label
    }
}

impl fmt::Display for NodeNotFound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "no node matches `{}`", self.label)
    }
}

impl std::error::Error for NodeNotFound {}

impl SerializedNodeGraph {
    /// Look up a node by its `(name, tag)` identity. The pair is unique
    /// across `nodes`, so the first match is the only match.
    pub fn find_node(&self, name: &str, tag: &str) -> Option<&SerializedNode> {
        self.nodes.iter().find(|n| n.name == name && n.tag == tag)
    }

    /// Externally visible instance ids for the node identified by
    /// `(node_name, node_tag)`. Returns `NodeNotFound` when no node
    /// matches; returns `Ok(vec![])` when the node exists but every
    /// instance is still `Starting`.
    pub fn running_instance_ids_by_node(
        &self,
        node_name: &str,
        node_tag: &str,
    ) -> Result<Vec<&str>, NodeNotFound> {
        self.find_node(node_name, node_tag)
            .map(SerializedNode::running_instance_ids)
            .ok_or_else(|| NodeNotFound::new(node_name, node_tag))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::runtime::ProducerRef;

    fn make_node(name: &str, tag: &str, instances: &[(&str, InstanceState)]) -> SerializedNode {
        SerializedNode {
            name: name.into(),
            tag: tag.into(),
            core_node: "test_core".into(),
            config_path: String::new(),
            artifact_path: None,
            stage: NodeStage::Ready,
            instances: instances
                .iter()
                .map(|(id, st)| SerializedInstance {
                    instance_id: (*id).into(),
                    state: *st,
                    healthy: true,
                    clock: ClockBinding::Wall,
                    slot_bindings: BTreeMap::new(),
                    pairing_slots: BTreeMap::new(),
                    endpoints: Vec::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn find_node_returns_matching_node() {
        let graph = SerializedNodeGraph {
            nodes: vec![
                make_node("foo", "v1", &[]),
                make_node("foo", "v2", &[("r1", InstanceState::Running)]),
                make_node("bar", "v1", &[]),
            ],
            edges: vec![],
        };
        let node = graph.find_node("foo", "v2").expect("node should be found");
        assert_eq!(node.name, "foo");
        assert_eq!(node.tag, "v2");
        assert_eq!(node.instances.len(), 1);
    }

    #[test]
    fn find_node_returns_none_when_missing() {
        let graph = SerializedNodeGraph {
            nodes: vec![make_node("foo", "v1", &[])],
            edges: vec![],
        };
        assert!(graph.find_node("foo", "v2").is_none());
        assert!(graph.find_node("bar", "v1").is_none());
    }

    #[test]
    fn by_node_returns_running_only() {
        let graph = SerializedNodeGraph {
            nodes: vec![make_node(
                "foo",
                "v1",
                &[
                    ("r1", InstanceState::Running),
                    ("s1", InstanceState::Starting),
                    ("r2", InstanceState::Running),
                ],
            )],
            edges: vec![],
        };
        assert_eq!(
            graph.running_instance_ids_by_node("foo", "v1"),
            Ok(vec!["r1", "r2"])
        );
    }

    #[test]
    fn by_node_ok_empty_when_all_starting() {
        let graph = SerializedNodeGraph {
            nodes: vec![make_node(
                "foo",
                "v1",
                &[
                    ("s1", InstanceState::Starting),
                    ("s2", InstanceState::Starting),
                ],
            )],
            edges: vec![],
        };
        assert_eq!(graph.running_instance_ids_by_node("foo", "v1"), Ok(vec![]));
    }

    #[test]
    fn by_node_err_when_name_mismatch() {
        let graph = SerializedNodeGraph {
            nodes: vec![make_node("foo", "v1", &[("r1", InstanceState::Running)])],
            edges: vec![],
        };
        assert_eq!(
            graph.running_instance_ids_by_node("bar", "v1"),
            Err(NodeNotFound::new("bar", "v1"))
        );
    }

    #[test]
    fn by_node_err_when_tag_mismatch() {
        let graph = SerializedNodeGraph {
            nodes: vec![make_node("foo", "v1", &[("r1", InstanceState::Running)])],
            edges: vec![],
        };
        assert_eq!(
            graph.running_instance_ids_by_node("foo", "v2"),
            Err(NodeNotFound::new("foo", "v2"))
        );
    }

    #[test]
    fn by_node_err_on_empty_graph() {
        let graph = SerializedNodeGraph {
            nodes: vec![],
            edges: vec![],
        };
        assert_eq!(
            graph.running_instance_ids_by_node("foo", "v1"),
            Err(NodeNotFound::new("foo", "v1"))
        );
    }

    #[test]
    fn by_node_picks_correct_among_many() {
        let graph = SerializedNodeGraph {
            nodes: vec![
                make_node("foo", "v1", &[("foo_v1_r1", InstanceState::Running)]),
                make_node(
                    "foo",
                    "v2",
                    &[
                        ("foo_v2_r1", InstanceState::Running),
                        ("foo_v2_r2", InstanceState::Running),
                    ],
                ),
                make_node("bar", "v1", &[("bar_v1_r1", InstanceState::Running)]),
            ],
            edges: vec![],
        };
        assert_eq!(
            graph.running_instance_ids_by_node("foo", "v2"),
            Ok(vec!["foo_v2_r1", "foo_v2_r2"])
        );
    }

    #[test]
    fn slot_bindings_round_trip_through_json() {
        use config::runtime::BoundProducers;

        let mut bindings = BTreeMap::new();
        bindings.insert(
            "arm".to_string(),
            BoundProducers::from(ProducerRef::new("core_a", "arm-1")),
        );
        // A multi-cardinality slot's ordered set and a zero_or_more slot's
        // empty set both round-trip.
        bindings.insert(
            "cameras".to_string(),
            BoundProducers::try_from(vec![
                ProducerRef::new("core_a", "cam-1"),
                ProducerRef::new("core_a", "cam-2"),
            ])
            .expect("distinct producers"),
        );
        bindings.insert("spare_cameras".to_string(), BoundProducers::default());
        let instance = SerializedInstance {
            instance_id: "i1".to_string(),
            state: InstanceState::Running,
            healthy: true,
            clock: ClockBinding::Wall,
            slot_bindings: bindings,
            pairing_slots: BTreeMap::new(),
            endpoints: Vec::new(),
        };

        let json = serde_json::to_string(&instance).expect("serialize");
        let decoded: SerializedInstance = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, instance);
    }

    #[test]
    fn instance_endpoints_round_trip_with_their_kind_and_are_always_on_the_wire() {
        let served = SerializedInstance {
            instance_id: "mcp".to_string(),
            state: InstanceState::Running,
            healthy: true,
            clock: ClockBinding::Wall,
            slot_bindings: BTreeMap::new(),
            pairing_slots: BTreeMap::new(),
            endpoints: vec![
                InstanceEndpoint {
                    label: "camera_v1".to_string(),
                    kind: EndpointKind::Mcp,
                    urls: vec!["http://127.0.0.1:8900/camera/v1/mcp".to_string()],
                },
                InstanceEndpoint {
                    label: "panel".to_string(),
                    kind: EndpointKind::Page,
                    urls: vec![
                        "http://127.0.0.1:8765".to_string(),
                        "http://192.168.1.5:8765".to_string(),
                    ],
                },
            ],
        };
        let json = serde_json::to_string(&served).expect("serialize");
        assert!(json.contains(r#""kind":"mcp""#), "{json}");
        assert!(json.contains(r#""kind":"page""#), "{json}");
        let decoded: SerializedInstance = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, served);

        let plain = SerializedInstance {
            endpoints: Vec::new(),
            ..served
        };
        let json = serde_json::to_string(&plain).expect("serialize");
        assert!(
            json.contains(r#""endpoints":[]"#),
            "a node that serves no endpoint still carries the empty set: {json}"
        );
        let decoded: SerializedInstance = serde_json::from_str(&json).expect("deserialize");
        assert!(decoded.endpoints.is_empty());
    }

    #[test]
    fn instance_without_slot_bindings_omits_the_field_and_reads_it_back_empty() {
        // No bindings -> `skip_serializing_if` keeps the field out of the wire
        // form, and an absent field reads back empty.
        let instance = SerializedInstance {
            instance_id: "i1".to_string(),
            state: InstanceState::Running,
            healthy: true,
            clock: ClockBinding::Wall,
            slot_bindings: BTreeMap::new(),
            pairing_slots: BTreeMap::new(),
            endpoints: Vec::new(),
        };
        let json = serde_json::to_string(&instance).expect("serialize");
        assert!(
            !json.contains("slot_bindings"),
            "empty bindings must be omitted from the wire form: {json}"
        );

        let absent = r#"{"instance_id":"i1","state":"running"}"#;
        let decoded: SerializedInstance = serde_json::from_str(absent).expect("decode");
        assert_eq!(decoded, instance);
        assert!(decoded.slot_bindings.is_empty());
    }

    #[test]
    fn pairing_slots_round_trip_through_json() {
        let mut pairing_slots = BTreeMap::new();
        pairing_slots.insert(
            "arm".to_string(),
            SerializedPairingSlot {
                pairing_name: "arm_link".to_string(),
                pairing_tag: "v1".to_string(),
                role: "controller".to_string(),
                cardinality: Cardinality::One,
                peers: vec![config::runtime::PairedPeer {
                    peer: ProducerRef::new("core_a", "arm_1"),
                    peer_link_id: "controller".to_string(),
                    copy: None,
                }],
            },
        );
        pairing_slots.insert(
            "spare".to_string(),
            SerializedPairingSlot {
                pairing_name: "arm_link".to_string(),
                pairing_tag: "v1".to_string(),
                role: "controller".to_string(),
                cardinality: Cardinality::ZeroOrMore,
                peers: Vec::new(),
            },
        );
        let instance = SerializedInstance {
            instance_id: "ctrl_1".to_string(),
            state: InstanceState::Running,
            healthy: true,
            clock: ClockBinding::Wall,
            slot_bindings: BTreeMap::new(),
            pairing_slots,
            endpoints: Vec::new(),
        };

        let json = serde_json::to_string(&instance).expect("serialize");
        let decoded: SerializedInstance = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, instance);

        // No pairings -> the field is omitted, and an absent field reads
        // back empty.
        let bare = SerializedInstance {
            pairing_slots: BTreeMap::new(),
            ..instance
        };
        let json = serde_json::to_string(&bare).expect("serialize");
        assert!(
            !json.contains("pairing_slots"),
            "empty pairing_slots must be omitted from the wire form: {json}"
        );
        let absent = r#"{"instance_id":"ctrl_1","state":"running"}"#;
        let decoded: SerializedInstance = serde_json::from_str(absent).expect("decode");
        assert!(decoded.pairing_slots.is_empty());
    }

    #[test]
    fn node_not_found_display() {
        let err = NodeNotFound::new("router", "v1");
        let msg = err.to_string();
        assert!(msg.contains("router"), "got: {msg}");
        assert!(msg.contains("v1"), "got: {msg}");
        assert_eq!(err.label(), "router:v1");
    }

    #[test]
    fn instance_count_and_running_ids_count_running_only() {
        let node = make_node(
            "foo",
            "v1",
            &[
                ("r1", InstanceState::Running),
                ("s1", InstanceState::Starting),
                ("r2", InstanceState::Running),
            ],
        );
        assert_eq!(node.instance_count(), 2);
        assert_eq!(node.running_instance_ids(), vec!["r1", "r2"]);
        // The documented invariant: the two agree on count.
        assert_eq!(node.instance_count(), node.running_instance_ids().len());
    }

    #[test]
    fn label_joins_name_and_tag() {
        let node = make_node("router", "v2", &[]);
        assert_eq!(node.label(), "router:v2");
    }

    #[test]
    fn instance_state_str_display_and_parse_round_trip() {
        for state in [
            InstanceState::Starting,
            InstanceState::Running,
            InstanceState::Finished,
            InstanceState::Failed,
        ] {
            assert_eq!(state.to_string(), state.as_str());
            assert_eq!(state.as_str().parse::<InstanceState>(), Ok(state));
        }
        assert_eq!(InstanceState::Starting.as_str(), "starting");
        assert_eq!(InstanceState::Running.as_str(), "running");
        assert_eq!(InstanceState::Finished.as_str(), "finished");
        assert_eq!(InstanceState::Failed.as_str(), "failed");

        let err = "bogus"
            .parse::<InstanceState>()
            .expect_err("unknown must fail");
        assert_eq!(err, UnknownInstanceState("bogus".to_owned()));
        assert!(err.to_string().contains("bogus"), "got: {err}");
    }

    #[test]
    fn instance_state_terminal_classification() {
        assert!(!InstanceState::Starting.is_terminal());
        assert!(!InstanceState::Running.is_terminal());
        assert!(InstanceState::Finished.is_terminal());
        assert!(InstanceState::Failed.is_terminal());
    }

    #[test]
    fn instance_state_serde_wire_form_is_lowercase_and_matches_as_str() {
        // The wire form is a cross-process contract (the daemon serializes
        // instance state into the graph the CLI/UI deserialize). It rides on the
        // `#[serde(rename_all = "lowercase")]` derive, a code path entirely
        // separate from `as_str`/`FromStr`, so pin the literal JSON bytes for
        // every variant and assert the two representations cannot drift apart.
        let cases = [
            (InstanceState::Starting, "\"starting\""),
            (InstanceState::Running, "\"running\""),
            (InstanceState::Finished, "\"finished\""),
            (InstanceState::Failed, "\"failed\""),
        ];
        for (state, wire) in cases {
            assert_eq!(
                serde_json::to_string(&state).expect("serialize"),
                wire,
                "wire form regressed for {state:?}"
            );
            assert_eq!(
                serde_json::from_str::<InstanceState>(wire).expect("deserialize"),
                state,
                "wire form did not round-trip for {state:?}"
            );
            // The derived serde form and the hand-written `as_str` must stay
            // identical: a future variant added to only one path would diverge.
            assert_eq!(
                serde_json::to_value(state).expect("to_value"),
                serde_json::Value::String(state.as_str().to_owned()),
                "serde form and as_str diverged for {state:?}"
            );
        }
    }

    #[test]
    fn node_stage_str_display_and_parse_round_trip() {
        for stage in [
            NodeStage::Added,
            NodeStage::Building,
            NodeStage::Ready,
            NodeStage::Root,
        ] {
            assert_eq!(stage.to_string(), stage.as_str());
            assert_eq!(stage.as_str().parse::<NodeStage>(), Ok(stage));
        }

        let err = "Nope".parse::<NodeStage>().expect_err("unknown must fail");
        assert_eq!(err, UnknownNodeStage("Nope".to_owned()));
        assert!(err.to_string().contains("Nope"), "got: {err}");
    }

    #[test]
    fn node_rejects_payload_without_stage_or_instances() {
        let bare =
            r#"{"name":"n","tag":"v1","core_node":"core-a","config_path":"","artifact_path":null}"#;
        let error = serde_json::from_str::<SerializedNode>(bare).expect_err("stage is required");
        assert!(error.to_string().contains("stage"), "{error}");
        let no_instances = r#"{"name":"n","tag":"v1","core_node":"core-a","config_path":"","artifact_path":null,"stage":"Ready"}"#;
        let error = serde_json::from_str::<SerializedNode>(no_instances)
            .expect_err("instances are required");
        assert!(error.to_string().contains("instances"), "{error}");
    }

    #[test]
    fn node_rejects_payload_without_required_core_node() {
        let pre_change = r#"{"name":"n","tag":"v1","config_path":"","artifact_path":null}"#;
        let error = serde_json::from_str::<SerializedNode>(pre_change)
            .expect_err("core_node is a required wire field");
        assert!(error.to_string().contains("core_node"), "got: {error}");
    }

    #[test]
    fn edge_via_contract_defaults_to_none_and_is_omitted_when_absent() {
        let edge = SerializedEdge {
            from: make_node("a", "v1", &[]),
            to: make_node("b", "v1", &[]),
            via_contract: None,
        };
        let json = serde_json::to_string(&edge).expect("serialize");
        assert!(
            !json.contains("via_contract"),
            "None must be omitted from the wire form: {json}"
        );
        let decoded: SerializedEdge = serde_json::from_str(&json).expect("decode");
        assert_eq!(decoded, edge);

        // And a populated interface round-trips.
        let via = SerializedEdge {
            via_contract: Some("camera:v1".to_owned()),
            ..edge
        };
        let json = serde_json::to_string(&via).expect("serialize");
        assert!(json.contains("camera:v1"), "got: {json}");
        assert_eq!(
            serde_json::from_str::<SerializedEdge>(&json).expect("decode"),
            via
        );
    }
}
