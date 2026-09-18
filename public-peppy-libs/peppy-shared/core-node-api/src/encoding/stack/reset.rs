use capnp::message::Builder;

use crate::node_capnp;
use crate::{Payload, Result};

use crate::encoding::{decode_message, encode_message, optional_text};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackResetResponse {
    pub success: bool,
    pub error_message: Option<String>,
}

impl StackResetResponse {
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
            let mut response = builder.init_root::<node_capnp::stack_reset_response::Builder>();
            response.set_success(self.success);
            if let Some(ref error_message) = self.error_message {
                response.set_error_message(error_message);
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<node_capnp::stack_reset_response::Reader>()?;
        Ok(Self {
            success: response.get_success(),
            error_message: optional_text(response.get_error_message()?.to_str()?),
        })
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
    fn response_new_round_trips_success() {
        let response = StackResetResponse::new(true, None);
        let payload = response.encode().expect("encode");
        let decoded = StackResetResponse::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, response);
        assert!(decoded.success);
        assert_eq!(decoded.error_message, None);
    }

    #[test]
    fn response_new_round_trips_with_error_message() {
        let response = StackResetResponse::new(false, Some("reset failed".to_string()));
        let payload = response.encode().expect("encode");
        let decoded = StackResetResponse::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, response);
        assert!(!decoded.success);
        assert_eq!(decoded.error_message.as_deref(), Some("reset failed"));
    }

    #[test]
    fn response_success_constructor_round_trips() {
        let response = StackResetResponse::success();
        assert!(response.success);
        assert_eq!(response.error_message, None);
        let payload = response.encode().expect("encode");
        let decoded = StackResetResponse::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, response);
    }

    #[test]
    fn response_failure_constructor_round_trips() {
        let response = StackResetResponse::failure("daemon unreachable");
        assert!(!response.success);
        assert_eq!(
            response.error_message.as_deref(),
            Some("daemon unreachable")
        );
        let payload = response.encode().expect("encode");
        let decoded = StackResetResponse::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, response);
    }

    #[test]
    fn response_decode_rejects_malformed() {
        assert!(StackResetResponse::decode(b"not capnp").is_err());
    }
}
