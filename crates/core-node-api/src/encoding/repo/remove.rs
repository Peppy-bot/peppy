use capnp::message::Builder;

use crate::repo_capnp;
use crate::{Payload, Result};

use crate::encoding::{decode_message, encode_message};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRemoveRequest {
    pub id: u64,
}

impl RepoRemoveRequest {
    pub fn new(id: u64) -> Self {
        Self { id }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<repo_capnp::repo_remove_request::Builder>();
            request.set_id(self.id);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<repo_capnp::repo_remove_request::Reader>()?;
        Ok(Self {
            id: request.get_id(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRemoveResponse {
    pub success: bool,
    pub error_message: String,
}

impl RepoRemoveResponse {
    pub fn success() -> Self {
        Self {
            success: true,
            error_message: String::new(),
        }
    }

    pub fn failure(message: impl Into<String>) -> Self {
        Self {
            success: false,
            error_message: message.into(),
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<repo_capnp::repo_remove_response::Builder>();
            response.set_success(self.success);
            response.set_error_message(&self.error_message);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<repo_capnp::repo_remove_response::Reader>()?;
        Ok(Self {
            success: response.get_success(),
            error_message: response.get_error_message()?.to_str()?.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_request_new_round_trips() {
        let request = RepoRemoveRequest::new(42);
        assert_eq!(request.id, 42);
        let payload = request.encode().expect("encode");
        let decoded = RepoRemoveRequest::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, request);
    }

    #[test]
    fn remove_request_round_trips_zero() {
        let request = RepoRemoveRequest::new(0);
        let payload = request.encode().expect("encode");
        let decoded = RepoRemoveRequest::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, request);
        assert_eq!(decoded.id, 0);
    }

    #[test]
    fn remove_response_success_round_trips() {
        let response = RepoRemoveResponse::success();
        let payload = response.encode().expect("encode");
        let decoded = RepoRemoveResponse::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, response);
        assert!(decoded.success);
        assert!(decoded.error_message.is_empty());
    }

    #[test]
    fn remove_response_failure_round_trips() {
        let response = RepoRemoveResponse::failure("no such repo");
        let payload = response.encode().expect("encode");
        let decoded = RepoRemoveResponse::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, response);
        assert!(!decoded.success);
        assert_eq!(decoded.error_message, "no such repo");
    }

    #[test]
    fn remove_request_decode_rejects_malformed_bytes() {
        RepoRemoveRequest::decode(&[0xFF, 0xFF, 0xFF, 0xFF])
            .expect_err("malformed bytes must be rejected");
    }

    #[test]
    fn remove_response_decode_rejects_malformed_bytes() {
        RepoRemoveResponse::decode(&[0xFF, 0xFF, 0xFF, 0xFF])
            .expect_err("malformed bytes must be rejected");
    }
}
