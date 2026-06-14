use capnp::message::Builder;
use config::node::Toolchain;
use std::path::PathBuf;

use crate::node_capnp;
use crate::{Payload, Result};

use crate::encoding::{decode_message, encode_message};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeInitRequest {
    pub node_root_dir: PathBuf,
    pub node_name: String,
    pub git_hash: String,
    pub with_container: bool,
    pub toolchain: Toolchain,
}

impl NodeInitRequest {
    pub fn new(
        node_root_dir: impl Into<PathBuf>,
        node_name: impl Into<String>,
        git_hash: impl Into<String>,
        with_container: bool,
        toolchain: Toolchain,
    ) -> Self {
        Self {
            node_root_dir: node_root_dir.into(),
            node_name: node_name.into(),
            git_hash: git_hash.into(),
            with_container,
            toolchain,
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<node_capnp::node_init_request::Builder>();
            request.set_node_root_dir(self.node_root_dir.to_string_lossy());
            request.set_node_name(&self.node_name);
            request.set_git_hash(&self.git_hash);
            request.set_with_container(self.with_container);
            request.set_toolchain(self.toolchain.to_string());
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<node_capnp::node_init_request::Reader>()?;
        let toolchain_str = request.get_toolchain()?.to_str()?;
        let toolchain = toolchain_str.parse()?;
        Ok(Self {
            node_root_dir: PathBuf::from(request.get_node_root_dir()?.to_str()?),
            node_name: request.get_node_name()?.to_str()?.to_owned(),
            git_hash: request.get_git_hash()?.to_str()?.to_owned(),
            with_container: request.get_with_container(),
            toolchain,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeInitResponse {
    pub success: bool,
    pub error_message: String,
}

impl NodeInitResponse {
    pub fn new(success: bool, error_message: impl Into<String>) -> Self {
        Self {
            success,
            error_message: error_message.into(),
        }
    }

    pub fn success() -> Self {
        Self::new(true, "")
    }

    pub fn failure(error_message: impl Into<String>) -> Self {
        Self::new(false, error_message)
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<node_capnp::node_init_response::Builder>();
            response.set_success(self.success);
            response.set_error_message(&self.error_message);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<node_capnp::node_init_response::Reader>()?;
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
    fn init_request_round_trips_with_container() {
        let request = NodeInitRequest::new(
            "/var/lib/node-root",
            "my_node",
            "deadbeef",
            true,
            Toolchain::Cargo,
        );
        let payload = request.encode().expect("encode");
        let decoded = NodeInitRequest::decode(payload.as_ref()).expect("decode");
        assert!(decoded.with_container);
        assert_eq!(decoded, request);
    }

    #[test]
    fn init_request_round_trips_without_container() {
        let request = NodeInitRequest::new(
            "/var/lib/node-root",
            "my_node",
            "abc123",
            false,
            Toolchain::Uv,
        );
        let payload = request.encode().expect("encode");
        let decoded = NodeInitRequest::decode(payload.as_ref()).expect("decode");
        assert!(!decoded.with_container);
        assert_eq!(decoded, request);
    }

    #[test]
    fn init_request_decode_rejects_malformed_bytes() {
        NodeInitRequest::decode(b"not a capnp message")
            .expect_err("malformed bytes should be rejected");
    }

    #[test]
    fn init_response_round_trips_success() {
        let response = NodeInitResponse::success();
        assert!(response.success);
        assert_eq!(response.error_message, "");
        let payload = response.encode().expect("encode");
        let decoded = NodeInitResponse::decode(payload.as_ref()).expect("decode");
        assert!(decoded.success);
        assert!(decoded.error_message.is_empty());
        assert_eq!(decoded, response);
    }

    #[test]
    fn init_response_round_trips_failure() {
        let response = NodeInitResponse::failure("boom");
        assert!(!response.success);
        assert_eq!(response.error_message, "boom");
        let payload = response.encode().expect("encode");
        let decoded = NodeInitResponse::decode(payload.as_ref()).expect("decode");
        assert!(!decoded.success);
        assert_eq!(decoded.error_message, "boom");
        assert_eq!(decoded, response);
    }

    #[test]
    fn init_response_new_round_trips() {
        let response = NodeInitResponse::new(true, "partial");
        let payload = response.encode().expect("encode");
        let decoded = NodeInitResponse::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, response);
    }

    #[test]
    fn init_response_decode_rejects_malformed_bytes() {
        NodeInitResponse::decode(b"not a capnp message")
            .expect_err("malformed bytes should be rejected");
    }
}
