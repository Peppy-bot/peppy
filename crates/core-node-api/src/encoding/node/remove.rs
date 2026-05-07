use capnp::message::Builder;

use crate::node_capnp;
use crate::{Payload, Result};

use crate::encoding::{decode_message, encode_message, optional_text};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRemoveRequest {
    pub node_name: String,
    pub tag: String,
    pub node_variant: String,
    pub stop_instances: bool,
}

impl NodeRemoveRequest {
    pub fn new(
        node_name: impl Into<String>,
        tag: impl Into<String>,
        node_variant: impl Into<String>,
    ) -> Self {
        Self {
            node_name: node_name.into(),
            tag: tag.into(),
            node_variant: node_variant.into(),
            stop_instances: false,
        }
    }

    pub fn with_stop_instances(mut self, stop_instances: bool) -> Self {
        self.stop_instances = stop_instances;
        self
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<node_capnp::node_remove_request::Builder>();
            request.set_node_name(&self.node_name);
            request.set_stop_instances(self.stop_instances);
            request.set_tag(&self.tag);
            request.set_node_variant(&self.node_variant);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<node_capnp::node_remove_request::Reader>()?;
        Ok(Self {
            node_name: request.get_node_name()?.to_str()?.to_owned(),
            tag: request.get_tag()?.to_str()?.to_owned(),
            node_variant: request.get_node_variant()?.to_str()?.to_owned(),
            stop_instances: request.get_stop_instances(),
        })
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

    pub fn encode(&self) -> Result<Payload> {
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
        Ok(Self {
            success: response.get_success(),
            error_message: optional_text(response.get_error_message()?.to_str()?),
        })
    }
}
