//! Cap'n Proto encoding utilities for ping messages.

use capnp::message::Builder;

use crate::ping_capnp;
use crate::{Payload, Result};

use super::{decode_message, encode_message};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PingRequest {
    pub timestamp: u64,
}

impl PingRequest {
    pub fn new(timestamp: u64) -> Self {
        Self { timestamp }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<ping_capnp::ping_request::Builder>();
            request.set_timestamp(self.timestamp);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<ping_capnp::ping_request::Reader>()?;
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

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<ping_capnp::ping_response::Builder>();
            response.set_timestamp(self.timestamp);
            response.set_message(&self.message);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<ping_capnp::ping_response::Reader>()?;
        Ok(Self {
            timestamp: response.get_timestamp(),
            message: response.get_message()?.to_str()?.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_request_new_sets_timestamp() {
        let request = PingRequest::new(42);
        assert_eq!(request.timestamp, 42);
    }

    #[test]
    fn ping_request_roundtrip() {
        let original = PingRequest::new(1_234_567_890);
        let encoded = original.encode().expect("encode");
        let decoded = PingRequest::decode(&encoded).expect("decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn ping_request_roundtrip_zero_timestamp() {
        let original = PingRequest::new(0);
        let encoded = original.encode().expect("encode");
        let decoded = PingRequest::decode(&encoded).expect("decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn ping_request_roundtrip_max_timestamp() {
        let original = PingRequest::new(u64::MAX);
        let encoded = original.encode().expect("encode");
        let decoded = PingRequest::decode(&encoded).expect("decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn ping_request_decode_rejects_malformed() {
        assert!(PingRequest::decode(b"not a capnp message").is_err());
    }

    #[test]
    fn ping_response_new_sets_fields() {
        let response = PingResponse::new(7, "pong");
        assert_eq!(response.timestamp, 7);
        assert_eq!(response.message, "pong");
    }

    #[test]
    fn ping_response_roundtrip() {
        let original = PingResponse::new(987_654_321, "pong");
        let encoded = original.encode().expect("encode");
        let decoded = PingResponse::decode(&encoded).expect("decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn ping_response_roundtrip_empty_message() {
        let original = PingResponse::new(0, "");
        let encoded = original.encode().expect("encode");
        let decoded = PingResponse::decode(&encoded).expect("decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn ping_response_roundtrip_max_timestamp() {
        let original = PingResponse::new(u64::MAX, "alive");
        let encoded = original.encode().expect("encode");
        let decoded = PingResponse::decode(&encoded).expect("decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn ping_response_decode_rejects_malformed() {
        assert!(PingResponse::decode(b"not a capnp message").is_err());
    }
}
