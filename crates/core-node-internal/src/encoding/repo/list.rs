use std::time::Duration;

use capnp::message::Builder;
use peppylib::types::Payload;
use peppylib::{MessengerHandle, ServiceMessenger};

use crate::Result;
use crate::encoding::{decode_message, encode_message, optional_text};
use crate::names;
use crate::repo_capnp;

/// Request message for the RepoList service (empty — list all).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoListRequest;

impl RepoListRequest {
    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let _req = builder.init_root::<repo_capnp::repo_list_request::Builder>();
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let _req = reader.get_root::<repo_capnp::repo_list_request::Reader>()?;
        Ok(Self)
    }

    pub async fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        target_core_node: &str,
        response_timeout: Duration,
    ) -> Result<RepoListResponse> {
        let request_payload = self.encode()?;
        let response = ServiceMessenger::poll(
            messenger,
            bound_core_node,
            as_instance_id,
            target_core_node,
            names::REPO_LIST,
            Some(target_core_node),
            None,
            request_payload,
            response_timeout,
        )
        .await?;
        RepoListResponse::decode(response.payload().as_ref())
    }
}

/// A single node entry in the repo list response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoListNodeEntry {
    pub node_name: String,
    pub node_tag: String,
    /// "fs", "git", or "url"
    pub source_type: String,
    /// Absolute path (fs) or relative path within repo (git)
    pub path: String,
}

/// Response message for the RepoList service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoListResponse {
    pub success: bool,
    pub error_message: Option<String>,
    pub nodes: Vec<RepoListNodeEntry>,
}

impl RepoListResponse {
    pub fn success(nodes: Vec<RepoListNodeEntry>) -> Self {
        Self {
            success: true,
            error_message: None,
            nodes,
        }
    }

    pub fn failure(message: impl Into<String>) -> Self {
        Self {
            success: false,
            error_message: Some(message.into()),
            nodes: Vec::new(),
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<repo_capnp::repo_list_response::Builder>();
            response.set_success(self.success);
            if let Some(ref msg) = self.error_message {
                response.set_error_message(msg);
            }
            let mut nodes_builder = response.init_nodes(self.nodes.len() as u32);
            for (i, node) in self.nodes.iter().enumerate() {
                let mut entry = nodes_builder.reborrow().get(i as u32);
                entry.set_node_name(&node.node_name);
                entry.set_node_tag(&node.node_tag);
                entry.set_source_type(&node.source_type);
                entry.set_path(&node.path);
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<repo_capnp::repo_list_response::Reader>()?;
        let nodes_reader = response.get_nodes()?;
        let mut nodes = Vec::with_capacity(nodes_reader.len() as usize);
        for i in 0..nodes_reader.len() {
            let entry = nodes_reader.get(i);
            nodes.push(RepoListNodeEntry {
                node_name: entry.get_node_name()?.to_str()?.to_owned(),
                node_tag: entry.get_node_tag()?.to_str()?.to_owned(),
                source_type: entry.get_source_type()?.to_str()?.to_owned(),
                path: entry.get_path()?.to_str()?.to_owned(),
            });
        }
        Ok(Self {
            success: response.get_success(),
            error_message: optional_text(response.get_error_message()?.to_str()?),
            nodes,
        })
    }
}
