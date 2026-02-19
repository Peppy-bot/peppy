use capnp::message::Builder;
use config::node::Toolchain;
use peppylib::types::Payload;
use peppylib::{MessengerHandle, ServiceMessenger};
use std::path::PathBuf;
use std::time::Duration;

use crate::Result;
use crate::names;
use crate::node_capnp;

use super::{decode_message, encode_message};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeInitRequest {
    pub node_root_dir: PathBuf,
    pub node_name: String,
    pub git_hash: String,
    pub toolchain: Toolchain,
}

impl NodeInitRequest {
    pub fn new(
        node_root_dir: impl Into<PathBuf>,
        node_name: impl Into<String>,
        git_hash: impl Into<String>,
        toolchain: Toolchain,
    ) -> Self {
        Self {
            node_root_dir: node_root_dir.into(),
            node_name: node_name.into(),
            git_hash: git_hash.into(),
            toolchain,
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<node_capnp::node_init_request::Builder>();
            request.set_node_root_dir(self.node_root_dir.to_string_lossy());
            request.set_node_name(&self.node_name);
            request.set_git_hash(&self.git_hash);
            request.set_toolchain(self.toolchain.to_string());
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<node_capnp::node_init_request::Reader>()?;
        let toolchain_str = request.get_toolchain()?.to_str()?;
        let toolchain = toolchain_str.parse()?;
        Ok(Self {
            node_root_dir: PathBuf::from(request.get_node_root_dir()?.to_str()?),
            node_name: request.get_node_name()?.to_str()?.to_owned(),
            git_hash: request.get_git_hash()?.to_str()?.to_owned(),
            toolchain,
        })
    }

    pub async fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_daemon_node: &str,
        as_instance_id: &str,
        target_daemon_node: &str,
        response_timeout: impl Into<Option<Duration>>,
    ) -> Result<NodeInitResponse> {
        let request_payload = self.encode()?;
        let response = ServiceMessenger::poll(
            messenger,
            bound_daemon_node,
            as_instance_id,
            target_daemon_node,
            names::NODE_INIT,
            Some(target_daemon_node),
            None,
            request_payload,
            response_timeout,
        )
        .await?;
        NodeInitResponse::decode(response.payload().as_ref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeInitResponse {
    pub success: bool,
    pub error_message: String,
}

impl NodeInitResponse {
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

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<node_capnp::node_init_response::Builder>();
            response.set_success(self.success);
            response.set_error_message(&self.error_message);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<node_capnp::node_init_response::Reader>()?;
        Ok(Self {
            success: response.get_success(),
            error_message: response.get_error_message()?.to_str()?.to_owned(),
        })
    }
}
