//! Cap'n Proto encoding utilities for node info messages.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::str::FromStr;

use capnp::message::Builder;
use config::node::NodeConfig;
use config::runtime::SlotBinding;

use crate::graph::{InstanceState, NodeStage};
use crate::node_capnp;
use crate::{Payload, Result};

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
    /// Pre-resolved per-slot bindings for this consumer instance,
    /// mirroring [`config::runtime::NodeInstanceConfig::slot_bindings`].
    /// Empty when the node has no `depends_on` slots. Surfacing this
    /// lets the launcher / CLI cross-check newly-staged binding plans
    /// against what running consumers have already claimed.
    pub slot_bindings: BTreeMap<String, SlotBinding>,
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
                    let slot_bindings: BTreeMap<String, SlotBinding> =
                        if slot_bindings_json.is_empty() {
                            BTreeMap::new()
                        } else {
                            serde_json5::from_str(slot_bindings_json).map_err(|e| {
                                crate::Error::Decoding(format!(
                                    "failed to deserialize slot_bindings: {}",
                                    e
                                ))
                            })?
                        };
                    instances.push(NodeInstanceInfo {
                        instance_id: entry.get_instance_id()?.to_str()?.to_owned(),
                        state,
                        healthy: entry.get_healthy(),
                        slot_bindings,
                    });
                }
                let add_log_path =
                    optional_text(found.get_add_log_path()?.to_str()?).map(PathBuf::from);
                let run_log_paths_reader = found.get_run_log_paths()?;
                let mut run_log_paths = Vec::with_capacity(run_log_paths_reader.len() as usize);
                for i in 0..run_log_paths_reader.len() {
                    run_log_paths.push(PathBuf::from(run_log_paths_reader.get(i)?.to_str()?));
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

#[cfg(test)]
mod tests {
    use super::*;
    use config::node::NodeConfigParser;

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
            peppy_schema: "node_v1",
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
        use config::runtime::ProducerRef;
        let bindings_a: BTreeMap<String, SlotBinding> = [
            (
                "wrist_left_camera".to_string(),
                SlotBinding::Pinned {
                    producer: ProducerRef::new("core_a", "cam1"),
                },
            ),
            (
                "extra_cam".to_string(),
                SlotBinding::FromAnyBound {
                    producers: vec![
                        ProducerRef::new("core_a", "cam2"),
                        ProducerRef::new("core_a", "cam3"),
                    ],
                },
            ),
            ("spare".to_string(), SlotBinding::FromAnyUnbound),
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
                },
                NodeInstanceInfo {
                    instance_id: "inst-no-bindings".to_string(),
                    state: InstanceState::Starting,
                    healthy: false,
                    slot_bindings: BTreeMap::new(),
                },
            ],
            add_log_path: None,
            run_log_paths: vec![],
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
                assert!(
                    info.instances[1].slot_bindings.is_empty(),
                    "empty slot_bindings should round-trip as empty"
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
