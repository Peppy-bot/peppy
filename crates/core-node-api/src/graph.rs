//! Serializable graph types shared across every stack-returning service.
//!
//! These are the JSON payload that every `*_response.graph_json` field
//! contains. Producer: `node_stack::NodeStack::to_serialized_graph`.
//! Consumer: any caller that parses `graph_json` (peppylib wrappers, the
//! peppy CLI, tests).

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use config::runtime::SlotBinding;
use serde::{Deserialize, Serialize};

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
    /// Validator-resolved per-slot bindings for this instance, keyed by the
    /// consumer manifest's `depends_on` link id. Mirrors
    /// [`config::runtime::NodeInstanceConfig::slot_bindings`] and the
    /// `TrackedNodeInstance` this is produced from. Empty for instances whose
    /// manifest declares no `depends_on` slots. Defaulted on decode so
    /// `graph_json` payloads from producers that predate this field still
    /// parse.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub slot_bindings: BTreeMap<String, SlotBinding>,
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
    pub config_path: String,
    pub artifact_path: Option<String>,
    /// Lifecycle stage name. `None` only for payloads produced by versions
    /// that predate the stage field; current producers always populate it.
    #[serde(default)]
    pub stage: Option<NodeStage>,
    /// All tracked instances with their per-instance state, including
    /// in-flight `Starting` instances.
    #[serde(default)]
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

    /// Returns the lifecycle stage label, or "Unknown" for legacy payloads
    /// that did not carry the stage field.
    pub fn stage_label(&self) -> &'static str {
        self.stage.map_or("Unknown", NodeStage::as_str)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializedEdge {
    pub from: SerializedNode,
    pub to: SerializedNode,
    /// `Some("name:tag")` when this edge is a dependency resolved through
    /// interface conformance (the consumer declares `depends_on.interfaces` and
    /// `to` is a node that `conforms_to` that interface); `None` for a direct
    /// `depends_on.nodes` edge. Defaulted on decode so payloads from producers
    /// that predate this field still parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via_interface: Option<String>,
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

    fn make_node(name: &str, tag: &str, instances: &[(&str, InstanceState)]) -> SerializedNode {
        SerializedNode {
            name: name.into(),
            tag: tag.into(),
            config_path: String::new(),
            artifact_path: None,
            stage: Some(NodeStage::Ready),
            instances: instances
                .iter()
                .map(|(id, st)| SerializedInstance {
                    instance_id: (*id).into(),
                    state: *st,
                    healthy: true,
                    slot_bindings: BTreeMap::new(),
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
        use config::runtime::ProducerRef;
        let mut bindings = BTreeMap::new();
        bindings.insert(
            "arm".to_string(),
            SlotBinding::Pinned {
                producer: ProducerRef::new("core_a", "arm-1"),
            },
        );
        bindings.insert(
            "sensors".to_string(),
            SlotBinding::FromAnyBound {
                producers: vec![
                    ProducerRef::new("core_a", "cam-1"),
                    ProducerRef::new("core_a", "cam-2"),
                ],
            },
        );
        let instance = SerializedInstance {
            instance_id: "i1".to_string(),
            state: InstanceState::Running,
            healthy: true,
            slot_bindings: bindings,
        };

        let json = serde_json::to_string(&instance).expect("serialize");
        let decoded: SerializedInstance = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, instance);
    }

    #[test]
    fn instance_without_slot_bindings_omits_field_and_decodes_legacy_payload() {
        // No bindings -> `skip_serializing_if` keeps the field out of the wire
        // form, so it stays byte-compatible with pre-bindings payloads.
        let instance = SerializedInstance {
            instance_id: "i1".to_string(),
            state: InstanceState::Running,
            healthy: true,
            slot_bindings: BTreeMap::new(),
        };
        let json = serde_json::to_string(&instance).expect("serialize");
        assert!(
            !json.contains("slot_bindings"),
            "empty bindings must be omitted from the wire form: {json}"
        );

        // A legacy payload that predates the field still decodes (default empty).
        let legacy = r#"{"instance_id":"i1","state":"running"}"#;
        let decoded: SerializedInstance = serde_json::from_str(legacy).expect("decode legacy");
        assert_eq!(decoded, instance);
        assert!(decoded.slot_bindings.is_empty());
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
    fn stage_label_reports_stage_or_unknown() {
        // make_node sets stage = Some(Ready).
        assert_eq!(make_node("a", "v1", &[]).stage_label(), "Ready");

        // A legacy payload with no stage reports "Unknown".
        let legacy = SerializedNode {
            name: "a".into(),
            tag: "v1".into(),
            config_path: String::new(),
            artifact_path: None,
            stage: None,
            instances: vec![],
        };
        assert_eq!(legacy.stage_label(), "Unknown");
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
    fn node_decodes_legacy_payload_without_stage_or_instances() {
        // Producers that predate `stage`/`instances` omit both; serde defaults
        // them to `None`/empty rather than failing to parse.
        let legacy = r#"{"name":"n","tag":"v1","config_path":"","artifact_path":null}"#;
        let decoded: SerializedNode = serde_json::from_str(legacy).expect("decode legacy node");
        assert_eq!(decoded.stage, None);
        assert!(decoded.instances.is_empty());
        assert_eq!(decoded.stage_label(), "Unknown");
    }

    #[test]
    fn edge_via_interface_defaults_to_none_and_is_omitted_when_absent() {
        let edge = SerializedEdge {
            from: make_node("a", "v1", &[]),
            to: make_node("b", "v1", &[]),
            via_interface: None,
        };
        let json = serde_json::to_string(&edge).expect("serialize");
        assert!(
            !json.contains("via_interface"),
            "None must be omitted from the wire form: {json}"
        );
        let decoded: SerializedEdge = serde_json::from_str(&json).expect("decode");
        assert_eq!(decoded, edge);

        // And a populated interface round-trips.
        let via = SerializedEdge {
            via_interface: Some("camera:v1".to_owned()),
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
