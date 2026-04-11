//! Cap'n Proto encoding utilities for node info messages.

use std::path::PathBuf;
use std::time::Duration;

use capnp::message::Builder;
use config::node::NodeConfig;
use peppylib::types::Payload;
use peppylib::{MessengerHandle, ServiceMessenger};

use crate::Result;
use crate::names;
use crate::node_capnp;

use crate::encoding::{decode_message, encode_message, optional_text};

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

    pub async fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        target_core_node: &str,
        response_timeout: Duration,
    ) -> Result<NodeInfoResponse> {
        let request_payload = self.encode()?;
        let response = ServiceMessenger::poll(
            messenger,
            bound_core_node,
            as_instance_id,
            target_core_node,
            names::NODE_INFO,
            Some(target_core_node),
            None,
            request_payload,
            response_timeout,
        )
        .await?;
        NodeInfoResponse::decode(response.payload().as_ref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeInstanceInfo {
    pub instance_id: String,
    /// Per-instance lifecycle state, as a lowercase string ("starting" or "running").
    pub state: String,
}

/// Body of a successful `node_info` lookup — carries all metadata about a
/// node that was found in the stack.
#[derive(Debug, Clone)]
pub struct NodeInfo {
    /// Resolved NodeConfig as stored in the node stack.
    pub config: NodeConfig,
    /// SHA256 of the serialized NodeConfig at the time of the response.
    pub config_integrity: String,
    /// Lifecycle stage of the entity ("Added"/"Building"/"Ready"/"Root").
    pub stage: String,
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
    Found(NodeInfo),
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
                    found.set_stage(&info.stage);
                    {
                        let mut instances_builder =
                            found.reborrow().init_instances(info.instances.len() as u32);
                        for (i, inst) in info.instances.iter().enumerate() {
                            let mut entry = instances_builder.reborrow().get(i as u32);
                            entry.set_instance_id(&inst.instance_id);
                            entry.set_state(&inst.state);
                        }
                    }
                    found.set_add_log_path(
                        info.add_log_path
                            .as_ref()
                            .map(|p| p.to_string_lossy().into_owned())
                            .unwrap_or_default()
                            .as_str(),
                    );
                    {
                        let mut paths_builder = found
                            .reborrow()
                            .init_run_log_paths(info.run_log_paths.len() as u32);
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
                let stage = found.get_stage()?.to_str()?.to_owned();
                let instances_reader = found.get_instances()?;
                let mut instances = Vec::with_capacity(instances_reader.len() as usize);
                for i in 0..instances_reader.len() {
                    let entry = instances_reader.get(i);
                    instances.push(NodeInstanceInfo {
                        instance_id: entry.get_instance_id()?.to_str()?.to_owned(),
                        state: entry.get_state()?.to_str()?.to_owned(),
                    });
                }
                let add_log_path =
                    optional_text(found.get_add_log_path()?.to_str()?).map(PathBuf::from);
                let run_log_paths_reader = found.get_run_log_paths()?;
                let mut run_log_paths = Vec::with_capacity(run_log_paths_reader.len() as usize);
                for i in 0..run_log_paths_reader.len() {
                    run_log_paths.push(PathBuf::from(run_log_paths_reader.get(i)?.to_str()?));
                }
                Ok(NodeInfoResponse::Found(NodeInfo {
                    config,
                    config_integrity,
                    stage,
                    instances,
                    add_log_path,
                    run_log_paths,
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_info_request_roundtrips_name_tag() {
        let encoded = NodeInfoRequest::new("sensor_node", "0.1.0")
            .encode()
            .expect("encoding should succeed");
        let decoded = NodeInfoRequest::decode(&encoded).expect("decoding should succeed");

        assert_eq!(decoded.node_name, "sensor_node");
        assert_eq!(decoded.node_tag, "0.1.0");
    }
}
