use capnp::message::Builder;

use crate::encoding::repo::add::RepoSourceKind;
use crate::encoding::{decode_message, encode_message, encode_message_non_empty, optional_text};
use crate::repo_capnp;
use crate::{NonEmptyPayload, Payload, Result};

/// Goal message for the RepoRefresh action (empty — refresh all repos).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRefreshGoal;

impl RepoRefreshGoal {
    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let _goal = builder.init_root::<repo_capnp::repo_refresh_goal::Builder>();
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let _goal = reader.get_root::<repo_capnp::repo_refresh_goal::Reader>()?;
        Ok(Self)
    }
}

/// Response to the RepoRefresh goal request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRefreshGoalResponse {
    pub accepted: bool,
    pub rejection_reason: Option<String>,
}

impl RepoRefreshGoalResponse {
    pub fn accepted() -> Self {
        Self {
            accepted: true,
            rejection_reason: None,
        }
    }

    pub fn rejected(reason: impl Into<String>) -> Self {
        Self {
            accepted: false,
            rejection_reason: Some(reason.into()),
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response =
                builder.init_root::<repo_capnp::repo_refresh_goal_response::Builder>();
            response.set_accepted(self.accepted);
            if let Some(ref reason) = self.rejection_reason {
                response.set_rejection_reason(reason);
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<repo_capnp::repo_refresh_goal_response::Reader>()?;
        Ok(Self {
            accepted: response.get_accepted(),
            rejection_reason: optional_text(response.get_rejection_reason()?.to_str()?),
        })
    }
}

/// Kind of item reported by a `RepoRefreshFeedback`. Carried on the wire
/// as a lowercase string so the schema stays human-readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RepoItemKind {
    Node,
    Launcher,
    Interface,
}

impl RepoItemKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RepoItemKind::Node => "node",
            RepoItemKind::Launcher => "launcher",
            RepoItemKind::Interface => "interface",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "node" => Some(RepoItemKind::Node),
            "launcher" => Some(RepoItemKind::Launcher),
            "interface" => Some(RepoItemKind::Interface),
            _ => None,
        }
    }
}

impl std::fmt::Display for RepoItemKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Feedback message for the RepoRefresh action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoRefreshFeedback {
    /// A node, launcher, or interface manifest discovered in a repository.
    Discovered {
        kind: RepoItemKind,
        item_name: String,
        /// Empty for launchers (which have no tag).
        item_tag: String,
        source_type: RepoSourceKind,
        /// Absolute path (fs) or repo-relative path (git) to the manifest file.
        path: String,
        /// SHA-256 of the manifest file bytes.
        sha256: String,
    },
    /// A repository that was skipped (listed in excluded_repositories.json5).
    Excluded {
        source_type: RepoSourceKind,
        /// Repository identity (URL or fs path).
        identity: String,
    },
    /// Free-form status update emitted during the scan (e.g. "Cloning <url>").
    Progress { message: String },
}

impl RepoRefreshFeedback {
    pub fn encode(&self) -> Result<NonEmptyPayload> {
        let mut builder = Builder::new_default();
        {
            let feedback = builder.init_root::<repo_capnp::repo_refresh_feedback::Builder>();
            let payload = feedback.init_payload();
            match self {
                Self::Discovered {
                    kind,
                    item_name,
                    item_tag,
                    source_type,
                    path,
                    sha256,
                } => {
                    let mut d = payload.init_discovered();
                    d.set_kind(kind.as_str());
                    d.set_item_name(item_name);
                    d.set_item_tag(item_tag);
                    d.set_source_type(source_type.as_str());
                    d.set_path(path);
                    d.set_sha256(sha256);
                }
                Self::Excluded {
                    source_type,
                    identity,
                } => {
                    let mut e = payload.init_excluded();
                    e.set_source_type(source_type.as_str());
                    e.set_identity(identity);
                }
                Self::Progress { message } => {
                    let mut p = payload;
                    p.set_progress(message.as_str());
                }
            }
        }
        encode_message_non_empty(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        use repo_capnp::repo_refresh_feedback::payload::Which;
        let reader = decode_message(data)?;
        let feedback = reader.get_root::<repo_capnp::repo_refresh_feedback::Reader>()?;
        match feedback.get_payload().which()? {
            Which::Discovered(d) => {
                let kind_str = d.get_kind()?.to_str()?;
                let kind = RepoItemKind::parse(kind_str).ok_or_else(|| {
                    crate::Error::Decoding(format!("unknown repo item kind: {kind_str}"))
                })?;
                let source_type_str = d.get_source_type()?.to_str()?;
                let source_type = RepoSourceKind::parse(source_type_str).ok_or_else(|| {
                    crate::Error::Decoding(format!("unknown source type: {source_type_str}"))
                })?;
                Ok(Self::Discovered {
                    kind,
                    item_name: d.get_item_name()?.to_str()?.to_owned(),
                    item_tag: d.get_item_tag()?.to_str()?.to_owned(),
                    source_type,
                    path: d.get_path()?.to_str()?.to_owned(),
                    sha256: d.get_sha256()?.to_str()?.to_owned(),
                })
            }
            Which::Excluded(e) => {
                let source_type_str = e.get_source_type()?.to_str()?;
                let source_type = RepoSourceKind::parse(source_type_str).ok_or_else(|| {
                    crate::Error::Decoding(format!("unknown source type: {source_type_str}"))
                })?;
                Ok(Self::Excluded {
                    source_type,
                    identity: e.get_identity()?.to_str()?.to_owned(),
                })
            }
            Which::Progress(p) => Ok(Self::Progress {
                message: p?.to_str()?.to_owned(),
            }),
        }
    }
}

/// Result message for the RepoRefresh action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRefreshResult {
    pub success: bool,
    pub error_message: Option<String>,
    pub total_nodes_found: u32,
    pub total_launchers_found: u32,
    pub total_interfaces_found: u32,
}

impl RepoRefreshResult {
    pub fn success(
        total_nodes_found: u32,
        total_launchers_found: u32,
        total_interfaces_found: u32,
    ) -> Self {
        Self {
            success: true,
            error_message: None,
            total_nodes_found,
            total_launchers_found,
            total_interfaces_found,
        }
    }

    pub fn failure(message: impl Into<String>) -> Self {
        Self {
            success: false,
            error_message: Some(message.into()),
            total_nodes_found: 0,
            total_launchers_found: 0,
            total_interfaces_found: 0,
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut result = builder.init_root::<repo_capnp::repo_refresh_result::Builder>();
            result.set_success(self.success);
            if let Some(ref msg) = self.error_message {
                result.set_error_message(msg);
            }
            result.set_total_nodes_found(self.total_nodes_found);
            result.set_total_launchers_found(self.total_launchers_found);
            result.set_total_interfaces_found(self.total_interfaces_found);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let result = reader.get_root::<repo_capnp::repo_refresh_result::Reader>()?;
        Ok(Self {
            success: result.get_success(),
            error_message: optional_text(result.get_error_message()?.to_str()?),
            total_nodes_found: result.get_total_nodes_found(),
            total_launchers_found: result.get_total_launchers_found(),
            total_interfaces_found: result.get_total_interfaces_found(),
        })
    }
}
