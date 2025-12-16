//! Cap'n Proto encoding utilities for node messages.

use std::path::PathBuf;

use bytes::Bytes;
use capnp::message::Builder;

use crate::Result;
use crate::node_capnp;

use super::{decode_message, encode_message};

// ============================================================================
// Node List
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeListRequest;

impl NodeListRequest {
    pub fn new() -> Self {
        Self
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let _request = builder.init_root::<node_capnp::node_list_request::Builder>();
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let _request = reader.get_root::<node_capnp::node_list_request::Reader>()?;
        Ok(Self)
    }
}

impl Default for NodeListRequest {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeListResponse {
    pub dot_graph: String,
}

impl NodeListResponse {
    pub fn new(dot_graph: impl Into<String>) -> Self {
        Self {
            dot_graph: dot_graph.into(),
        }
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<node_capnp::node_list_response::Builder>();
            response.set_dot_graph(&self.dot_graph);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<node_capnp::node_list_response::Reader>()?;
        Ok(Self {
            dot_graph: response.get_dot_graph()?.to_str()?.to_owned(),
        })
    }
}

// ============================================================================
// Node Add
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeAddRequest {
    pub peppy_json5: String,
    pub from_dir: PathBuf,
}

impl NodeAddRequest {
    pub fn new(peppy_json5: impl Into<String>, from_dir: impl Into<PathBuf>) -> Self {
        Self {
            peppy_json5: peppy_json5.into(),
            from_dir: from_dir.into(),
        }
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<node_capnp::node_add_request::Builder>();
            request.set_peppy_json5(&self.peppy_json5);
            request.set_from_dir(&self.from_dir.to_string_lossy());
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<node_capnp::node_add_request::Reader>()?;
        Ok(Self {
            peppy_json5: request.get_peppy_json5()?.to_str()?.to_owned(),
            from_dir: PathBuf::from(request.get_from_dir()?.to_str()?),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeAddResponse {
    pub success: bool,
    pub node_id: String,
    pub error_message: String,
}

impl NodeAddResponse {
    pub fn new(
        success: bool,
        node_id: impl Into<String>,
        error_message: impl Into<String>,
    ) -> Self {
        Self {
            success,
            node_id: node_id.into(),
            error_message: error_message.into(),
        }
    }

    pub fn success(node_id: impl Into<String>) -> Self {
        Self::new(true, node_id, "")
    }

    pub fn failure(error_message: impl Into<String>) -> Self {
        Self::new(false, "", error_message)
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<node_capnp::node_add_response::Builder>();
            response.set_success(self.success);
            response.set_node_id(&self.node_id);
            response.set_error_message(&self.error_message);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<node_capnp::node_add_response::Reader>()?;
        Ok(Self {
            success: response.get_success(),
            node_id: response.get_node_id()?.to_str()?.to_owned(),
            error_message: response.get_error_message()?.to_str()?.to_owned(),
        })
    }
}

// ============================================================================
// Node Sync
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSyncRequest;

impl NodeSyncRequest {
    pub fn new() -> Self {
        Self
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let _request = builder.init_root::<node_capnp::node_sync_request::Builder>();
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let _request = reader.get_root::<node_capnp::node_sync_request::Reader>()?;
        Ok(Self)
    }
}

impl Default for NodeSyncRequest {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSyncResponse {
    pub success: bool,
    pub error_message: String,
}

impl NodeSyncResponse {
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

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<node_capnp::node_sync_response::Builder>();
            response.set_success(self.success);
            response.set_error_message(&self.error_message);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<node_capnp::node_sync_response::Reader>()?;
        Ok(Self {
            success: response.get_success(),
            error_message: response.get_error_message()?.to_str()?.to_owned(),
        })
    }
}
