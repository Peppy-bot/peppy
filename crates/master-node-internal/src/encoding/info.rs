//! Cap'n Proto encoding utilities for info messages.

use bytes::Bytes;
use capnp::message::Builder;

use crate::Result;
use crate::info_capnp;

use super::{decode_message, encode_message};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfoRequest {}

impl InfoRequest {
    pub fn new() -> Self {
        Self {}
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            builder.init_root::<info_capnp::info_request::Builder>();
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        reader.get_root::<info_capnp::info_request::Reader>()?;
        Ok(Self {})
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfoResponse {
    pub uptime_secs: u64,
    pub master_node_name: String,
    pub master_node_instance_id: String,
    pub host_name: String,
    pub node_count: u32,
}

impl InfoResponse {
    pub fn new(
        uptime_secs: u64,
        master_node_name: impl Into<String>,
        master_node_instance_id: impl Into<String>,
        host_name: impl Into<String>,
        node_count: u32,
    ) -> Self {
        Self {
            uptime_secs,
            master_node_name: master_node_name.into(),
            master_node_instance_id: master_node_instance_id.into(),
            host_name: host_name.into(),
            node_count,
        }
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<info_capnp::info_response::Builder>();
            response.set_uptime_secs(self.uptime_secs);
            response.set_master_node_name(&self.master_node_name);
            response.set_master_node_instance_id(&self.master_node_instance_id);
            response.set_host_name(&self.host_name);
            response.set_node_count(self.node_count);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<info_capnp::info_response::Reader>()?;
        Ok(Self {
            uptime_secs: response.get_uptime_secs(),
            master_node_name: response.get_master_node_name()?.to_str()?.to_owned(),
            master_node_instance_id: response.get_master_node_instance_id()?.to_str()?.to_owned(),
            host_name: response.get_host_name()?.to_str()?.to_owned(),
            node_count: response.get_node_count(),
        })
    }
}
