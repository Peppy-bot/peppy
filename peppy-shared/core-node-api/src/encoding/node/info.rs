//! Cap'n Proto encoding utilities for node info messages.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::str::FromStr;

use capnp::message::Builder;
use config::node::{NodeConfig, RestartDeclaration, RestartOrigin};
use config::runtime::{ClockBinding, SlotBindings};

use crate::graph::{
    ExitCause, InstanceEndpoint, InstanceState, NodeStage, RestartStatus, SerializedPairingSlot,
};
use crate::node_capnp;
use crate::{Payload, Result};

use crate::encoding::endpoints::{read_node_endpoints, write_node_endpoints};
use crate::encoding::{
    capnp_list_len, decode_message, encode_message, optional_text, read_text_list, write_text_list,
};

/// Request payload for the `node_info` service.
///
/// Identifies a node already present in the node stack by `(name, tag)`.
/// Unlike `node_add`, `node_info` does not resolve configs from filesystem,
/// git, or HTTP sources: it only inspects what is already in the stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeInfoRequest {
    pub node_name: String,
    pub node_tag: String,
}

impl NodeInfoRequest {
    pub fn new(node_name: impl Into<String>, node_tag: impl Into<String>) -> Self {
        Self {
            node_name: node_name.into(),
            node_tag: node_tag.into(),
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<node_capnp::node_info_request::Builder>();
            request.set_node_name(&self.node_name);
            request.set_node_tag(&self.node_tag);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<node_capnp::node_info_request::Reader>()?;
        Ok(Self {
            node_name: request.get_node_name()?.to_str()?.to_owned(),
            node_tag: request.get_node_tag()?.to_str()?.to_owned(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeInstanceInfo {
    pub instance_id: String,
    pub state: InstanceState,
    /// Liveness from the daemon's most recent `node_health` probe for this
    /// instance: `true` while it answers within the probe timeout, `false`
    /// once a probe fails. Surfaced by `peppy node info` so a failing instance
    /// is visible without a separate health round-trip. Decodes to `true` when
    /// absent (an older producer) rather than flagging the instance unhealthy.
    pub healthy: bool,
    /// Producers bound to each of this consumer instance's `depends_on`
    /// slots, mirroring
    /// [`config::runtime::NodeInstanceConfig::slot_bindings`].
    /// Empty when the node has no `depends_on` slots. Surfacing this
    /// lets the launcher / CLI cross-check newly-staged binding plans
    /// against what running consumers have already claimed.
    pub slot_bindings: SlotBindings,
    /// Live pairing-slot state per `depends_on.pairings` entry, mirroring
    /// [`crate::graph::SerializedInstance::pairing_slots`]. Empty when the
    /// node declares no pairings. Lets the CLI's `--link` preflight see
    /// which slots of a running instance are already claimed.
    pub pairing_slots: BTreeMap<String, SerializedPairingSlot>,
    /// The one clock this instance reads. Lets a `node run` preflight hold a
    /// new instance's connections to the same clock rule a launch applies,
    /// against the instances already running.
    pub clock: ClockBinding,
    /// The endpoints the instance serves, in label order, mirroring
    /// [`crate::graph::SerializedInstance::endpoints`]. Empty for a node
    /// whose manifest declares none.
    pub endpoints: Vec<InstanceEndpoint>,
    /// The instance's restart policy and how its restarts went.
    pub restart_status: crate::graph::RestartStatus,
}

/// Body of a successful `node_info` lookup: carries all metadata about a
/// node that was found in the stack.
#[derive(Debug, Clone)]
pub struct NodeInfo {
    /// Resolved NodeConfig as stored in the node stack.
    pub config: NodeConfig,
    /// SHA256 of the serialized NodeConfig at the time of the response.
    pub config_integrity: String,
    pub stage: NodeStage,
    /// All tracked instances of this entity, including in-flight `Starting`
    /// ones, with their per-instance state.
    pub instances: Vec<NodeInstanceInfo>,
    /// Most-recent add/build log file produced for this entity, if any.
    pub add_log_path: Option<PathBuf>,
    /// Per-instance run log paths, aligned with `instances` (same order).
    pub run_log_paths: Vec<PathBuf>,
    /// The daemon's clock when it answered, in milliseconds since the Unix
    /// epoch: what each instance's `next_start_ms` counts down against.
    pub reported_at_ms: u64,
}

/// Response payload for the `node_info` service.
///
/// `NotInStack` is a first-class *successful* negative answer to the lookup,
/// not a protocol-level error. Keeping "no such node" apart from "malformed
/// request" lets a normal flow, such as the preflight inside
/// `peppy node add`, look a node up without an ERROR log.
#[derive(Debug, Clone)]
pub enum NodeInfoResponse {
    /// The `(name, tag)` pair is not currently in the node stack.
    NotInStack,
    /// The node is in the stack; carries its full metadata.
    Found(Box<NodeInfo>),
}

impl NodeInfoResponse {
    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<node_capnp::node_info_response::Builder>();
            match self {
                // Select the `notInStack :Void` arm of the union.
                NodeInfoResponse::NotInStack => response.set_not_in_stack(()),
                NodeInfoResponse::Found(info) => write_node_info(response.init_found(), info)?,
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<node_capnp::node_info_response::Reader>()?;
        match response.which()? {
            node_capnp::node_info_response::Which::NotInStack(()) => {
                Ok(NodeInfoResponse::NotInStack)
            }
            node_capnp::node_info_response::Which::Found(found) => {
                Ok(NodeInfoResponse::Found(Box::new(read_node_info(found)?)))
            }
        }
    }
}

/// Writes a found node into the `found` arm of a `NodeInfoResponse`.
fn write_node_info(
    mut found: node_capnp::node_info_response::found::Builder<'_>,
    info: &NodeInfo,
) -> Result<()> {
    // `run_log_paths` is documented to be order-aligned with `instances`.
    // Reject a mismatch here rather than emit a payload that would silently
    // misassociate logs on decode.
    if info.run_log_paths.len() != info.instances.len() {
        return Err(crate::Error::Encoding(format!(
            "run_log_paths length ({}) does not match instances length ({})",
            info.run_log_paths.len(),
            info.instances.len()
        )));
    }
    let config_json5 = serde_json5::to_string(&info.config)
        .map_err(|e| crate::Error::Encoding(format!("failed to serialize config: {}", e)))?;
    found.set_config_json5(&config_json5);
    found.set_config_sha256(&info.config_integrity);
    found.set_stage(info.stage.as_str());
    found.set_reported_at_ms(info.reported_at_ms);
    let instance_count = capnp_list_len(info.instances.len(), "NodeInfo.instances")?;
    let mut instances = found.reborrow().init_instances(instance_count);
    for (index, instance) in info.instances.iter().enumerate() {
        write_node_instance(instances.reborrow().get(index as u32), instance)?;
    }
    // Only set the field when present; leaving it unset writes capnp's
    // empty-text default, which `optional_text` decodes back to `None`.
    // Borrows via `to_string_lossy` (no owning allocation) like the
    // `run_log_paths` list below.
    if let Some(path) = &info.add_log_path {
        found.set_add_log_path(path.to_string_lossy().as_ref());
    }
    let run_log_path_count = capnp_list_len(info.run_log_paths.len(), "NodeInfo.run_log_paths")?;
    write_text_list(
        found.init_run_log_paths(run_log_path_count),
        info.run_log_paths.iter().map(|path| path.to_string_lossy()),
    );
    Ok(())
}

/// Inverse of [`write_node_info`].
fn read_node_info(found: node_capnp::node_info_response::found::Reader<'_>) -> Result<NodeInfo> {
    let config: NodeConfig = serde_json5::from_str(found.get_config_json5()?.to_str()?)
        .map_err(|e| crate::Error::Decoding(format!("failed to deserialize config: {}", e)))?;
    let config_integrity = found.get_config_sha256()?.to_str()?.to_owned();
    let stage = NodeStage::from_str(found.get_stage()?.to_str()?)
        .map_err(|e| crate::Error::Decoding(e.to_string()))?;
    let instances = found
        .get_instances()?
        .iter()
        .map(read_node_instance)
        .collect::<Result<Vec<_>>>()?;
    let add_log_path = optional_text(found.get_add_log_path()?.to_str()?).map(PathBuf::from);
    let run_log_paths: Vec<PathBuf> = read_text_list(found.get_run_log_paths()?)?
        .into_iter()
        .map(PathBuf::from)
        .collect();
    // `run_log_paths` is documented to be order-aligned with `instances`.
    // Reject a mismatched payload instead of handing back logs misassociated
    // with the wrong instances.
    if run_log_paths.len() != instances.len() {
        return Err(crate::Error::Decoding(format!(
            "run_log_paths length ({}) does not match instances length ({})",
            run_log_paths.len(),
            instances.len()
        )));
    }
    Ok(NodeInfo {
        config,
        config_integrity,
        stage,
        instances,
        add_log_path,
        run_log_paths,
        reported_at_ms: found.get_reported_at_ms(),
    })
}

/// Writes one instance into an initialized `NodeInstanceInfo` builder.
fn write_node_instance(
    mut entry: node_capnp::node_instance_info::Builder<'_>,
    instance: &NodeInstanceInfo,
) -> Result<()> {
    let instance_id = &instance.instance_id;
    entry.set_instance_id(instance_id);
    entry.set_state(instance.state.as_str());
    entry.set_healthy(instance.healthy);
    entry.set_slot_bindings_json(&serialize_instance_field(
        &instance.slot_bindings,
        "slot_bindings",
        instance_id,
    )?);
    entry.set_pairing_slots_json(&serialize_instance_field(
        &instance.pairing_slots,
        "pairing_slots",
        instance_id,
    )?);
    entry.set_clock_json(&serialize_instance_field(
        &instance.clock,
        "clock",
        instance_id,
    )?);
    let endpoint_count = capnp_list_len(instance.endpoints.len(), "NodeInstanceInfo.endpoints")?;
    write_node_endpoints(
        entry.reborrow().init_endpoints(endpoint_count),
        &instance.endpoints,
    )?;
    let status = &instance.restart_status;
    let (policy, max_restarts) = super::run::declaration_on_wire(status.restart);
    entry.set_restart(policy);
    entry.set_max_restarts(max_restarts);
    entry.set_restart_origin(status.restart_origin.as_str());
    entry.set_restart_count(status.restart_count);
    entry.set_attempt(status.attempt);
    write_exit_cause(entry.reborrow().init_last_exit(), status.last_exit.as_ref());
    entry.set_running_since_ms(status.running_since_ms.unwrap_or(0));
    entry.set_next_start_ms(status.next_start_ms.unwrap_or(0));
    Ok(())
}

/// Inverse of [`write_node_instance`].
fn read_node_instance(
    entry: node_capnp::node_instance_info::Reader<'_>,
) -> Result<NodeInstanceInfo> {
    let state = InstanceState::from_str(entry.get_state()?.to_str()?)
        .map_err(|e| crate::Error::Decoding(e.to_string()))?;
    let slot_bindings =
        deserialize_instance_field(entry.get_slot_bindings_json()?.to_str()?, "slot_bindings")?;
    let pairing_slots =
        deserialize_instance_field(entry.get_pairing_slots_json()?.to_str()?, "pairing_slots")?;
    let clock = deserialize_instance_field(entry.get_clock_json()?.to_str()?, "clock")?;
    let restart =
        read_restart_declaration(entry.get_restart()?.to_str()?, entry.get_max_restarts())?;
    let restart_origin = read_restart_origin(entry.get_restart_origin()?.to_str()?)?;
    Ok(NodeInstanceInfo {
        instance_id: entry.get_instance_id()?.to_str()?.to_owned(),
        state,
        healthy: entry.get_healthy(),
        slot_bindings,
        pairing_slots,
        clock,
        endpoints: read_node_endpoints(entry.get_endpoints()?)?,
        restart_status: RestartStatus {
            restart,
            restart_origin,
            restart_count: entry.get_restart_count(),
            attempt: entry.get_attempt(),
            last_exit: read_exit_cause(entry.get_last_exit()?)?,
            running_since_ms: wire_time(entry.get_running_since_ms()),
            next_start_ms: wire_time(entry.get_next_start_ms()),
        },
    })
}

/// Serializes one JSON5 field of an instance. The default value (no slot
/// bindings, no pairing slots, the wall clock) travels as empty text, which
/// [`deserialize_instance_field`] reads back as that default.
fn serialize_instance_field<T: serde::Serialize + Default + PartialEq>(
    value: &T,
    field: &str,
    instance_id: &str,
) -> Result<String> {
    if *value == T::default() {
        return Ok(String::new());
    }
    serde_json5::to_string(value).map_err(|e| {
        crate::Error::Encoding(format!(
            "failed to serialize {field} for instance `{instance_id}`: {e}"
        ))
    })
}

/// Inverse of [`serialize_instance_field`].
fn deserialize_instance_field<T: serde::de::DeserializeOwned + Default>(
    json5: &str,
    field: &str,
) -> Result<T> {
    if json5.is_empty() {
        return Ok(T::default());
    }
    serde_json5::from_str(json5)
        .map_err(|e| crate::Error::Decoding(format!("failed to deserialize {field}: {e}")))
}

/// A time the wire carries as milliseconds since the Unix epoch, `0` for
/// none.
fn wire_time(ms: u64) -> Option<u64> {
    (ms != 0).then_some(ms)
}

/// Reads a declaration from its two wire fields; an empty policy is a daemon
/// older than the policy, whose instances are never restarted.
fn read_restart_declaration(policy: &str, max_restarts: u32) -> Result<RestartDeclaration> {
    Ok(super::run::read_declaration(policy, max_restarts)?
        .unwrap_or_else(crate::graph::older_daemon_restart))
}

/// Reads an origin; an empty one is a daemon older than the policy.
fn read_restart_origin(origin: &str) -> Result<RestartOrigin> {
    if origin.is_empty() {
        return Ok(RestartOrigin::Default);
    }
    RestartOrigin::from_str(origin).map_err(|e| crate::Error::Decoding(e.to_string()))
}

/// Writes an exit cause, or the `none` arm while no process has exited.
fn write_exit_cause(mut builder: node_capnp::exit_cause::Builder<'_>, cause: Option<&ExitCause>) {
    match cause {
        None => builder.set_none(()),
        Some(ExitCause::Code(code)) => builder.set_code(*code),
        Some(ExitCause::Signal {
            number,
            name,
            core_dumped,
        }) => {
            let mut group = builder.init_signal();
            group.set_number(*number);
            group.set_name(name.as_deref().unwrap_or_default());
            group.set_core_dumped(*core_dumped);
        }
        Some(ExitCause::HealthKill { unanswered_secs }) => {
            builder.set_health_kill_secs(*unanswered_secs)
        }
        Some(ExitCause::Message(text)) => builder.set_message(text),
    }
}

/// Inverse of [`write_exit_cause`].
fn read_exit_cause(reader: node_capnp::exit_cause::Reader<'_>) -> Result<Option<ExitCause>> {
    use node_capnp::exit_cause::Which;
    Ok(match reader.which()? {
        Which::None(()) => None,
        Which::Code(code) => Some(ExitCause::Code(code)),
        Which::Signal(group) => {
            let name = group.get_name()?.to_str()?;
            Some(ExitCause::Signal {
                number: group.get_number(),
                name: (!name.is_empty()).then(|| name.to_owned()),
                core_dumped: group.get_core_dumped(),
            })
        }
        Which::HealthKillSecs(unanswered_secs) => Some(ExitCause::HealthKill { unanswered_secs }),
        Which::Message(text) => Some(ExitCause::Message(text?.to_str()?.to_owned())),
    })
}

impl crate::encoding::Wire for NodeInfoRequest {
    type Root = crate::node_capnp::node_info_request::Owned;
}

impl crate::encoding::Wire for NodeInfoResponse {
    type Root = crate::node_capnp::node_info_response::Owned;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::decoding_error;
    use config::node::{NodeConfigParser, RestartPolicy};
    use config::runtime::ProducerRef;
    use std::num::NonZeroU32;

    #[test]
    fn node_info_request_roundtrips_name_tag() {
        let encoded = NodeInfoRequest::new("sensor_node", "v1")
            .encode()
            .expect("encoding should succeed");
        let decoded = NodeInfoRequest::decode(&encoded).expect("decoding should succeed");

        assert_eq!(decoded.node_name, "sensor_node");
        assert_eq!(decoded.node_tag, "v1");
    }

    fn sample_config_for_roundtrip() -> NodeConfig {
        let config_json5 = r#"{
            peppy_schema: "node/v1",
            manifest: { name: "sensor_node", tag: "v1" },
            execution: { language: "rust", run_cmd: ["sleep", "10"] }
        }"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("peppy.json5");
        std::fs::write(&path, config_json5).expect("write config");
        NodeConfigParser::from_path(&path).expect("parse config")
    }

    #[test]
    fn node_info_response_found_roundtrips() {
        let info = NodeInfo {
            config: sample_config_for_roundtrip(),
            config_integrity: "0".repeat(64),
            stage: NodeStage::Added,
            instances: vec![],
            add_log_path: None,
            run_log_paths: vec![],
            reported_at_ms: 1_700_000_000_000,
        };
        let encoded = NodeInfoResponse::Found(Box::new(info))
            .encode()
            .expect("encoding should succeed");
        let decoded = NodeInfoResponse::decode(&encoded).expect("decoding should succeed");

        match decoded {
            NodeInfoResponse::Found(info) => {
                assert_eq!(info.config.manifest.name.as_str(), "sensor_node");
                assert_eq!(info.stage, NodeStage::Added);
                assert_eq!(info.reported_at_ms, 1_700_000_000_000);
            }
            NodeInfoResponse::NotInStack => panic!("expected Found"),
        }
    }

    #[test]
    fn node_info_response_roundtrips_instance_slot_bindings() {
        let bindings_a: config::runtime::SlotBindings = [
            (
                "wrist_left_camera".to_string(),
                config::runtime::BoundProducers::from(ProducerRef::new("core_a", "cam1")),
            ),
            // A multi-cardinality slot's ordered set must survive the trip,
            // with the copy each member belongs to.
            (
                "wrist_right_camera".to_string(),
                config::runtime::BoundProducers::try_from(vec![
                    config::runtime::BoundMember::from(ProducerRef::new("core_a", "cam2")),
                    config::runtime::BoundMember {
                        producer: ProducerRef::new("core_a", "bravo_cam"),
                        copy: Some(config::runtime::CopyTag {
                            name: config::runtime::Name::new("bravo").unwrap(),
                            instance_id: config::runtime::Name::new("cam").unwrap(),
                        }),
                    },
                ])
                .expect("distinct producers"),
            ),
        ]
        .into_iter()
        .collect();
        let pairing_slots_a: BTreeMap<String, SerializedPairingSlot> = [
            (
                "arm".to_string(),
                SerializedPairingSlot {
                    pairing_name: "arm_link".to_string(),
                    pairing_tag: "v1".to_string(),
                    role: "controller".to_string(),
                    cardinality: config::node::Cardinality::One,
                    peers: vec![config::runtime::PairedPeer {
                        peer: ProducerRef::new("core_a", "arm_1"),
                        peer_link_id: "controller".to_string(),
                        copy: None,
                    }],
                },
            ),
            (
                "gripper".to_string(),
                SerializedPairingSlot {
                    pairing_name: "gripper_link".to_string(),
                    pairing_tag: "v1".to_string(),
                    role: "controller".to_string(),
                    cardinality: config::node::Cardinality::ZeroOrMore,
                    peers: Vec::new(),
                },
            ),
        ]
        .into_iter()
        .collect();
        let info = NodeInfo {
            config: sample_config_for_roundtrip(),
            config_integrity: "0".repeat(64),
            stage: NodeStage::Ready,
            instances: vec![
                NodeInstanceInfo {
                    instance_id: "inst-with-bindings".to_string(),
                    state: InstanceState::Running,
                    healthy: true,
                    slot_bindings: bindings_a.clone(),
                    pairing_slots: pairing_slots_a.clone(),
                    clock: ClockBinding::consumer(
                        config::runtime::ClockDomainId::new(
                            config::runtime::Name::new("robot").expect("valid name"),
                            config::runtime::CoreNodeName::new("cn-sim").expect("valid name"),
                            config::runtime::ClockIncarnation::try_from(7).expect("non-zero"),
                        ),
                        config::runtime::ProducerRef::new("cn-sim", "sim_inst"),
                    ),
                    endpoints: crate::encoding::endpoints::sample_endpoints(),
                    restart_status: RestartStatus {
                        restart: RestartDeclaration::new(
                            RestartPolicy::OnFailure,
                            NonZeroU32::new(5),
                        )
                        .unwrap(),
                        restart_origin: RestartOrigin::Instance,
                        restart_count: 2,
                        attempt: 3,
                        last_exit: Some(ExitCause::signal(9, false)),
                        running_since_ms: Some(1_700_000_000_000),
                        next_start_ms: Some(1_700_000_004_000),
                    },
                },
                NodeInstanceInfo {
                    instance_id: "inst-no-bindings".to_string(),
                    state: InstanceState::Starting,
                    healthy: false,
                    slot_bindings: BTreeMap::new(),
                    pairing_slots: BTreeMap::new(),
                    clock: ClockBinding::Wall,
                    endpoints: Vec::new(),
                    restart_status: RestartStatus::default(),
                },
            ],
            add_log_path: None,
            // Kept order-aligned with `instances` so encode/decode accept it.
            run_log_paths: vec![
                PathBuf::from("/var/log/inst-with-bindings.log"),
                PathBuf::from("/var/log/inst-no-bindings.log"),
            ],
            reported_at_ms: 1_700_000_000_000,
        };
        let encoded = NodeInfoResponse::Found(Box::new(info))
            .encode()
            .expect("encoding should succeed");
        let decoded = NodeInfoResponse::decode(&encoded).expect("decoding should succeed");

        match decoded {
            NodeInfoResponse::Found(info) => {
                assert_eq!(info.instances.len(), 2);
                assert_eq!(
                    info.instances[0].slot_bindings, bindings_a,
                    "slot_bindings should round-trip for the first instance"
                );
                assert_eq!(
                    info.instances[0].endpoints,
                    crate::encoding::endpoints::sample_endpoints(),
                    "endpoints round-trip with their kind and every URL"
                );
                assert!(
                    info.instances[1].endpoints.is_empty(),
                    "a node that declares no endpoint round-trips the empty set"
                );
                assert!(
                    info.instances[1].slot_bindings.is_empty(),
                    "empty slot_bindings should round-trip as empty"
                );
                assert_eq!(
                    info.instances[0].pairing_slots, pairing_slots_a,
                    "pairing_slots should round-trip for the first instance"
                );
                assert!(
                    info.instances[1].pairing_slots.is_empty(),
                    "empty pairing_slots should round-trip as empty"
                );
                assert!(
                    info.instances[0].healthy,
                    "healthy=true should round-trip for the first instance"
                );
                assert!(
                    !info.instances[1].healthy,
                    "healthy=false should round-trip for the second instance"
                );
            }
            NodeInfoResponse::NotInStack => panic!("expected Found"),
        }
    }

    /// The clock, the state and the log paths of each instance survive the
    /// trip, a wall clock included.
    #[test]
    fn node_info_response_roundtrips_clock_state_and_log_paths() {
        let sim_clock = ClockBinding::publisher(config::runtime::ClockDomainId::new(
            config::runtime::Name::new("robot").expect("valid name"),
            config::runtime::CoreNodeName::new("cn-sim").expect("valid name"),
            config::runtime::ClockIncarnation::try_from(7).expect("non-zero"),
        ));
        let instance =
            |instance_id: &str, state: InstanceState, clock: ClockBinding| NodeInstanceInfo {
                instance_id: instance_id.to_string(),
                state,
                healthy: true,
                slot_bindings: BTreeMap::new(),
                pairing_slots: BTreeMap::new(),
                clock,
                endpoints: Vec::new(),
                restart_status: RestartStatus::default(),
            };
        let info = NodeInfo {
            config: sample_config_for_roundtrip(),
            config_integrity: "0".repeat(64),
            stage: NodeStage::Ready,
            instances: vec![
                instance("sim_inst", InstanceState::Running, sim_clock.clone()),
                instance("wall_inst", InstanceState::Starting, ClockBinding::Wall),
            ],
            add_log_path: Some(PathBuf::from("/var/log/add.log")),
            run_log_paths: vec![
                PathBuf::from("/var/log/sim_inst.log"),
                PathBuf::from("/var/log/wall_inst.log"),
            ],
            reported_at_ms: 1_700_000_000_000,
        };
        let encoded = NodeInfoResponse::Found(Box::new(info))
            .encode()
            .expect("encoding should succeed");
        let NodeInfoResponse::Found(decoded) =
            NodeInfoResponse::decode(&encoded).expect("decoding should succeed")
        else {
            panic!("expected Found");
        };
        assert_eq!(decoded.config_integrity, "0".repeat(64));
        assert_eq!(decoded.stage, NodeStage::Ready);
        assert_eq!(decoded.instances[0].instance_id, "sim_inst");
        assert_eq!(decoded.instances[0].state, InstanceState::Running);
        assert_eq!(decoded.instances[0].clock, sim_clock);
        assert_eq!(decoded.instances[1].instance_id, "wall_inst");
        assert_eq!(decoded.instances[1].state, InstanceState::Starting);
        assert_eq!(decoded.instances[1].clock, ClockBinding::Wall);
        assert_eq!(
            decoded.add_log_path,
            Some(PathBuf::from("/var/log/add.log"))
        );
        assert_eq!(
            decoded.run_log_paths,
            vec![
                PathBuf::from("/var/log/sim_inst.log"),
                PathBuf::from("/var/log/wall_inst.log"),
            ]
        );
    }

    /// A found response with one instance, hand-built so a test can write
    /// what the encoder never writes.
    fn found_payload_with_one_instance(
        edit_instance: impl FnOnce(node_capnp::node_instance_info::Builder<'_>),
        run_log_path_count: u32,
    ) -> Payload {
        let mut builder = Builder::new_default();
        {
            let response = builder.init_root::<node_capnp::node_info_response::Builder>();
            let mut found = response.init_found();
            found.set_config_json5(
                serde_json5::to_string(&sample_config_for_roundtrip()).expect("serialize config"),
            );
            found.set_stage(NodeStage::Ready.as_str());
            let mut entry = found.reborrow().init_instances(1).get(0);
            entry.set_instance_id("inst_1");
            entry.set_state(InstanceState::Running.as_str());
            edit_instance(entry);
            let mut paths = found.init_run_log_paths(run_log_path_count);
            for index in 0..run_log_path_count {
                paths.set(index, "/var/log/inst_1.log");
            }
        }
        encode_message(&builder).expect("encode")
    }

    #[test]
    fn node_info_response_decode_names_the_json5_field_it_cannot_read() {
        type Corrupt = fn(node_capnp::node_instance_info::Builder<'_>);
        let fields: [(&str, Corrupt); 3] = [
            ("slot_bindings", |mut entry| {
                entry.set_slot_bindings_json("{")
            }),
            ("pairing_slots", |mut entry| {
                entry.set_pairing_slots_json("{")
            }),
            ("clock", |mut entry| entry.set_clock_json("{")),
        ];
        for (field, corrupt) in fields {
            let payload = found_payload_with_one_instance(corrupt, 1);
            let message = decoding_error(NodeInfoResponse::decode(&payload));
            assert!(
                message.starts_with(&format!("failed to deserialize {field}: ")),
                "got: {message}"
            );
        }
    }

    /// `run_log_paths` is order-aligned with `instances`, so a list of
    /// another length is refused in both directions.
    #[test]
    fn node_info_response_refuses_run_log_paths_out_of_step_with_instances() {
        let info = NodeInfo {
            config: sample_config_for_roundtrip(),
            config_integrity: "0".repeat(64),
            stage: NodeStage::Added,
            instances: vec![],
            add_log_path: None,
            run_log_paths: vec![PathBuf::from("/var/log/orphan.log")],
            reported_at_ms: 1_700_000_000_000,
        };
        let error = NodeInfoResponse::Found(Box::new(info))
            .encode()
            .expect_err("a log path without an instance must be refused");
        assert_eq!(
            error.to_string(),
            "encoding error: run_log_paths length (1) does not match instances length (0)"
        );

        let payload = found_payload_with_one_instance(|_| {}, 0);
        assert_eq!(
            decoding_error(NodeInfoResponse::decode(&payload)),
            "run_log_paths length (0) does not match instances length (1)"
        );
    }

    #[test]
    fn node_info_response_not_in_stack_roundtrips() {
        let encoded = NodeInfoResponse::NotInStack
            .encode()
            .expect("encoding should succeed");
        let decoded = NodeInfoResponse::decode(&encoded).expect("decoding should succeed");

        match decoded {
            NodeInfoResponse::NotInStack => {}
            NodeInfoResponse::Found(_) => panic!("expected NotInStack"),
        }
    }

    #[test]
    fn node_info_response_decode_rejects_malformed_bytes() {
        assert!(NodeInfoResponse::decode(&[0xde, 0xad, 0xbe, 0xef]).is_err());
    }

    /// The restart fields survive the wire for every exit cause, and an
    /// instance that never ran reads back with no cap, no exit and no times.
    #[test]
    fn node_instance_info_restart_fields_round_trip() {
        let instance = |last_exit: Option<ExitCause>| NodeInstanceInfo {
            instance_id: "inst".to_string(),
            state: InstanceState::Restarting,
            healthy: false,
            slot_bindings: BTreeMap::new(),
            pairing_slots: BTreeMap::new(),
            clock: ClockBinding::Wall,
            endpoints: Vec::new(),
            restart_status: RestartStatus {
                restart: RestartDeclaration::new(RestartPolicy::Always, NonZeroU32::new(3))
                    .unwrap(),
                restart_origin: RestartOrigin::Node,
                restart_count: 4,
                attempt: 2,
                last_exit,
                running_since_ms: Some(1_700_000_000_000),
                next_start_ms: Some(1_700_000_002_000),
            },
        };
        let causes = [
            None,
            Some(ExitCause::Code(3)),
            Some(ExitCause::signal(11, true)),
            Some(ExitCause::HealthKill {
                unanswered_secs: 60,
            }),
            Some(ExitCause::Message("spawn failed".to_string())),
        ];
        let info = NodeInfo {
            config: sample_config_for_roundtrip(),
            config_integrity: "0".repeat(64),
            stage: NodeStage::Ready,
            instances: causes.iter().cloned().map(instance).collect(),
            add_log_path: None,
            run_log_paths: causes
                .iter()
                .map(|_| PathBuf::from("/var/log/inst.log"))
                .collect(),
            reported_at_ms: 1_700_000_000_000,
        };
        let expected = info.instances.clone();
        let encoded = NodeInfoResponse::Found(Box::new(info))
            .encode()
            .expect("encode");
        let NodeInfoResponse::Found(decoded) = NodeInfoResponse::decode(&encoded).expect("decode")
        else {
            panic!("expected Found");
        };
        assert_eq!(decoded.instances, expected);

        let never_ran = NodeInstanceInfo {
            state: InstanceState::Starting,
            restart_status: RestartStatus::default(),
            ..instance(None)
        };
        let info = NodeInfo {
            config: sample_config_for_roundtrip(),
            config_integrity: "0".repeat(64),
            stage: NodeStage::Ready,
            instances: vec![never_ran.clone()],
            add_log_path: None,
            run_log_paths: vec![PathBuf::from("/var/log/inst.log")],
            reported_at_ms: 1_700_000_000_000,
        };
        let encoded = NodeInfoResponse::Found(Box::new(info))
            .encode()
            .expect("encode");
        let NodeInfoResponse::Found(decoded) = NodeInfoResponse::decode(&encoded).expect("decode")
        else {
            panic!("expected Found");
        };
        assert_eq!(decoded.instances, [never_ran]);
    }

    /// A daemon older than the restart fields writes none: its instances
    /// read as `never`, written by nobody, since that daemon restarts nothing.
    #[test]
    fn node_instance_info_without_restart_fields_reads_never() {
        let instance = NodeInstanceInfo {
            instance_id: "inst".to_string(),
            state: InstanceState::Running,
            healthy: true,
            slot_bindings: BTreeMap::new(),
            pairing_slots: BTreeMap::new(),
            clock: ClockBinding::Wall,
            endpoints: Vec::new(),
            restart_status: RestartStatus {
                restart: RestartDeclaration::policy_only(RestartPolicy::Always),
                restart_origin: RestartOrigin::Instance,
                restart_count: 0,
                attempt: 0,
                last_exit: None,
                running_since_ms: None,
                next_start_ms: None,
            },
        };
        let info = NodeInfo {
            config: sample_config_for_roundtrip(),
            config_integrity: "0".repeat(64),
            stage: NodeStage::Ready,
            instances: vec![instance],
            add_log_path: None,
            run_log_paths: vec![PathBuf::from("/var/log/inst.log")],
            reported_at_ms: 1_700_000_000_000,
        };
        let encoded = NodeInfoResponse::Found(Box::new(info))
            .encode()
            .expect("encode");
        let reader = decode_message(&encoded).expect("a message");
        let mut builder = Builder::new_default();
        builder
            .set_root(
                reader
                    .get_root::<node_capnp::node_info_response::Reader>()
                    .expect("a response"),
            )
            .expect("a copy");
        {
            let root = builder
                .get_root::<node_capnp::node_info_response::Builder>()
                .expect("a response");
            let node_capnp::node_info_response::Which::Found(found) =
                root.which().expect("a union")
            else {
                panic!("expected Found");
            };
            let mut entry = found.get_instances().expect("instances").get(0);
            entry.set_restart("");
            entry.set_restart_origin("");
        }
        let NodeInfoResponse::Found(decoded) =
            NodeInfoResponse::decode(&encode_message(&builder).expect("encode")).expect("decode")
        else {
            panic!("expected Found");
        };
        assert_eq!(
            decoded.instances[0].restart_status.restart,
            RestartDeclaration::policy_only(RestartPolicy::Never)
        );
        assert_eq!(
            decoded.instances[0].restart_status.restart_origin,
            RestartOrigin::Default
        );
    }
}
