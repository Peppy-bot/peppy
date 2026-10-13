//! Serializable graph types shared across every stack-returning service, the
//! refusal text of an instance id the stack already tracks, and the wire's
//! unix-millisecond time (`unix_ms`).
//!
//! These are the JSON payload that every `*_response.graph_json` field
//! contains. Producer: `node_stack::NodeStack::to_serialized_graph`.
//! Consumer: any caller that parses `graph_json` (peppylib wrappers, the
//! peppy CLI, tests).

use std::collections::BTreeMap;
use std::fmt;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::str::FromStr;

use config::node::{Cardinality, EndpointKind, RestartDeclaration, RestartOrigin, RestartPolicy};
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
/// name (`"starting"`, `"running"`, `"restarting"`, `"finished"`, `"failed"`).
///
/// `Starting` and `Running` have a process. `Restarting` is an instance
/// whose process exited and whose restart policy starts it again: the daemon
/// waits out the backoff delay, then the instance is `Starting` again.
/// `Finished` is a clean exit (status code 0) the policy leaves alone, the
/// expected end state of a one-shot node that completes its work and shuts
/// itself down; `Failed` is a non-zero or signal-driven exit the policy
/// leaves alone, a refused restart, or the attempt `max_restarts` refused. Instances
/// without a process stay visible in the stack until the stack is cleared or
/// the instance is stopped, and the health monitor never probes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstanceState {
    Starting,
    Running,
    Restarting,
    Finished,
    Failed,
}

