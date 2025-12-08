//! Cap'n Proto encoding utilities for ping messages.

use bytes::Bytes;
use capnp::message::Builder;

use crate::Result;
use crate::messages_capnp;

use super::{decode_message, encode_message};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PingRequest {
    pub timestamp: u64,
}

impl PingRequest {
    pub fn new(timestamp: u64) -> Self {
        Self { timestamp }
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<messages_capnp::ping_request::Builder>();
            request.set_timestamp(self.timestamp);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<messages_capnp::ping_request::Reader>()?;
        Ok(Self {
            timestamp: request.get_timestamp(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PingResponse {
    pub timestamp: u64,
    pub message: String,
}

impl PingResponse {
    pub fn new(timestamp: u64, message: impl Into<String>) -> Self {
        Self {
            timestamp,
            message: message.into(),
        }
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<messages_capnp::ping_response::Builder>();
            response.set_timestamp(self.timestamp);
            response.set_message(&self.message);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<messages_capnp::ping_response::Reader>()?;
        Ok(Self {
            timestamp: response.get_timestamp(),
            message: response.get_message()?.to_str()?.to_owned(),
        })
    }
}
