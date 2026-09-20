use std::fmt;

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
    ///
    /// Cross-repository shadowing: a supported feature with a documented
    /// order, where the lower-id repository deterministically wins. An
    /// identity claimed twice inside ONE repository is not representable:
    /// a repository index keys each identity once, so a second claim has
    /// nowhere to go.
    pub duplicate: bool,
    /// Id of the owning repository (from `repositories.json5`).
    pub repo_id: u32,
    /// Display label of the owning repository (path for fs, `"url (ref: r)"` for git).
    pub repo_label: String,
}

/// Read status of one configured repository, so a partial update is
/// legible: which repositories are current, which are serving entries
/// retained from an earlier read, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoListRepoEntry {
    pub id: u32,
    /// Display label (path for fs, `"url (ref: r)"` for git).
    pub label: String,
    pub source_type: RepoSourceKind,
    /// Unix seconds of the last read that produced entries. `None` when
    /// this repository has never been read successfully on this machine.
    pub last_read_unix_secs: Option<u64>,
    /// `true` when the entries listed for this repository come from an
    /// earlier read because its most recent one failed.
    pub retained: bool,
    /// `None` when the last read succeeded, otherwise the failure kind
    /// with its detail. An outage and a content bug are never collapsed
    /// into one label.
    pub failure: Option<RepoListRepoFailure>,
}

/// Why a repository's last read failed. Closed on purpose: an outage and
/// a content bug send the user to completely different places, and a
/// third kind would have to earn its own recovery path first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RepoListRepoFailureKind {
    /// The repository could not be reached (network, auth, missing path).
    Unreachable,
    /// The repository was read, but its contents do not resolve.
    Conflict,
}

impl RepoListRepoFailureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RepoListRepoFailureKind::Unreachable => "unreachable",
            RepoListRepoFailureKind::Conflict => "conflict",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "unreachable" => Some(RepoListRepoFailureKind::Unreachable),
            "conflict" => Some(RepoListRepoFailureKind::Conflict),
            _ => None,
        }
    }
}

impl fmt::Display for RepoListRepoFailureKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoListRepoFailure {
    pub kind: RepoListRepoFailureKind,
    pub detail: String,
}

/// Response message for the RepoList service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoListResponse {
    pub success: bool,
    pub error_message: Option<String>,
    pub nodes: Vec<RepoListNodeEntry>,
    pub repos: Vec<RepoListRepoEntry>,
}

impl RepoListResponse {
    pub fn success(nodes: Vec<RepoListNodeEntry>, repos: Vec<RepoListRepoEntry>) -> Self {
        Self {
            success: true,
            error_message: None,
            nodes,
            repos,
        }
    }

