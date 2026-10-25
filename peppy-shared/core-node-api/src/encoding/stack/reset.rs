use capnp::message::Builder;

use crate::node_capnp;
use crate::{Payload, Result};

use crate::encoding::{decode_message, encode_message};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StackResetRequest;

impl StackResetRequest {
    pub fn new() -> Self {
        Self
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        builder.init_root::<node_capnp::stack_reset_request::Builder>();
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        reader.get_root::<node_capnp::stack_reset_request::Reader>()?;
        Ok(Self)
    }
}

/// The response of a stack reset that succeeded. Carries no fields: a
/// failed reset is an error reply, with a failure kind and a message.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StackResetResponse;

impl StackResetResponse {
    pub fn new() -> Self {
        Self
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        builder.init_root::<node_capnp::stack_reset_response::Builder>();
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        reader.get_root::<node_capnp::stack_reset_response::Reader>()?;
        Ok(Self)
    }
}

impl crate::encoding::Wire for StackResetRequest {
    type Root = crate::node_capnp::stack_reset_request::Owned;
}

impl crate::encoding::Wire for StackResetResponse {
    type Root = crate::node_capnp::stack_reset_response::Owned;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips() {
        let request = StackResetRequest::new();
        let payload = request.encode().expect("encode");
        let decoded = StackResetRequest::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, request);
    }

    // Round-trips a Default-constructed request; `::default()` on this unit
    // struct is intentional, hence the lint allow.
    #[allow(clippy::default_constructed_unit_structs)]
    #[test]
    fn request_default_round_trips() {
        let request = StackResetRequest::default();
        let payload = request.encode().expect("encode");
        StackResetRequest::decode(payload.as_ref()).expect("decode");
    }

    #[test]
    fn request_decode_rejects_malformed() {
        assert!(StackResetRequest::decode(b"not capnp").is_err());
    }

    #[test]
    fn response_round_trips() {
        let payload = StackResetResponse::new().encode().expect("encode");
        let decoded = StackResetResponse::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, StackResetResponse);
    }

    #[test]
    fn response_decode_rejects_malformed() {
        assert!(StackResetResponse::decode(b"not capnp").is_err());
    }
}
