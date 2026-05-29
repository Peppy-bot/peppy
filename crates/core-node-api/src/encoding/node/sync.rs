use std::path::PathBuf;

use capnp::message::Builder;

use crate::node_capnp;
use crate::{Payload, Result};

use crate::encoding::RepoSourceKind;
use crate::encoding::{capnp_list_len, decode_message, encode_message};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSyncRequest {
    pub node_root_dir: PathBuf,
    pub git_hash: String,
    /// When true, dependencies missing from the persistent node stack are
    /// resolved by materializing them from the configured repositories
    /// (`~/.peppy/cache/nodes.json5`). Stack lookup still wins; the
    /// repo cache is consulted only as a fallback.
    pub include_repositories: bool,
}

impl NodeSyncRequest {
    pub fn new(
        node_root_dir: impl Into<PathBuf>,
        git_hash: impl Into<String>,
        include_repositories: bool,
    ) -> Self {
        Self {
            node_root_dir: node_root_dir.into(),
            git_hash: git_hash.into(),
            include_repositories,
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut request = builder.init_root::<node_capnp::node_generate_request::Builder>();
            request.set_node_root_dir(self.node_root_dir.to_string_lossy());
            request.set_git_hash(&self.git_hash);
            request.set_include_repositories(self.include_repositories);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let request = reader.get_root::<node_capnp::node_generate_request::Reader>()?;
        Ok(Self {
            node_root_dir: PathBuf::from(request.get_node_root_dir()?.to_str()?),
            git_hash: request.get_git_hash()?.to_str()?.to_owned(),
            include_repositories: request.get_include_repositories(),
        })
    }
}

/// One dependency the daemon resolved by fetching it through the
/// repository cache during a `node_sync` request that set
/// `include_repositories`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoResolvedEntry {
    pub name: String,
    pub tag: String,
    pub source_kind: RepoSourceKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeSyncResponse {
    pub success: bool,
    pub error_message: String,
    /// `name:tag` of every dependency resolved through the persistent
    /// node stack. Empty on failure responses.
    pub resolved_from_stack: Vec<String>,
    /// Every dependency materialized through the repository cache.
    /// Empty on failure responses or when `include_repositories` was off.
    pub resolved_from_repositories: Vec<RepoResolvedEntry>,
}

impl NodeSyncResponse {
    pub fn new(success: bool, error_message: impl Into<String>) -> Self {
        Self {
            success,
            error_message: error_message.into(),
            resolved_from_stack: Vec::new(),
            resolved_from_repositories: Vec::new(),
        }
    }

    pub fn success() -> Self {
        Self::new(true, "")
    }

    pub fn failure(error_message: impl Into<String>) -> Self {
        Self::new(false, error_message)
    }

    /// Constructs a successful response that carries dependency-resolution
    /// provenance for the CLI's verbose output.
    pub fn success_with_provenance(
        resolved_from_stack: Vec<String>,
        resolved_from_repositories: Vec<RepoResolvedEntry>,
    ) -> Self {
        Self {
            success: true,
            error_message: String::new(),
            resolved_from_stack,
            resolved_from_repositories,
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<node_capnp::node_sync_response::Builder>();
            response.set_success(self.success);
            response.set_error_message(&self.error_message);
            let stack_count = capnp_list_len(
                self.resolved_from_stack.len(),
                "NodeSyncResponse.resolved_from_stack",
            )?;
            let mut stack_list = response.reborrow().init_resolved_from_stack(stack_count);
            for (i, dep) in self.resolved_from_stack.iter().enumerate() {
                stack_list.set(i as u32, dep.as_str());
            }
            let repo_count = capnp_list_len(
                self.resolved_from_repositories.len(),
                "NodeSyncResponse.resolved_from_repositories",
            )?;
            let mut repo_list = response.init_resolved_from_repositories(repo_count);
            for (i, entry) in self.resolved_from_repositories.iter().enumerate() {
                let mut item = repo_list.reborrow().get(i as u32);
                item.set_name(&entry.name);
                item.set_tag(&entry.tag);
                item.set_source_kind(entry.source_kind.as_str());
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<node_capnp::node_sync_response::Reader>()?;
        let mut resolved_from_stack = Vec::new();
        if response.has_resolved_from_stack() {
            for dep in response.get_resolved_from_stack()?.iter() {
                resolved_from_stack.push(dep?.to_str()?.to_owned());
            }
        }
        let mut resolved_from_repositories = Vec::new();
        if response.has_resolved_from_repositories() {
            for item in response.get_resolved_from_repositories()?.iter() {
                let kind_str = item.get_source_kind()?.to_str()?;
                let source_kind = RepoSourceKind::parse(kind_str).ok_or_else(|| {
                    crate::Error::Decoding(format!(
                        "unknown source_kind '{kind_str}' in NodeSyncResponse"
                    ))
                })?;
                resolved_from_repositories.push(RepoResolvedEntry {
                    name: item.get_name()?.to_str()?.to_owned(),
                    tag: item.get_tag()?.to_str()?.to_owned(),
                    source_kind,
                });
            }
        }
        Ok(Self {
            success: response.get_success(),
            error_message: response.get_error_message()?.to_str()?.to_owned(),
            resolved_from_stack,
            resolved_from_repositories,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_sync_request_roundtrip_with_flag_off() {
        let original = NodeSyncRequest::new("/some/path", "deadbeef", false);
        let encoded = original.encode().unwrap();
        let decoded = NodeSyncRequest::decode(&encoded).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn node_sync_request_roundtrip_with_flag_on() {
        let original = NodeSyncRequest::new("/some/path", "abc123", true);
        let encoded = original.encode().unwrap();
        let decoded = NodeSyncRequest::decode(&encoded).unwrap();
        assert!(decoded.include_repositories);
        assert_eq!(decoded, original);
    }

    #[test]
    fn node_sync_response_roundtrip_carries_provenance() {
        let original = NodeSyncResponse::success_with_provenance(
            vec!["foo:v1".to_owned(), "bar:v1".to_owned()],
            vec![
                RepoResolvedEntry {
                    name: "baz".to_owned(),
                    tag: "v2".to_owned(),
                    source_kind: RepoSourceKind::Git,
                },
                RepoResolvedEntry {
                    name: "qux".to_owned(),
                    tag: "v3".to_owned(),
                    source_kind: RepoSourceKind::Fs,
                },
            ],
        );
        let encoded = original.encode().unwrap();
        let decoded = NodeSyncResponse::decode(&encoded).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn node_sync_response_failure_has_empty_provenance() {
        let original = NodeSyncResponse::failure("nope");
        let encoded = original.encode().unwrap();
        let decoded = NodeSyncResponse::decode(&encoded).unwrap();
        assert!(!decoded.success);
        assert_eq!(decoded.error_message, "nope");
        assert!(decoded.resolved_from_stack.is_empty());
        assert!(decoded.resolved_from_repositories.is_empty());
    }
}