impl InstanceState {
    pub fn as_str(self) -> &'static str {
        match self {
            InstanceState::Starting => "starting",
            InstanceState::Running => "running",
            InstanceState::Restarting => "restarting",
            InstanceState::Finished => "finished",
            InstanceState::Failed => "failed",
        }
    }

    /// Whether a process of this instance exists or is being started.
    /// Callers use it to skip process-only work such as health probing, pair
    /// delivery and shutdown requests, and to render an instance without a
    /// process with no health verdict.
    pub fn has_process(self) -> bool {
        matches!(self, InstanceState::Starting | InstanceState::Running)
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
            "restarting" => Ok(InstanceState::Restarting),
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

/// The command that stops `instance_id` on the daemon `core_node`, as every
/// remedy types it.
pub fn stop_command(instance_id: &str, core_node: &str) -> String {
    format!("`peppy node stop {instance_id} --core-node {core_node}`")
}

/// The refusal of an instance id the stack of the daemon `core_node` already
/// tracks, in any state, naming the node, the state and what frees the id.
pub fn duplicate_instance_id_message(
    instance_id: &str,
    node_name: &str,
    node_tag: &str,
    core_node: &str,
    state: InstanceState,
) -> String {
    let stop = stop_command(instance_id, core_node);
    let remedy = match state {
        InstanceState::Starting => format!("run under another id, or {stop} once it is running"),
        InstanceState::Running => format!("run under another id, or {stop} first"),
        InstanceState::Restarting | InstanceState::Finished | InstanceState::Failed => {
            format!("{stop} frees the id")
        }
    };
    format!(
        "Instance ID `{instance_id}` already exists for node `{node_name}:{node_tag}` and is \
         {state}; {remedy}"
    )
}

/// A time as the wire carries it: milliseconds since the Unix epoch.
pub fn unix_ms(time: std::time::SystemTime) -> u64 {
    time.duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Why the last start of an instance ended: its process's exit, or a start
/// failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitCause {
    /// The process exited with this code.
    Code(i32),
    /// The process was ended by a signal: its number, its name on the
    /// machine the process ran on, and whether the process dumped core.
    Signal {
        number: i32,
        name: Option<String>,
        core_dumped: bool,
    },
    /// The daemon killed the process, which answered no health check for
    /// this many seconds.
    HealthKill { unanswered_secs: u64 },
    /// The daemon's own account: a start failure's message, or an exit
    /// status it could not read.
    Message(String),
}

impl ExitCause {
    /// The cause of a reaped process's exit.
    pub fn of(status: &std::io::Result<std::process::ExitStatus>) -> Self {
        match status {
            Ok(status) => match (status.code(), signal_of(status)) {
                (Some(code), _) => ExitCause::Code(code),
                (None, Some(signal)) => ExitCause::signal(signal, core_dumped(status)),
                (None, None) => ExitCause::Message(status.to_string()),
            },
            Err(e) => ExitCause::Message(format!("could not read exit status: {e}")),
        }
    }

    /// The cause of an end by `signal`, named as this machine numbers it.
    pub fn signal(signal: i32, core_dumped: bool) -> Self {
        ExitCause::Signal {
            number: signal,
            name: signal_name(signal).map(str::to_owned),
            core_dumped,
        }
    }

    /// Whether the process exited with code 0.
    pub fn is_clean(&self) -> bool {
        matches!(self, ExitCause::Code(0))
    }
}

#[cfg(unix)]
fn signal_of(status: &std::process::ExitStatus) -> Option<i32> {
    status.signal()
}

#[cfg(not(unix))]
fn signal_of(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

#[cfg(unix)]
fn core_dumped(status: &std::process::ExitStatus) -> bool {
    status.core_dumped()
}

#[cfg(not(unix))]
fn core_dumped(_status: &std::process::ExitStatus) -> bool {
    false
}

/// The name of `signal` as this machine numbers it.
#[cfg(unix)]
fn signal_name(signal: i32) -> Option<&'static str> {
    Some(match signal {
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGILL => "SIGILL",
        libc::SIGTRAP => "SIGTRAP",
        libc::SIGABRT => "SIGABRT",
        libc::SIGBUS => "SIGBUS",
        libc::SIGFPE => "SIGFPE",
        libc::SIGKILL => "SIGKILL",
        libc::SIGUSR1 => "SIGUSR1",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGUSR2 => "SIGUSR2",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGALRM => "SIGALRM",
        libc::SIGTERM => "SIGTERM",
        libc::SIGCHLD => "SIGCHLD",
        libc::SIGCONT => "SIGCONT",
        libc::SIGSTOP => "SIGSTOP",
        libc::SIGTSTP => "SIGTSTP",
        libc::SIGTTIN => "SIGTTIN",
        libc::SIGTTOU => "SIGTTOU",
        libc::SIGURG => "SIGURG",
        libc::SIGXCPU => "SIGXCPU",
        libc::SIGXFSZ => "SIGXFSZ",
        libc::SIGVTALRM => "SIGVTALRM",
        libc::SIGPROF => "SIGPROF",
        libc::SIGWINCH => "SIGWINCH",
        libc::SIGIO => "SIGIO",
        libc::SIGSYS => "SIGSYS",
        _ => return None,
    })
}

#[cfg(not(unix))]
fn signal_name(_signal: i32) -> Option<&'static str> {
    None
}

