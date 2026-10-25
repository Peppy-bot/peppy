use capnp::message::Builder;
use config::node::Toolchain;
use std::path::PathBuf;

use crate::node_capnp;
use crate::{Payload, Result};

use crate::encoding::{decode_message, encode_fs_path, encode_message};

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
            request.set_node_root_dir(encode_fs_path(&self.node_root_dir, "node_root_dir")?);
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

/// The response of a node init that succeeded. Carries no fields: a failed
/// init is an error reply, with a failure kind and a message.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NodeInitResponse;

impl NodeInitResponse {
    pub fn new() -> Self {
        Self
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            builder.init_root::<node_capnp::node_init_response::Builder>();
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        reader.get_root::<node_capnp::node_init_response::Reader>()?;
        Ok(Self)
    }
}

impl crate::encoding::Wire for NodeInitRequest {
    type Root = crate::node_capnp::node_init_request::Owned;
}

impl crate::encoding::Wire for NodeInitResponse {
    type Root = crate::node_capnp::node_init_response::Owned;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The daemon creates the node under `node_root_dir` as sent, so a path
    /// that is not UTF-8 is refused instead of reaching it with its invalid
    /// bytes replaced.
    #[cfg(unix)]
    #[test]
    fn init_request_encode_refuses_a_node_root_dir_that_is_not_utf8() {
        let request = NodeInitRequest::new(
            crate::encoding::non_utf8_path(),
            "my_node",
            "deadbeef",
            false,
            Toolchain::Cargo,
        );
        let refusal = crate::encoding::encoding_error(request.encode());
        assert!(
            refusal.contains("node_root_dir is not valid UTF-8"),
            "{refusal}"
        );
    }

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
    fn init_response_round_trips() {
        let payload = NodeInitResponse::new().encode().expect("encode");
        let decoded = NodeInitResponse::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, NodeInitResponse);
    }

    #[test]
    fn init_response_decode_rejects_malformed_bytes() {
        NodeInitResponse::decode(b"not a capnp message")
            .expect_err("malformed bytes should be rejected");
    }
}
