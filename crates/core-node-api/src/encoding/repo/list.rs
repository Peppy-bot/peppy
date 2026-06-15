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
                entry.set_duplicate(node.duplicate);
                entry.set_repo_id(node.repo_id);
                entry.set_repo_label(&node.repo_label);
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
            let source_type_str = entry.get_source_type()?.to_str()?;
            let source_type = RepoSourceKind::parse(source_type_str).ok_or_else(|| {
                crate::Error::Decoding(format!("unknown source type: {source_type_str}"))
            })?;
            nodes.push(RepoListNodeEntry {
                node_name: entry.get_node_name()?.to_str()?.to_owned(),
                node_tag: entry.get_node_tag()?.to_str()?.to_owned(),
                source_type,
                path: entry.get_path()?.to_str()?.to_owned(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry() -> RepoListNodeEntry {
        RepoListNodeEntry {
            node_name: "robot".to_owned(),
            node_tag: "v1".to_owned(),
            source_type: RepoSourceKind::Fs,
            path: "/abs/repo/robot".to_owned(),
            duplicate: false,
            repo_id: 7,
            repo_label: "/abs/repo".to_owned(),
        }
    }

    #[test]
    fn list_request_round_trips() {
        let request = RepoListRequest;
        let payload = request.encode().expect("encode");
        let decoded = RepoListRequest::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, request);
    }

    #[test]
    fn list_response_success_round_trips_empty() {
        let response = RepoListResponse::success(Vec::new());
        let payload = response.encode().expect("encode");
        let decoded = RepoListResponse::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, response);
        assert!(decoded.success);
        assert!(decoded.error_message.is_none());
        assert!(decoded.nodes.is_empty());
    }

    #[test]
    fn list_response_success_round_trips_multiple_entries() {
        let response = RepoListResponse::success(vec![
            sample_entry(),
            RepoListNodeEntry {
                node_name: "camera".to_owned(),
                node_tag: "latest".to_owned(),
                source_type: RepoSourceKind::Git,
                path: "nodes/camera".to_owned(),
                duplicate: true,
                repo_id: 42,
                repo_label: "https://github.com/org/repo (ref: main)".to_owned(),
            },
            RepoListNodeEntry {
                node_name: "lidar".to_owned(),
                node_tag: "v2".to_owned(),
                source_type: RepoSourceKind::Url,
                path: "https://example.com/packages/lidar".to_owned(),
                duplicate: false,
                repo_id: 3,
                repo_label: "https://example.com/packages".to_owned(),
            },
        ]);
        let payload = response.encode().expect("encode");
        let decoded = RepoListResponse::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, response);
    }

    #[test]
    fn list_response_failure_round_trips() {
        let response = RepoListResponse::failure("boom");
        let payload = response.encode().expect("encode");
        let decoded = RepoListResponse::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, response);
        assert!(!decoded.success);
        assert_eq!(decoded.error_message.as_deref(), Some("boom"));
        assert!(decoded.nodes.is_empty());
    }

    #[test]
    fn repo_source_kind_as_str_parse_round_trip() {
        for kind in [RepoSourceKind::Fs, RepoSourceKind::Git, RepoSourceKind::Url] {
            assert_eq!(RepoSourceKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(RepoSourceKind::Fs.as_str(), "fs");
        assert_eq!(RepoSourceKind::Git.as_str(), "git");
        assert_eq!(RepoSourceKind::Url.as_str(), "url");
    }

    #[test]
    fn repo_source_kind_parse_rejects_unknown() {
        assert_eq!(RepoSourceKind::parse("ftp"), None);
        assert_eq!(RepoSourceKind::parse(""), None);
        assert_eq!(RepoSourceKind::parse("FS"), None);
    }

    #[test]
    fn list_response_decode_rejects_unknown_source_type() {
        // A peer that puts an unrecognized source type on the wire is rejected.
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<repo_capnp::repo_list_response::Builder>();
            response.set_success(true);
            let mut nodes = response.init_nodes(1);
            let mut entry = nodes.reborrow().get(0);
            entry.set_node_name("robot");
            entry.set_node_tag("v1");
            entry.set_source_type("bogus");
            entry.set_path("/abs/repo/robot");
            entry.set_duplicate(false);
            entry.set_repo_id(1);
            entry.set_repo_label("/abs/repo");
        }
        let payload = encode_message(&builder).expect("encode raw response");
        let err = RepoListResponse::decode(payload.as_ref())
            .expect_err("unknown source type must be rejected");
        assert!(
            matches!(err, crate::Error::Decoding(_)),
            "expected Decoding error, got {err:?}"
        );
    }

    #[test]
    fn list_request_decode_rejects_malformed_bytes() {
        RepoListRequest::decode(&[0xFF, 0xFF, 0xFF, 0xFF])
            .expect_err("malformed bytes must be rejected");
    }

    #[test]
    fn list_response_decode_rejects_malformed_bytes() {
        RepoListResponse::decode(&[0xFF, 0xFF, 0xFF, 0xFF])
            .expect_err("malformed bytes must be rejected");
    }
}