impl fmt::Display for ExitCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExitCause::Code(code) => write!(f, "exit code {code}"),
            ExitCause::Signal {
                number,
                name,
                core_dumped,
            } => {
                write!(f, "signal {number}")?;
                if let Some(name) = name {
                    write!(f, " ({name})")?;
                }
                if *core_dumped {
                    f.write_str(", core dumped")?;
                }
                Ok(())
            }
            ExitCause::HealthKill { unanswered_secs } => {
                write!(
                    f,
                    "killed after {unanswered_secs} s without an answered health check"
                )
            }
            ExitCause::Message(text) => f.write_str(text),
        }
    }
}

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
    /// is the default and is omitted from the wire, so an absent field decodes
    /// as wall time.
    #[serde(default, skip_serializing_if = "ClockBinding::is_wall")]
    pub clock: ClockBinding,
    /// Validator-resolved producers bound to each of this instance's
    /// `depends_on` slots, keyed by the consumer manifest's link id.
    /// Mirrors [`config::runtime::NodeInstanceConfig::slot_bindings`] and
    /// the `TrackedNodeInstance` this is produced from. Empty for
    /// instances whose manifest declares no `depends_on` slots, and then
    /// omitted from the wire, so an absent field decodes as empty.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub slot_bindings: SlotBindings,
    /// Live pairing-slot state for every `depends_on.pairings` entry, keyed
    /// by the slot's link_id. Overlaid by the daemon from the manifest plus
    /// its pairing registry when serializing the graph: this is the
    /// observability surface behind `peppy stack list`'s pairing rows.
    /// Empty for instances whose manifest declares no pairings, and then
    /// omitted from the wire, so an absent field decodes as empty.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pairing_slots: BTreeMap<String, SerializedPairingSlot>,
    /// The endpoints this instance serves, in label order: what `peppy stack
    /// list` prints so an operator finds the URLs without reading the node.
    /// Empty for a node whose manifest declares none, and always on the wire
    /// so a reader sees the set the daemon holds.
    #[serde(default)]
    pub endpoints: Vec<InstanceEndpoint>,
    /// The instance's restart policy and how its restarts went, its fields
    /// beside the others on the wire.
    #[serde(flatten)]
    pub restart_status: RestartStatus,
}

/// An instance's restart policy as the daemon resolved it and how its
/// restarts went, as `peppy stack list` and `peppy node info` report them.
/// Each field is absent from a daemon older than the policy and reads as
/// `never` with no restarts, since such a daemon restarts nothing, so a newer
/// `peppy` still lists a stale daemon's stack and names its version.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartStatus {
    /// The resolved policy, with its cap.
    #[serde(default = "older_daemon_restart")]
    pub restart: RestartDeclaration,
    /// Where the policy was written.
    #[serde(default)]
    pub restart_origin: RestartOrigin,
    /// Times a restarted process of the instance reached `running`.
    #[serde(default)]
    pub restart_count: u32,
    /// The attempts in a row `max_restarts` caps: 0 until the first exit
    /// the policy restarts; each such exit or failed start takes the next
    /// one, and an exit after a run of the reset window takes 1 again. An
    /// exit a removal asked for sets 0, and any other exit the policy does
    /// not restart leaves it as it was.
    #[serde(default)]
    pub attempt: u32,
    /// Why the last start ended; `None` while none has.
    #[serde(default)]
    pub last_exit: Option<ExitCause>,
    /// When the instance last reached `running`, in milliseconds since the
    /// Unix epoch.
    #[serde(default)]
    pub running_since_ms: Option<u64>,
    /// When the next start is due, in milliseconds since the Unix epoch,
    /// while the instance is `restarting`.
    #[serde(default)]
    pub next_start_ms: Option<u64>,
}

impl SerializedInstance {
    /// An instance with every field but its id and state at the value a fresh
    /// start gives it: healthy, wall clock, no bindings, pairs or endpoints,
    /// the built-in restart policy, no restarts.
    pub fn of(instance_id: String, state: InstanceState) -> Self {
        Self {
            instance_id,
            state,
            healthy: true,
            clock: ClockBinding::Wall,
            slot_bindings: BTreeMap::new(),
            pairing_slots: BTreeMap::new(),
            endpoints: Vec::new(),
            restart_status: RestartStatus::default(),
        }
    }
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
    /// no pair by design from one whose peer is missing. Omitted from the wire
    /// when `one`, which is also what an absent field decodes to.
    #[serde(default, skip_serializing_if = "Cardinality::is_one")]
    pub cardinality: Cardinality,
}

