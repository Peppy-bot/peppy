//! Cap'n Proto encoding utilities for node info messages.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::str::FromStr;

use capnp::message::Builder;
use config::node::NodeConfig;
use config::runtime::{ClockBinding, SlotBindings};

use crate::graph::{InstanceEndpoint, InstanceState, NodeStage, SerializedPairingSlot};
use crate::node_capnp;
use crate::{Payload, Result};

use crate::encoding::endpoints::{read_node_endpoints, write_node_endpoints};
use crate::encoding::{capnp_list_len, decode_message, encode_message, optional_text};

/// Request payload for the `node_info` service.
///
/// Identifies a node already present in the node stack by `(name, tag)`.
/// Unlike `node_add`, `node_info` does not resolve configs from filesystem,
/// git, or HTTP sources — it only inspects what is already in the stack.
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
    /// node declares no pairings. Lets the CLI's `--pair` preflight see
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
}

/// Body of a successful `node_info` lookup — carries all metadata about a
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
}

/// Response payload for the `node_info` service.
///
/// `NotInStack` is a first-class *successful* negative answer to the lookup,
/// not a protocol-level error. Prior to this shape, the daemon rejected
/// missing-node lookups with `InvalidServiceRequest`, which conflated "no
/// such node" with "malformed request" and produced spurious ERROR logs
/// during normal flows like the preflight inside `peppy node add`.
#[derive(Debug, Clone)]
pub enum NodeInfoResponse {
    /// The `(name, tag)` pair is not currently in the node stack.
    NotInStack,
    /// The node is in the stack — carries its full metadata.
    Found(Box<NodeInfo>),
}

