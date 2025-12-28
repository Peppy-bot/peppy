use std::path::PathBuf;
use std::time::Duration;

use bytes::Bytes;
use capnp::message::Builder;
use config::peppy_config::BuildSystem;
use peppylib::{MessengerHandle, ServiceMessenger};

use crate::Result;
use crate::node_capnp;

use super::{decode_message, encode_message};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSyncRequest {
    pub node_root_dir: PathBuf,
    pub build_system: BuildSystem,
}

impl NodeSyncRequest {
    pub fn new(node_root_dir: impl Into<PathBuf>) -> Self {
        Self {
            node_root_dir: node_root_dir.into(),
            build_system: BuildSystem::Cargo,
        }
    }

    pub fn with_build_system(mut self, build_system: BuildSystem) -> Self {
        self.build_system = build_system;
        self
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<node_capnp::node_sync_request::Builder>();
            request.set_node_root_dir(&self.node_root_dir.to_string_lossy());
            request.set_language(&self.build_system.to_string());
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<node_capnp::node_sync_request::Reader>()?;
        let build_system_str = request.get_language()?.to_str()?;
        let build_system = build_system_str.parse()?;
        Ok(Self {
            node_root_dir: PathBuf::from(request.get_node_root_dir()?.to_str()?),
            build_system,
        })
    }

    pub async fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_master_node: &str,
        as_instance_id: &str,
        target_node_name: &str,
        target_instance_id: Option<&str>,
        response_timeout: Duration,
    ) -> Result<NodeSyncResponse> {
        let request_payload = self.encode()?;
        let response = ServiceMessenger::poll(
            messenger,
            bound_master_node,
            as_instance_id,
            target_node_name,
            "node_sync",
            None,
            target_instance_id,
            request_payload,
            response_timeout,
        )
        .await?;
        NodeSyncResponse::decode(&response.payload().to_bytes())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSyncResponse {
    pub success: bool,
    pub error_message: String,
}

impl NodeSyncResponse {
    pub fn new(success: bool, error_message: impl Into<String>) -> Self {
        Self {
            success,
            error_message: error_message.into(),
        }
    }

    pub fn success() -> Self {
        Self::new(true, "")
    }

    pub fn failure(error_message: impl Into<String>) -> Self {
        Self::new(false, error_message)
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<node_capnp::node_sync_response::Builder>();
            response.set_success(self.success);
            response.set_error_message(&self.error_message);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<node_capnp::node_sync_response::Reader>()?;
        Ok(Self {
            success: response.get_success(),
            error_message: response.get_error_message()?.to_str()?.to_owned(),
        })
    }
}
