//! Cap'n Proto encoding utilities for info messages.

use capnp::message::Builder;

use crate::Result;
use crate::info_capnp;

use super::{decode_message, encode_message};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InfoRequest;

impl InfoRequest {
    pub fn new() -> Self {
        Self
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut builder = Builder::new_default();
        {
            builder.init_root::<info_capnp::info_request::Builder>();
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        reader.get_root::<info_capnp::info_request::Reader>()?;
        Ok(Self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerInfo {
    pub apptainer_version: String,
    pub lima_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfoResponse {
    pub uptime_secs: u64,
    pub core_node_name: String,
    pub core_node_instance_id: String,
    pub host_name: String,
    pub node_count: u32,
    pub git_version: String,
    pub container_info: ContainerInfo,
}

impl InfoResponse {
    pub fn new(
        uptime_secs: u64,
        core_node_name: impl Into<String>,
        core_node_instance_id: impl Into<String>,
        host_name: impl Into<String>,
        node_count: u32,
        git_version: impl Into<String>,
        container_info: ContainerInfo,
    ) -> Self {
        Self {
            uptime_secs,
            core_node_name: core_node_name.into(),
            core_node_instance_id: core_node_instance_id.into(),
            host_name: host_name.into(),
            node_count,
            git_version: git_version.into(),
            container_info,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<info_capnp::info_response::Builder>();
            response.set_uptime_secs(self.uptime_secs);
            response.set_core_node_name(&self.core_node_name);
            response.set_core_node_instance_id(&self.core_node_instance_id);
            response.set_host_name(&self.host_name);
            response.set_node_count(self.node_count);
            response.set_git_version(&self.git_version);
            let mut container = response.init_container_info();
            container.set_apptainer_version(&self.container_info.apptainer_version);
            container.set_lima_version(&self.container_info.lima_version);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<info_capnp::info_response::Reader>()?;
        let container = response.get_container_info()?;
        Ok(Self {
            uptime_secs: response.get_uptime_secs(),
            core_node_name: response.get_core_node_name()?.to_str()?.to_owned(),
            core_node_instance_id: response.get_core_node_instance_id()?.to_str()?.to_owned(),
            host_name: response.get_host_name()?.to_str()?.to_owned(),
            node_count: response.get_node_count(),
            git_version: response.get_git_version()?.to_str()?.to_owned(),
            container_info: ContainerInfo {
                apptainer_version: container.get_apptainer_version()?.to_str()?.to_owned(),
                lima_version: container.get_lima_version()?.to_str()?.to_owned(),
            },
        })
    }
}
