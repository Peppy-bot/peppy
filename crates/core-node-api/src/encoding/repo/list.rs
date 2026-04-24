use capnp::message::Builder;

use crate::encoding::repo::add::RepoSourceKind;
use crate::encoding::{capnp_list_len, decode_message, encode_message, optional_text};
use crate::repo_capnp;
use crate::{Payload, Result};

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
}

/// A single node entry in the repo list response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoListNodeEntry {
    pub node_name: String,
    pub node_tag: String,
    pub source_type: RepoSourceKind,
    /// Absolute path (fs) or relative path within repo (git)
    pub path: String,
    /// Variant names declared by this node (empty if none).
    pub variants: Vec<String>,
    /// `true` when another repository with higher priority already provides
    /// this `(name, tag)` pair.
    pub duplicate: bool,
    /// Id of the owning repository (from `repositories.json5`).
    pub repo_id: u32,
    /// Display label of the owning repository (path for fs, `"url (ref: r)"` for git).
    pub repo_label: String,
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
            let node_count = capnp_list_len(self.nodes.len(), "RepoListResponse.nodes")?;
            let mut nodes_builder = response.init_nodes(node_count);
            for (i, node) in self.nodes.iter().enumerate() {
                let mut entry = nodes_builder.reborrow().get(i as u32);
                entry.set_node_name(&node.node_name);
                entry.set_node_tag(&node.node_tag);
                entry.set_source_type(node.source_type.as_str());
                entry.set_path(&node.path);
                entry.reborrow().set_duplicate(node.duplicate);
                entry.reborrow().set_repo_id(node.repo_id);
                entry.reborrow().set_repo_label(&node.repo_label);
                let variant_count =
                    capnp_list_len(node.variants.len(), "RepoListNodeEntry.variants")?;
                let mut variants_builder = entry.init_variants(variant_count);
                for (j, v) in node.variants.iter().enumerate() {
                    variants_builder.set(j as u32, v);
                }
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
            let variants_reader = entry.get_variants()?;
            let mut variants = Vec::with_capacity(variants_reader.len() as usize);
            for j in 0..variants_reader.len() {
                variants.push(variants_reader.get(j)?.to_str()?.to_owned());
            }
            let source_type_str = entry.get_source_type()?.to_str()?;
            let source_type = RepoSourceKind::parse(source_type_str).ok_or_else(|| {
                crate::Error::Decoding(format!("unknown source type: {source_type_str}"))
            })?;
            nodes.push(RepoListNodeEntry {
                node_name: entry.get_node_name()?.to_str()?.to_owned(),
                node_tag: entry.get_node_tag()?.to_str()?.to_owned(),
                source_type,
                path: entry.get_path()?.to_str()?.to_owned(),
                variants,
                duplicate: entry.get_duplicate(),
                repo_id: entry.get_repo_id(),
                repo_label: entry.get_repo_label()?.to_str()?.to_owned(),
            });
        }
        Ok(Self {
            success: response.get_success(),
            error_message: optional_text(response.get_error_message()?.to_str()?),
            nodes,
        })
    }
}
