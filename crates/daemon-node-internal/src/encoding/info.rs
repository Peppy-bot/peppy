//! Cap'n Proto encoding utilities for info messages.

use std::time::Duration;

use bytes::Bytes;
use capnp::message::Builder;
use peppylib::{MessengerHandle, ServiceMessenger};

use crate::Result;
use crate::info_capnp;
use crate::names;

use super::{decode_message, encode_message};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InfoRequest;

impl InfoRequest {
    pub fn new() -> Self {
        Self
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
        Ok(Self)
    }

    pub async fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_daemon_node: &str,
        as_instance_id: &str,
        target_daemon_node: &str,
        response_timeout: Duration,
    ) -> Result<InfoResponse> {
        let request_payload = self.encode()?;
        let response = ServiceMessenger::poll(
            messenger,
            bound_daemon_node,
            as_instance_id,
            target_daemon_node,
            names::INFO,
            Some(target_daemon_node),
            None,
            request_payload,
            response_timeout,
        )
        .await?;
        InfoResponse::decode(&response.payload().to_bytes())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfoResponse {
    pub uptime_secs: u64,
    pub daemon_node_name: String,
    pub daemon_node_instance_id: String,
    pub host_name: String,
    pub node_count: u32,
    pub git_version: String,
}

impl InfoResponse {
    pub fn new(
        uptime_secs: u64,
        daemon_node_name: impl Into<String>,
        daemon_node_instance_id: impl Into<String>,
        host_name: impl Into<String>,
        node_count: u32,
        git_version: impl Into<String>,
    ) -> Self {
        Self {
            uptime_secs,
            daemon_node_name: daemon_node_name.into(),
            daemon_node_instance_id: daemon_node_instance_id.into(),
            host_name: host_name.into(),
            node_count,
            git_version: git_version.into(),
        }
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<info_capnp::info_response::Builder>();
            response.set_uptime_secs(self.uptime_secs);
            response.set_daemon_node_name(&self.daemon_node_name);
            response.set_daemon_node_instance_id(&self.daemon_node_instance_id);
            response.set_host_name(&self.host_name);
            response.set_node_count(self.node_count);
            response.set_git_version(&self.git_version);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<info_capnp::info_response::Reader>()?;
        Ok(Self {
            uptime_secs: response.get_uptime_secs(),
            daemon_node_name: response.get_daemon_node_name()?.to_str()?.to_owned(),
            daemon_node_instance_id: response.get_daemon_node_instance_id()?.to_str()?.to_owned(),
            host_name: response.get_host_name()?.to_str()?.to_owned(),
            node_count: response.get_node_count(),
            git_version: response.get_git_version()?.to_str()?.to_owned(),
        })
    }
}