/// The policy of a daemon older than the restart fields, whose instances
/// are never restarted.
pub(crate) fn older_daemon_restart() -> RestartDeclaration {
    RestartDeclaration::policy_only(RestartPolicy::Never)
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

    /// Externally visible instance ids: the subset of `instances` that have
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
    /// `depends_on.nodes` edge. Omitted from the wire when `None`, which is
    /// also what an absent field decodes to.
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
    /// matches; returns `Ok(vec![])` when the node exists but none of its
    /// instances is `Running`.
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
    use config::node::RestartPolicy;
    use std::num::NonZeroU32;

    use config::runtime::ProducerRef;

    /// The wire form of an instance with one key removed, the payload an
    /// optional field reads back from when it is absent.
    fn without_key(json: &str, key: &str) -> String {
        let mut value: serde_json::Value = serde_json::from_str(json).expect("json");
        value.as_object_mut().expect("object").remove(key);
        value.to_string()
    }

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
                .map(|(id, st)| SerializedInstance::of((*id).into(), *st))
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
                    ("w1", InstanceState::Restarting),
                    ("d1", InstanceState::Finished),
                    ("f1", InstanceState::Failed),
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
    fn by_node_ok_empty_when_none_is_running() {
        let graph = SerializedNodeGraph {
            nodes: vec![make_node(
                "foo",
                "v1",
                &[
                    ("s1", InstanceState::Starting),
                    ("w1", InstanceState::Restarting),
                    ("d1", InstanceState::Finished),
                    ("f1", InstanceState::Failed),
                ],
            )],
            edges: vec![],
        };
        assert_eq!(graph.running_instance_ids_by_node("foo", "v1"), Ok(vec![]));
    }

    #[test]
    fn by_node_err_when_no_node_matches() {
        let foo_v1 = || vec![make_node("foo", "v1", &[("r1", InstanceState::Running)])];
        for (case, nodes, name, tag) in [
            ("name mismatch", foo_v1(), "bar", "v1"),
            ("tag mismatch", foo_v1(), "foo", "v2"),
            ("empty graph", vec![], "foo", "v1"),
        ] {
            let graph = SerializedNodeGraph {
                nodes,
                edges: vec![],
            };
            assert_eq!(
                graph.running_instance_ids_by_node(name, tag),
                Err(NodeNotFound::new(name, tag)),
                "{case}"
            );
        }
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
        // A multi-cardinality slot's ordered set, with the copy a member
        // belongs to, and a zero_or_more slot's empty set both round-trip.
        bindings.insert(
            "cameras".to_string(),
            BoundProducers::try_from(vec![
                config::runtime::BoundMember::from(ProducerRef::new("core_a", "cam-1")),
                config::runtime::BoundMember {
                    producer: ProducerRef::new("core_a", "bravo_cam"),
                    copy: Some(config::runtime::CopyTag {
                        name: config::runtime::Name::new("bravo").unwrap(),
                        instance_id: config::runtime::Name::new("cam").unwrap(),
                    }),
                },
            ])
            .expect("distinct producers"),
        );
        bindings.insert("spare_cameras".to_string(), BoundProducers::default());
        let instance = SerializedInstance {
            slot_bindings: bindings,
            ..SerializedInstance::of("i1".to_string(), InstanceState::Running)
        };

        let json = serde_json::to_string(&instance).expect("serialize");
        let decoded: SerializedInstance = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, instance);
    }

    #[test]
    fn instance_endpoints_round_trip_with_their_kind_and_are_always_on_the_wire() {
        let served = SerializedInstance {
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
            ..SerializedInstance::of("mcp".to_string(), InstanceState::Running)
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
        let instance = SerializedInstance::of("i1".to_string(), InstanceState::Running);
        let json = serde_json::to_string(&instance).expect("serialize");
        assert!(
            !json.contains("slot_bindings"),
            "empty bindings must be omitted from the wire form: {json}"
        );

        let absent = without_key(&json, "slot_bindings");
        let decoded: SerializedInstance = serde_json::from_str(&absent).expect("decode");
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
            pairing_slots,
            ..SerializedInstance::of("ctrl_1".to_string(), InstanceState::Running)
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
        let absent = without_key(&json, "pairing_slots");
        let decoded: SerializedInstance = serde_json::from_str(&absent).expect("decode");
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
                ("w1", InstanceState::Restarting),
                ("d1", InstanceState::Finished),
                ("f1", InstanceState::Failed),
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
            InstanceState::Restarting,
            InstanceState::Finished,
            InstanceState::Failed,
        ] {
            assert_eq!(state.to_string(), state.as_str());
            assert_eq!(state.as_str().parse::<InstanceState>(), Ok(state));
        }
        assert_eq!(InstanceState::Starting.as_str(), "starting");
        assert_eq!(InstanceState::Running.as_str(), "running");
        assert_eq!(InstanceState::Restarting.as_str(), "restarting");
        assert_eq!(InstanceState::Finished.as_str(), "finished");
        assert_eq!(InstanceState::Failed.as_str(), "failed");

        let err = "bogus"
            .parse::<InstanceState>()
            .expect_err("unknown must fail");
        assert_eq!(err, UnknownInstanceState("bogus".to_owned()));
        assert!(err.to_string().contains("bogus"), "got: {err}");
    }

    #[test]
    fn has_process_is_starting_or_running() {
        assert!(InstanceState::Starting.has_process());
        assert!(InstanceState::Running.has_process());
        assert!(!InstanceState::Restarting.has_process());
        assert!(!InstanceState::Finished.has_process());
        assert!(!InstanceState::Failed.has_process());
    }

    #[test]
    fn duplicate_instance_id_names_the_state_and_what_to_do() {
        let text = duplicate_instance_id_message(
            "cam_1",
            "camera",
            "v1",
            "cn-a",
            InstanceState::Restarting,
        );
        assert_eq!(
            text,
            "Instance ID `cam_1` already exists for node `camera:v1` and is restarting; `peppy node stop cam_1 --core-node cn-a` frees the id"
        );
        let text =
            duplicate_instance_id_message("cam_1", "camera", "v1", "cn-a", InstanceState::Running);
        assert_eq!(
            text,
            "Instance ID `cam_1` already exists for node `camera:v1` and is running; run under another id, or `peppy node stop cam_1 --core-node cn-a` first"
        );
        let text =
            duplicate_instance_id_message("cam_1", "camera", "v1", "cn-a", InstanceState::Starting);
        assert_eq!(
            text,
            "Instance ID `cam_1` already exists for node `camera:v1` and is starting; run under another id, or `peppy node stop cam_1 --core-node cn-a` once it is running"
        );
        for (state, word) in [
            (InstanceState::Finished, "finished"),
            (InstanceState::Failed, "failed"),
        ] {
            assert_eq!(
                duplicate_instance_id_message("cam_1", "camera", "v1", "cn-a", state),
                format!(
                    "Instance ID `cam_1` already exists for node `camera:v1` and is {word}; `peppy node stop cam_1 --core-node cn-a` frees the id"
                )
            );
        }
    }

    #[test]
    fn exit_cause_of_code_signal_health_kill_and_message() {
        // A wait status carries the exit code in bits 8..16 and a signal in
        // the low bits.
        let code = std::process::ExitStatus::from_raw(3 << 8);
        assert_eq!(ExitCause::of(&Ok(code)), ExitCause::Code(3));
        assert_eq!(ExitCause::Code(3).to_string(), "exit code 3");
        assert!(!ExitCause::Code(3).is_clean());
        assert!(ExitCause::of(&Ok(std::process::ExitStatus::from_raw(0))).is_clean());
        let signal = std::process::ExitStatus::from_raw(9);
        assert_eq!(ExitCause::of(&Ok(signal)), ExitCause::signal(9, false));
        assert_eq!(
            ExitCause::signal(9, false).to_string(),
            "signal 9 (SIGKILL)"
        );
        assert!(!ExitCause::signal(9, false).is_clean());
        // The core-dump flag is bit 7 of the wait status.
        let segfault = std::process::ExitStatus::from_raw(11 | 0x80);
        assert_eq!(
            ExitCause::of(&Ok(segfault)).to_string(),
            "signal 11 (SIGSEGV), core dumped"
        );
        assert_eq!(
            ExitCause::signal(200, false).to_string(),
            "signal 200",
            "a number this machine does not name prints alone"
        );
        let unread = ExitCause::of(&Err(std::io::Error::other("no status")));
        assert_eq!(
            unread,
            ExitCause::Message("could not read exit status: no status".to_owned())
        );
        assert_eq!(unread.to_string(), "could not read exit status: no status");
        let health_kill = ExitCause::HealthKill {
            unanswered_secs: 60,
        };
        assert_eq!(
            health_kill.to_string(),
            "killed after 60 s without an answered health check"
        );
        assert!(!health_kill.is_clean());
        assert_eq!(
            serde_json::to_value(&health_kill).expect("serialize"),
            serde_json::json!({ "health_kill": { "unanswered_secs": 60 } }),
            "`--json` carries the seconds unanswered as a field"
        );
    }

    /// A daemon older than the policy sends none of its fields; the reader
    /// fills `never`, since that daemon restarts nothing, so the listing
    /// still names it.
    #[test]
    fn serialized_instance_reads_a_payload_without_restart_fields_as_never() {
        let json = serde_json::to_string(&SerializedInstance::of(
            "i1".to_string(),
            InstanceState::Running,
        ))
        .expect("serialize");
        let mut stripped: serde_json::Value = serde_json::from_str(&json).expect("json");
        let object = stripped.as_object_mut().expect("object");
        for key in [
            "restart",
            "restart_origin",
            "restart_count",
            "attempt",
            "last_exit",
            "running_since_ms",
            "next_start_ms",
        ] {
            object.remove(key);
        }
        let decoded: SerializedInstance =
            serde_json::from_str(&stripped.to_string()).expect("deserialize");
        let mut never = SerializedInstance::of("i1".to_string(), InstanceState::Running);
        never.restart_status.restart = RestartDeclaration::policy_only(RestartPolicy::Never);
        assert_eq!(decoded, never);
    }

    #[test]
    fn serialized_instance_restart_fields_round_trip() {
        let mut instance = SerializedInstance::of("i1".to_string(), InstanceState::Restarting);
        instance.restart_status.restart =
            RestartDeclaration::new(RestartPolicy::OnFailure, NonZeroU32::new(5)).unwrap();
        instance.restart_status.restart_origin = RestartOrigin::Node;
        instance.restart_status.restart_count = 2;
        instance.restart_status.attempt = 3;
        instance.restart_status.last_exit = Some(ExitCause::signal(9, false));
        instance.restart_status.running_since_ms = Some(1_700_000_000_000);
        instance.restart_status.next_start_ms = Some(1_700_000_004_000);
        let json = serde_json::to_string(&instance).expect("serialize");
        // The restart fields sit beside the instance's own, at its top level.
        let value: serde_json::Value = serde_json::from_str(&json).expect("a JSON object");
        for key in [
            "restart",
            "restart_origin",
            "restart_count",
            "attempt",
            "last_exit",
            "running_since_ms",
            "next_start_ms",
        ] {
            assert!(value.get(key).is_some(), "`{key}` at the top level: {json}");
        }
        assert_eq!(value["restart_count"], 2);
        assert!(
            json.contains(r#""restart":{"policy":"on_failure","max_restarts":5}"#),
            "{json}"
        );
        assert!(json.contains(r#""restart_origin":"node""#), "{json}");
        assert!(
            json.contains(
                r#""last_exit":{"signal":{"number":9,"name":"SIGKILL","core_dumped":false}}"#
            ),
            "{json}"
        );
        let decoded: SerializedInstance = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, instance);
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
            (InstanceState::Restarting, "\"restarting\""),
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
        let without_core_node = r#"{"name":"n","tag":"v1","config_path":"","artifact_path":null}"#;
        let error = serde_json::from_str::<SerializedNode>(without_core_node)
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
