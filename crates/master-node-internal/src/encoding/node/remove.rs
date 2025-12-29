use std::time::Duration;

use bytes::Bytes;
use capnp::message::Builder;
use peppylib::{MessengerHandle, ServiceMessenger};

use crate::Result;
use crate::names;
use crate::node_capnp;

use super::{decode_message, encode_message};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRemoveRequest {
    pub node_name: String,
    pub stop_instances: bool,
}

impl NodeRemoveRequest {
    pub fn new(node_name: impl Into<String>) -> Self {
        Self {
            node_name: node_name.into(),
            stop_instances: false,
        }
    }

    pub fn with_stop_instances(mut self, stop_instances: bool) -> Self {
        self.stop_instances = stop_instances;
        self
    }

    fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<node_capnp::node_remove_request::Builder>();
            request.set_node_name(&self.node_name);
            request.set_stop_instances(self.stop_instances);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<node_capnp::node_remove_request::Reader>()?;
        Ok(Self {
            node_name: request.get_node_name()?.to_str()?.to_owned(),
            stop_instances: request.get_stop_instances(),
        })
    }

    pub async fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_master_node: &str,
        as_instance_id: &str,
        target_master_node: &str,
        response_timeout: Duration,
    ) -> Result<NodeRemoveResponse> {
        let request_payload = self.encode()?;
        let response = ServiceMessenger::poll(
            messenger,
            bound_master_node,
            as_instance_id,
            target_master_node,
            names::NODE_REMOVE,
            Some(target_master_node),
            None,
            request_payload,
            response_timeout,
        )
        .await?;
        NodeRemoveResponse::decode(&response.payload().to_bytes())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRemoveResponse {
    pub success: bool,
    pub error_message: Option<String>,
}

impl NodeRemoveResponse {
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
            let mut response = builder.init_root::<node_capnp::node_remove_response::Builder>();
            response.set_success(self.success);
            if let Some(ref error_message) = self.error_message {
                response.set_error_message(error_message);
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<node_capnp::node_remove_response::Reader>()?;
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
