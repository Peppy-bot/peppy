use capnp::message::Builder;

use crate::node_capnp;
use crate::{Payload, Result};

use crate::encoding::{decode_message, encode_message, optional_text};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRemoveRequest {
    pub node_name: String,
    pub tag: String,
    pub stop_instances: bool,
    /// Variant label of the node to remove. `None` is wire-encoded as the
    /// empty string, which the daemon resolves per the bare-form rule:
    /// matches when exactly one variant of `(name, tag)` exists, otherwise
    /// errors with the available variants.
    pub variant: Option<String>,
}

impl NodeRemoveRequest {
    pub fn new(node_name: impl Into<String>, tag: impl Into<String>) -> Self {
        Self {
            node_name: node_name.into(),
            tag: tag.into(),
            stop_instances: false,
            variant: None,
        }
    }

    pub fn with_stop_instances(mut self, stop_instances: bool) -> Self {
        self.stop_instances = stop_instances;
        self
    }

    pub fn with_variant(mut self, variant: impl Into<String>) -> Self {
        self.variant = Some(variant.into());
        self
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<node_capnp::node_remove_request::Builder>();
            request.set_node_name(&self.node_name);
            request.set_stop_instances(self.stop_instances);
            request.set_tag(&self.tag);
            request.set_variant(self.variant.as_deref().unwrap_or(""));
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<node_capnp::node_remove_request::Reader>()?;
        Ok(Self {
            node_name: request.get_node_name()?.to_str()?.to_owned(),
            tag: request.get_tag()?.to_str()?.to_owned(),
            stop_instances: request.get_stop_instances(),
            variant: optional_text(request.get_variant()?.to_str()?),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_remove_request_round_trips_with_explicit_variant() {
        let encoded = NodeRemoveRequest::new("sensor", "0.1.0")
            .with_variant("realsense_d405")
            .with_stop_instances(true)
            .encode()
            .expect("encoding should succeed");
        let decoded = NodeRemoveRequest::decode(&encoded).expect("decoding should succeed");
        assert_eq!(decoded.node_name, "sensor");
        assert_eq!(decoded.tag, "0.1.0");
        assert!(decoded.stop_instances);
        assert_eq!(decoded.variant.as_deref(), Some("realsense_d405"));
    }

    #[test]
    fn node_remove_request_bare_form_decodes_with_no_variant() {
        let encoded = NodeRemoveRequest::new("sensor", "0.1.0")
            .encode()
            .expect("encoding should succeed");
        let decoded = NodeRemoveRequest::decode(&encoded).expect("decoding should succeed");
        assert!(decoded.variant.is_none());
        assert!(!decoded.stop_instances);
    }
}
