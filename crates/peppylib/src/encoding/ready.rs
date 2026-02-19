//! Cap'n Proto encoding utilities for ready messages.

use crate::types::Payload;
use capnp::message::Builder;

use crate::error::Result;
use crate::health_capnp;

use super::{decode_message, encode_message};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeReadyRequest {}

impl NodeReadyRequest {
    pub fn new() -> Self {
        Self {}
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let _request = builder.init_root::<health_capnp::node_ready_request::Builder>();
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let _request = reader
            .get_root::<health_capnp::node_ready_request::Reader>()
            .map_err(|e| crate::error::Error::Deserialization(e.to_string()))?;
        Ok(Self {})
    }
}

impl Default for NodeReadyRequest {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeReadyResponse {}

impl NodeReadyResponse {
    pub fn new() -> Self {
        Self {}
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let _response = builder.init_root::<health_capnp::node_ready_response::Builder>();
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let _response = reader
            .get_root::<health_capnp::node_ready_response::Reader>()
            .map_err(|e| crate::error::Error::Deserialization(e.to_string()))?;
        Ok(Self {})
    }
}

impl Default for NodeReadyResponse {
    fn default() -> Self {
        Self::new()
    }
}
