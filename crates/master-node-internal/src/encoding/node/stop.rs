use std::time::Duration;

use bytes::Bytes;
use capnp::message::Builder;
use peppylib::{MessengerHandle, ServiceMessenger};

use crate::Result;
use crate::names;
use crate::node_capnp;

use super::{decode_message, encode_message};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeStopRequest {
    pub instance_id: String,
}

impl NodeStopRequest {
    pub fn new(instance_id: impl Into<String>) -> Self {
        Self {
            instance_id: instance_id.into(),
        }
    }

    fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<node_capnp::node_stop_request::Builder>();
            request.set_instance_id(&self.instance_id);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<node_capnp::node_stop_request::Reader>()?;
        Ok(Self {
            instance_id: request.get_instance_id()?.to_str()?.to_owned(),
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
    ) -> Result<NodeStopResponse> {
        let request_payload = self.encode()?;
        let response = ServiceMessenger::poll(
            messenger,
            bound_master_node,
            as_instance_id,
            target_node_name,
            names::NODE_STOP,
            None,
            target_instance_id,
            request_payload,
            response_timeout,
        )
        .await?;
        NodeStopResponse::decode(&response.payload().to_bytes())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeStopResponse {
    pub success: bool,
    pub error_message: Option<String>,
}

impl NodeStopResponse {
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
            let mut response = builder.init_root::<node_capnp::node_stop_response::Builder>();
            response.set_success(self.success);
            if let Some(ref error_message) = self.error_message {
                response.set_error_message(error_message);
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<node_capnp::node_stop_response::Reader>()?;
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
