//! Cap'n Proto encoding utilities for health messages.

use capnp::message::Builder;

use crate::health_capnp;
use crate::{Payload, Result};

use super::{decode_message, encode_message};

/// Request for the core-node `/health` service. Carries no fields: the probe is
/// a liveness check, so a well-formed reply is itself the signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HealthRequest;

impl HealthRequest {
    pub fn new() -> Self {
        Self
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            builder.init_root::<health_capnp::health_request::Builder>();
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        // Validate the framing decodes as a HealthRequest; the struct is empty,
        // so there is nothing else to read back.
        reader.get_root::<health_capnp::health_request::Reader>()?;
        Ok(Self)
    }
}

/// Response from the core-node `/health` service: the daemon's status and how
/// long the core node has been running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthResponse {
    pub status: String,
    pub uptime_secs: u64,
}

impl HealthResponse {
    pub fn new(status: impl Into<String>, uptime_secs: u64) -> Self {
        Self {
            status: status.into(),
            uptime_secs,
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<health_capnp::health_response::Builder>();
            response.set_status(&self.status);
            response.set_uptime_secs(self.uptime_secs);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<health_capnp::health_response::Reader>()?;
        Ok(Self {
            status: response.get_status()?.to_str()?.to_owned(),
            uptime_secs: response.get_uptime_secs(),
        })
    }
}

impl crate::encoding::Wire for HealthRequest {
    type Root = crate::health_capnp::health_request::Owned;
}

impl crate::encoding::Wire for HealthResponse {
    type Root = crate::health_capnp::health_response::Owned;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_request_roundtrip() {
        let original = HealthRequest::new();
        let encoded = original.encode().expect("encode");
        let decoded = HealthRequest::decode(&encoded).expect("decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn health_request_decode_rejects_malformed() {
        assert!(HealthRequest::decode(b"not a capnp message").is_err());
    }

    #[test]
    fn health_response_new_sets_fields() {
        let response = HealthResponse::new("healthy", 42);
        assert_eq!(response.status, "healthy");
        assert_eq!(response.uptime_secs, 42);
    }

    #[test]
    fn health_response_roundtrip() {
        let original = HealthResponse::new("healthy", 1_234_567_890);
        let encoded = original.encode().expect("encode");
        let decoded = HealthResponse::decode(&encoded).expect("decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn health_response_roundtrip_empty_status() {
        let original = HealthResponse::new("", 0);
        let encoded = original.encode().expect("encode");
        let decoded = HealthResponse::decode(&encoded).expect("decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn health_response_roundtrip_max_uptime() {
        let original = HealthResponse::new("healthy", u64::MAX);
        let encoded = original.encode().expect("encode");
        let decoded = HealthResponse::decode(&encoded).expect("decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn health_response_decode_rejects_malformed() {
        assert!(HealthResponse::decode(b"not a capnp message").is_err());
    }
}
