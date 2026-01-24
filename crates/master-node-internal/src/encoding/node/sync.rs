use std::path::PathBuf;
use std::time::Duration;

use bytes::Bytes;
use capnp::message::Builder;
use config::peppy_config::Toolchain;
use peppylib::{MessengerHandle, ServiceMessenger};

use crate::Result;
use crate::names;
use crate::node_capnp;

use super::{decode_message, encode_message};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSyncRequest {
    pub node_root_dir: PathBuf,
    pub build_system: Toolchain,
    pub git_hash: String,
}

impl NodeSyncRequest {
    pub fn new(node_root_dir: impl Into<PathBuf>, git_hash: impl Into<String>) -> Self {
        Self {
            node_root_dir: node_root_dir.into(),
            build_system: Toolchain::Cargo,
            git_hash: git_hash.into(),
        }
    }

    pub fn with_build_system(mut self, build_system: Toolchain) -> Self {
        self.build_system = build_system;
        self
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<node_capnp::node_generate_request::Builder>();
            request.set_node_root_dir(self.node_root_dir.to_string_lossy());
            request.set_language(self.build_system.to_string());
            request.set_git_hash(&self.git_hash);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<node_capnp::node_generate_request::Reader>()?;
        let build_system_str = request.get_language()?.to_str()?;
        let build_system = build_system_str.parse()?;
        let git_hash = request.get_git_hash()?.to_str()?.to_owned();
        Ok(Self {
            node_root_dir: PathBuf::from(request.get_node_root_dir()?.to_str()?),
            build_system,
            git_hash,
        })
    }

    pub async fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_master_node: &str,
        as_instance_id: &str,
        target_master_node: &str,
        response_timeout: Duration,
    ) -> Result<NodeSyncResponse> {
        let request_payload = self.encode()?;
        let response = ServiceMessenger::poll(
            messenger,
            bound_master_node,
            as_instance_id,
            target_master_node,
            names::NODE_SYNC,
            Some(target_master_node),
            None,
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
