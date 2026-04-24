use std::path::PathBuf;

use capnp::message::Builder;

use crate::node_capnp;
use crate::{Payload, Result};

use crate::encoding::{capnp_list_len, decode_message, encode_message};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSyncRequest {
    pub node_root_dir: PathBuf,
    pub git_hash: String,
    /// Peer node root directories whose `peppy.json5` should be considered
    /// when resolving this node's dependencies. Used by `node sync -a` so the
    /// daemon can resolve sibling nodes that have not been added to the
    /// persistent node stack yet. Empty for plain `node sync`.
    pub local_peers: Vec<PathBuf>,
}

impl NodeSyncRequest {
    pub fn new(
        node_root_dir: impl Into<PathBuf>,
        git_hash: impl Into<String>,
        local_peers: Vec<PathBuf>,
    ) -> Self {
        Self {
            node_root_dir: node_root_dir.into(),
            git_hash: git_hash.into(),
            local_peers,
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<node_capnp::node_generate_request::Builder>();
            request.set_node_root_dir(self.node_root_dir.to_string_lossy());
            request.set_git_hash(&self.git_hash);
            let peer_count = capnp_list_len(self.local_peers.len(), "NodeSyncRequest.local_peers")?;
            let mut peers = request.init_local_peers(peer_count);
            for (i, peer) in self.local_peers.iter().enumerate() {
                peers.set(i as u32, peer.to_string_lossy());
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<node_capnp::node_generate_request::Reader>()?;
        let git_hash = request.get_git_hash()?.to_str()?.to_owned();
        let mut local_peers = Vec::new();
        if request.has_local_peers() {
            let peers_reader = request.get_local_peers()?;
            for peer in peers_reader.iter() {
                local_peers.push(PathBuf::from(peer?.to_str()?));
            }
        }
        Ok(Self {
            node_root_dir: PathBuf::from(request.get_node_root_dir()?.to_str()?),
            git_hash,
            local_peers,
        })
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

    pub fn encode(&self) -> Result<Payload> {
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