    pub fn failure(message: impl Into<String>) -> Self {
        Self {
            success: false,
            error_message: Some(message.into()),
            nodes: Vec::new(),
            repos: Vec::new(),
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
            let mut nodes_builder = response.reborrow().init_nodes(node_count);
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
            let repo_count = capnp_list_len(self.repos.len(), "RepoListResponse.repos")?;
            let mut repos_builder = response.init_repos(repo_count);
            for (i, repo) in self.repos.iter().enumerate() {
                let mut entry = repos_builder.reborrow().get(i as u32);
                entry.set_id(repo.id);
                entry.set_label(&repo.label);
                entry.set_source_type(repo.source_type.as_str());
                // 0 is the "never read successfully" sentinel: a real
                // read at the epoch is not a case worth modelling.
                entry.set_last_read_unix_secs(repo.last_read_unix_secs.unwrap_or(0));
                entry.set_retained(repo.retained);
                if let Some(ref failure) = repo.failure {
                    entry.set_failure_kind(failure.kind.as_str());
                    entry.set_failure_detail(&failure.detail);
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
        let repos_reader = response.get_repos()?;
        let mut repos = Vec::with_capacity(repos_reader.len() as usize);
        for i in 0..repos_reader.len() {
            let entry = repos_reader.get(i);
            let source_type_str = entry.get_source_type()?.to_str()?;
            let source_type = RepoSourceKind::parse(source_type_str).ok_or_else(|| {
                crate::Error::Decoding(format!("unknown source type: {source_type_str}"))
            })?;
            // The empty kind is the "read succeeded" sentinel; anything
            // else has to be one of the two kinds we know how to route.
            let failure = match entry.get_failure_kind()?.to_str()? {
                "" => None,
                kind => Some(RepoListRepoFailure {
                    kind: RepoListRepoFailureKind::parse(kind).ok_or_else(|| {
                        crate::Error::Decoding(format!("unknown repo failure kind: {kind}"))
                    })?,
                    detail: entry.get_failure_detail()?.to_str()?.to_owned(),
                }),
            };
            let last_read = entry.get_last_read_unix_secs();
            repos.push(RepoListRepoEntry {
                id: entry.get_id(),
                label: entry.get_label()?.to_str()?.to_owned(),
                source_type,
                last_read_unix_secs: (last_read != 0).then_some(last_read),
                retained: entry.get_retained(),
                failure,
            });
        }
        Ok(Self {
            success: response.get_success(),
            error_message: optional_text(response.get_error_message()?.to_str()?),
            nodes,
            repos,
        })
    }
}

impl crate::encoding::Wire for RepoListRequest {
    type Root = crate::repo_capnp::repo_list_request::Owned;
}

impl crate::encoding::Wire for RepoListResponse {
    type Root = crate::repo_capnp::repo_list_response::Owned;
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

    fn sample_repo() -> RepoListRepoEntry {
        RepoListRepoEntry {
            id: 7,
            label: "/abs/repo".to_owned(),
            source_type: RepoSourceKind::Fs,
            last_read_unix_secs: Some(1_753_900_000),
            retained: false,
            failure: None,
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
        let response = RepoListResponse::success(Vec::new(), Vec::new());
        let payload = response.encode().expect("encode");
        let decoded = RepoListResponse::decode(payload.as_ref()).expect("decode");
        assert_eq!(decoded, response);
        assert!(decoded.success);
        assert!(decoded.error_message.is_none());
        assert!(decoded.nodes.is_empty());
        assert!(decoded.repos.is_empty());
    }

    #[test]
    fn list_response_success_round_trips_multiple_entries() {
        let response = RepoListResponse::success(
            vec![
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
                    source_type: RepoSourceKind::Fs,
                    path: "/abs/other/lidar/peppy.json5".to_owned(),
                    duplicate: false,
                    repo_id: 3,
                    repo_label: "/abs/other".to_owned(),
                },
            ],
            vec![
                sample_repo(),
                RepoListRepoEntry {
                    id: 42,
                    label: "https://github.com/org/repo (ref: main)".to_owned(),
                    source_type: RepoSourceKind::Git,
                    last_read_unix_secs: Some(1_753_900_000),
                    retained: true,
                    failure: Some(RepoListRepoFailure {
                        kind: RepoListRepoFailureKind::Unreachable,
                        detail: "network is unreachable".to_owned(),
                    }),
                },
                RepoListRepoEntry {
                    id: 3,
                    label: "/abs/other".to_owned(),
                    source_type: RepoSourceKind::Fs,
                    last_read_unix_secs: None,
                    retained: false,
                    failure: None,
                },
                RepoListRepoEntry {
                    id: 9,
                    label: "/abs/other".to_owned(),
                    source_type: RepoSourceKind::Fs,
                    last_read_unix_secs: Some(1_753_900_001),
                    retained: true,
                    failure: Some(RepoListRepoFailure {
                        kind: RepoListRepoFailureKind::Conflict,
                        detail: "robot:v1 claimed twice".to_owned(),
                    }),
                },
            ],
        );
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
        assert!(decoded.repos.is_empty());
    }

    #[test]
    fn repo_source_kind_as_str_parse_round_trip() {
        for kind in [RepoSourceKind::Fs, RepoSourceKind::Git] {
            assert_eq!(RepoSourceKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(RepoSourceKind::Fs.as_str(), "fs");
        assert_eq!(RepoSourceKind::Git.as_str(), "git");
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
    fn repo_failure_kind_as_str_parse_round_trip() {
        for kind in [
            RepoListRepoFailureKind::Unreachable,
            RepoListRepoFailureKind::Conflict,
        ] {
            assert_eq!(RepoListRepoFailureKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(RepoListRepoFailureKind::Unreachable.as_str(), "unreachable");
        assert_eq!(RepoListRepoFailureKind::Conflict.as_str(), "conflict");
        assert_eq!(RepoListRepoFailureKind::Conflict.to_string(), "conflict");
    }

    #[test]
    fn repo_failure_kind_parse_rejects_unknown() {
        // The empty kind is the "read succeeded" sentinel, handled by the
        // decoder rather than by this mapping.
        assert_eq!(RepoListRepoFailureKind::parse(""), None);
        assert_eq!(RepoListRepoFailureKind::parse("timeout"), None);
        assert_eq!(RepoListRepoFailureKind::parse("Unreachable"), None);
    }

    #[test]
    fn list_response_decode_rejects_unknown_failure_kind() {
        // A peer that invents a failure kind is rejected rather than
        // silently handed to a caller that has no recovery path for it.
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<repo_capnp::repo_list_response::Builder>();
            response.set_success(true);
            let mut repos = response.init_repos(1);
            let mut entry = repos.reborrow().get(0);
            entry.set_id(1);
            entry.set_label("/abs/repo");
            entry.set_source_type("fs");
            entry.set_last_read_unix_secs(0);
            entry.set_retained(false);
            entry.set_failure_kind("timeout");
            entry.set_failure_detail("took too long");
        }
        let payload = encode_message(&builder).expect("encode raw response");
        let err = RepoListResponse::decode(payload.as_ref())
            .expect_err("unknown failure kind must be rejected");
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
