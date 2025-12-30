use std::time::Duration;

use bytes::Bytes;
use capnp::message::Builder;
use peppylib::{MessengerHandle, ServiceMessenger};

use crate::Result;
use crate::names;
use crate::node_capnp;

use super::{decode_message, encode_message};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeStartRequest {
    pub runtime_config_json5: String,
    pub node_name: String,
    pub tag: String,
}

impl NodeStartRequest {
    pub fn new(
        runtime_config_json5: impl Into<String>,
        node_name: impl Into<String>,
        tag: impl Into<String>,
    ) -> Self {
        Self {
            runtime_config_json5: runtime_config_json5.into(),
            node_name: node_name.into(),
            tag: tag.into(),
        }
    }

    fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<node_capnp::node_start_request::Builder>();
            request.set_runtime_config_json5(&self.runtime_config_json5);
            request.set_node_name(&self.node_name);
            request.set_tag(&self.tag);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<node_capnp::node_start_request::Reader>()?;
        Ok(Self {
            runtime_config_json5: request.get_runtime_config_json5()?.to_str()?.to_owned(),
            node_name: request.get_node_name()?.to_str()?.to_owned(),
            tag: request.get_tag()?.to_str()?.to_owned(),
        })
    }

    pub async fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_master_node: &str,
        as_instance_id: &str,
        target_master_node: &str,
        response_timeout: Duration,
    ) -> Result<NodeStartResponse> {
        let request_payload = self.encode()?;
        let response = ServiceMessenger::poll(
            messenger,
            bound_master_node,
            as_instance_id,
            target_master_node,
            names::NODE_START,
            Some(target_master_node),
            None,
            request_payload,
            response_timeout,
        )
        .await?;
        NodeStartResponse::decode(&response.payload().to_bytes())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeStartResponse {
    pub success: bool,
    pub error_message: Option<String>,
}

impl NodeStartResponse {
    pub fn new(success: bool, error_message: Option<String>) -> Self {
        Self {
            success,
            error_message,
        }
    }

    pub fn success() -> Self {
        Self::new(true, None)
    }

    pub fn failure(error_message: impl Into<String>) -> Self {
        Self::new(false, Some(error_message.into()))
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<node_capnp::node_start_response::Builder>();
            response.set_success(self.success);
            if let Some(ref error_message) = self.error_message {
                response.set_error_message(error_message);
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<node_capnp::node_start_response::Reader>()?;
        let error_message_str = response.get_error_message()?.to_str()?;
        let error_message = if error_message_str.is_empty() {
            None
        } else {
            Some(error_message_str.to_owned())
        };
        Ok(Self {
            success: response.get_success(),
            error_message,
        })
    }
}
