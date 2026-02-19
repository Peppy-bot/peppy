//! Cap'n Proto encoding utilities for health messages.

use crate::types::Payload;
use capnp::message::Builder;

use crate::error::Result;
use crate::health_capnp;

use super::{decode_message, encode_message};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeHealthRequest {}

impl NodeHealthRequest {
    pub fn new() -> Self {
        Self {}
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let _request = builder.init_root::<health_capnp::node_health_request::Builder>();
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let _request = reader
            .get_root::<health_capnp::node_health_request::Reader>()
            .map_err(|e| crate::error::Error::Deserialization(e.to_string()))?;
        Ok(Self {})
    }
}

impl Default for NodeHealthRequest {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeHealthResponse {}

impl NodeHealthResponse {
    pub fn new() -> Self {
        Self {}
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let _response = builder.init_root::<health_capnp::node_health_response::Builder>();
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let _response = reader
            .get_root::<health_capnp::node_health_response::Reader>()
            .map_err(|e| crate::error::Error::Deserialization(e.to_string()))?;
        Ok(Self {})
    }
}

impl Default for NodeHealthResponse {
    fn default() -> Self {
        Self::new()
    }
}
