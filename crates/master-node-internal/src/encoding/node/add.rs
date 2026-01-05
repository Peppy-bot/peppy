use std::path::PathBuf;
use std::time::Duration;

use bytes::Bytes;
use capnp::message::Builder;
use peppylib::{MessengerHandle, ServiceMessenger};

use crate::Result;
use crate::names;
use crate::node_capnp;

use super::{decode_message, encode_message};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeAddRequest {
    pub peppy_json5: String,
    pub from_dir: PathBuf,
    pub instance_id: Option<String>,
}

impl NodeAddRequest {
    pub fn new(peppy_json5: impl Into<String>, from_dir: impl Into<PathBuf>) -> Self {
        Self {
            peppy_json5: peppy_json5.into(),
            from_dir: from_dir.into(),
            instance_id: None,
        }
    }

    pub fn with_instance_id(mut self, instance_id: impl Into<String>) -> Self {
        self.instance_id = Some(instance_id.into());
        self
    }

    fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<node_capnp::node_add_request::Builder>();
            request.set_peppy_json5(&self.peppy_json5);
            request.set_from_dir(self.from_dir.to_string_lossy());
            if let Some(ref instance_id) = self.instance_id {
                request.set_instance_id(instance_id);
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<node_capnp::node_add_request::Reader>()?;
        let instance_id_str = request.get_instance_id()?.to_str()?;
        let instance_id = if instance_id_str.is_empty() {
            None
        } else {
            Some(instance_id_str.to_owned())
        };
        Ok(Self {
            peppy_json5: request.get_peppy_json5()?.to_str()?.to_owned(),
            from_dir: PathBuf::from(request.get_from_dir()?.to_str()?),
            instance_id,
        })
    }

    pub async fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_master_node: &str,
        as_instance_id: &str,
        target_master_node: &str,
        response_timeout: Duration,
    ) -> Result<NodeAddResponse> {
        let request_payload = self.encode()?;
        let response = ServiceMessenger::poll(
            messenger,
            bound_master_node,
            as_instance_id,
            target_master_node,
            names::NODE_ADD,
            Some(target_master_node),
            None,
            request_payload,
            response_timeout,
        )
        .await?;
        NodeAddResponse::decode(&response.payload().to_bytes())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeAddResponse {
    pub success: bool,
    pub error_message: Option<String>,
}

impl NodeAddResponse {
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
            let mut response = builder.init_root::<node_capnp::node_add_response::Builder>();
            response.set_success(self.success);
            if let Some(ref error_message) = self.error_message {
                response.set_error_message(error_message);
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<node_capnp::node_add_response::Reader>()?;
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