impl NodeInfoResponse {
    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let response = builder.init_root::<node_capnp::node_info_response::Builder>();
            match self {
                NodeInfoResponse::NotInStack => {
                    // Select the `notInStack :Void` arm of the union.
                    let mut response = response;
                    response.set_not_in_stack(());
                }
                NodeInfoResponse::Found(info) => {
                    // `run_log_paths` is documented to be order-aligned with
                    // `instances`. Reject a mismatch here rather than emit a
                    // payload that would silently misassociate logs on decode.
                    if info.run_log_paths.len() != info.instances.len() {
                        return Err(crate::Error::Encoding(format!(
                            "run_log_paths length ({}) does not match instances length ({})",
                            info.run_log_paths.len(),
                            info.instances.len()
                        )));
                    }
                    let mut found = response.init_found();
                    let config_json5 = serde_json5::to_string(&info.config).map_err(|e| {
                        crate::Error::Encoding(format!("failed to serialize config: {}", e))
                    })?;
                    found.set_config_json5(&config_json5);
                    found.set_config_sha256(&info.config_integrity);
                    found.set_stage(info.stage.as_str());
                    {
                        let instance_count =
                            capnp_list_len(info.instances.len(), "NodeInfo.instances")?;
                        let mut instances_builder = found.reborrow().init_instances(instance_count);
                        for (i, inst) in info.instances.iter().enumerate() {
                            let mut entry = instances_builder.reborrow().get(i as u32);
                            entry.set_instance_id(&inst.instance_id);
                            entry.set_state(inst.state.as_str());
                            entry.set_healthy(inst.healthy);
                            let slot_bindings_json = if inst.slot_bindings.is_empty() {
                                String::new()
                            } else {
                                serde_json5::to_string(&inst.slot_bindings).map_err(|e| {
                                    crate::Error::Encoding(format!(
                                        "failed to serialize slot_bindings for instance `{}`: {}",
                                        inst.instance_id, e
                                    ))
                                })?
                            };
                            entry.set_slot_bindings_json(&slot_bindings_json);
                            let pairing_slots_json = if inst.pairing_slots.is_empty() {
                                String::new()
                            } else {
                                serde_json5::to_string(&inst.pairing_slots).map_err(|e| {
                                    crate::Error::Encoding(format!(
                                        "failed to serialize pairing_slots for instance `{}`: {}",
                                        inst.instance_id, e
                                    ))
                                })?
                            };
                            entry.set_pairing_slots_json(&pairing_slots_json);
                            let clock_json = if inst.clock.is_wall() {
                                String::new()
                            } else {
                                serde_json5::to_string(&inst.clock).map_err(|e| {
                                    crate::Error::Encoding(format!(
                                        "failed to serialize clock for instance `{}`: {}",
                                        inst.instance_id, e
                                    ))
                                })?
                            };
                            entry.set_clock_json(&clock_json);
                            let endpoint_count =
                                capnp_list_len(inst.endpoints.len(), "NodeInstanceInfo.endpoints")?;
                            write_node_endpoints(
                                entry.init_endpoints(endpoint_count),
                                &inst.endpoints,
                            )?;
                        }
                    }
                    // Only set the field when present; leaving it unset writes
                    // capnp's empty-text default, which `optional_text` decodes
                    // back to `None`. Borrows via `to_string_lossy` (no owning
                    // allocation) like the `run_log_paths` loop below.
                    if let Some(path) = &info.add_log_path {
                        found.set_add_log_path(path.to_string_lossy().as_ref());
                    }
                    {
                        let run_log_path_count =
                            capnp_list_len(info.run_log_paths.len(), "NodeInfo.run_log_paths")?;
                        let mut paths_builder =
                            found.reborrow().init_run_log_paths(run_log_path_count);
                        for (i, path) in info.run_log_paths.iter().enumerate() {
                            paths_builder.set(i as u32, path.to_string_lossy().as_ref());
                        }
                    }
                }
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
                let config_json5 = found.get_config_json5()?.to_str()?;
                let config: NodeConfig = serde_json5::from_str(config_json5).map_err(|e| {
                    crate::Error::Decoding(format!("failed to deserialize config: {}", e))
                })?;
                let config_integrity = found.get_config_sha256()?.to_str()?.to_owned();
                let stage_str = found.get_stage()?.to_str()?;
                let stage = NodeStage::from_str(stage_str)
                    .map_err(|e| crate::Error::Decoding(e.to_string()))?;
                let instances_reader = found.get_instances()?;
                let mut instances = Vec::with_capacity(instances_reader.len() as usize);
                for i in 0..instances_reader.len() {
                    let entry = instances_reader.get(i);
                    let state_str = entry.get_state()?.to_str()?;
                    let state = InstanceState::from_str(state_str)
                        .map_err(|e| crate::Error::Decoding(e.to_string()))?;
                    let slot_bindings_json = entry.get_slot_bindings_json()?.to_str()?;
                    let slot_bindings: SlotBindings = if slot_bindings_json.is_empty() {
                        BTreeMap::new()
                    } else {
                        serde_json5::from_str(slot_bindings_json).map_err(|e| {
                            crate::Error::Decoding(format!(
                                "failed to deserialize slot_bindings: {}",
                                e
                            ))
                        })?
                    };
                    let pairing_slots_json = entry.get_pairing_slots_json()?.to_str()?;
                    let pairing_slots: BTreeMap<String, SerializedPairingSlot> =
                        if pairing_slots_json.is_empty() {
                            BTreeMap::new()
                        } else {
                            serde_json5::from_str(pairing_slots_json).map_err(|e| {
                                crate::Error::Decoding(format!(
                                    "failed to deserialize pairing_slots: {}",
                                    e
                                ))
                            })?
                        };
                    let clock_json = entry.get_clock_json()?.to_str()?;
                    let clock: ClockBinding = if clock_json.is_empty() {
                        ClockBinding::Wall
                    } else {
                        serde_json5::from_str(clock_json).map_err(|e| {
                            crate::Error::Decoding(format!("failed to deserialize clock: {}", e))
                        })?
                    };
                    instances.push(NodeInstanceInfo {
                        instance_id: entry.get_instance_id()?.to_str()?.to_owned(),
                        state,
                        healthy: entry.get_healthy(),
                        slot_bindings,
                        pairing_slots,
                        clock,
                        endpoints: read_node_endpoints(entry.get_endpoints()?)?,
                    });
                }
                let add_log_path =
                    optional_text(found.get_add_log_path()?.to_str()?).map(PathBuf::from);
                let run_log_paths_reader = found.get_run_log_paths()?;
                let mut run_log_paths = Vec::with_capacity(run_log_paths_reader.len() as usize);
                for i in 0..run_log_paths_reader.len() {
                    run_log_paths.push(PathBuf::from(run_log_paths_reader.get(i)?.to_str()?));
                }
                // `run_log_paths` is documented to be order-aligned with
                // `instances`. Reject a mismatched payload instead of handing
                // back logs misassociated with the wrong instances.
                if run_log_paths.len() != instances.len() {
                    return Err(crate::Error::Decoding(format!(
                        "run_log_paths length ({}) does not match instances length ({})",
                        run_log_paths.len(),
                        instances.len()
                    )));
                }
                Ok(NodeInfoResponse::Found(Box::new(NodeInfo {
                    config,
                    config_integrity,
                    stage,
                    instances,
                    add_log_path,
                    run_log_paths,
                })))
            }
        }
    }
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
    use config::node::NodeConfigParser;
    use config::runtime::ProducerRef;

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
        };
        let encoded = NodeInfoResponse::Found(Box::new(info))
            .encode()
            .expect("encoding should succeed");
        let decoded = NodeInfoResponse::decode(&encoded).expect("decoding should succeed");

        match decoded {
            NodeInfoResponse::Found(info) => {
                assert_eq!(info.config.manifest.name.as_str(), "sensor_node");
                assert_eq!(info.stage, NodeStage::Added);
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
            // A multi-cardinality slot's ordered set must survive the trip.
            (
                "wrist_right_camera".to_string(),
                config::runtime::BoundProducers::try_from(vec![
                    ProducerRef::new("core_a", "cam2"),
                    ProducerRef::new("core_a", "cam3"),
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
                },
                NodeInstanceInfo {
                    instance_id: "inst-no-bindings".to_string(),
                    state: InstanceState::Starting,
                    healthy: false,
                    slot_bindings: BTreeMap::new(),
                    pairing_slots: BTreeMap::new(),
                    clock: ClockBinding::Wall,
                    endpoints: Vec::new(),
                },
            ],
            add_log_path: None,
            // Kept order-aligned with `instances` so encode/decode accept it.
            run_log_paths: vec![
                PathBuf::from("/var/log/inst-with-bindings.log"),
                PathBuf::from("/var/log/inst-no-bindings.log"),
            ],
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
}
