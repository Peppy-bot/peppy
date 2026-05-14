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

/// Feedback message for the RepoRefresh action. Carries one of three kinds
/// of payload, disambiguated by `excluded` / `status_message` / `kind`:
///   - a discovered item (`kind` set, `item_name`/`item_tag`/`path`/`sha256` populated),
///   - an excluded repository (`excluded == true`, `path` carries the identity),
///   - a progress update (`status_message` non-empty).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRefreshFeedback {
    pub item_name: String,
    pub item_tag: String,
    /// `Some` when reporting a discovered item; `None` for excluded /
    /// progress feedback.
    pub kind: Option<RepoItemKind>,
    pub source_type: RepoSourceKind,
    /// For node and interface items, the absolute (fs) or repo-relative
    /// (git) path to the manifest file. For launchers, the same. For
    /// exclusions, the repository identity.
    pub path: String,
    /// SHA-256 of the manifest bytes. Empty for exclusion / progress.
    pub sha256: String,
    pub excluded: bool,
    /// Non-empty when this feedback is a progress/status update emitted
    /// during the scan (e.g. "Cloning <url>"). When non-empty, the other
    /// fields are meaningless.
    pub status_message: String,
}

impl RepoRefreshFeedback {
    pub fn new_node(
        item_name: impl Into<String>,
        item_tag: impl Into<String>,
        source_type: RepoSourceKind,
        path: impl Into<String>,
        sha256: impl Into<String>,
    ) -> Self {
        Self::new_item(
            RepoItemKind::Node,
            item_name,
            item_tag,
            source_type,
            path,
            sha256,
        )
    }

    pub fn new_launcher(
        item_name: impl Into<String>,
        source_type: RepoSourceKind,
        path: impl Into<String>,
        sha256: impl Into<String>,
    ) -> Self {
        Self::new_item(
            RepoItemKind::Launcher,
            item_name,
            String::new(),
            source_type,
            path,
            sha256,
        )
    }

    pub fn new_interface(
        item_name: impl Into<String>,
        item_tag: impl Into<String>,
        source_type: RepoSourceKind,
        path: impl Into<String>,
        sha256: impl Into<String>,
    ) -> Self {
        Self::new_item(
            RepoItemKind::Interface,
            item_name,
            item_tag,
            source_type,
            path,
            sha256,
        )
    }

    fn new_item(
        kind: RepoItemKind,
        item_name: impl Into<String>,
        item_tag: impl Into<String>,
        source_type: RepoSourceKind,
        path: impl Into<String>,
        sha256: impl Into<String>,
    ) -> Self {
        Self {
            item_name: item_name.into(),
            item_tag: item_tag.into(),
            kind: Some(kind),
            source_type,
            path: path.into(),
            sha256: sha256.into(),
            excluded: false,
            status_message: String::new(),
        }
    }

    /// Create a feedback entry representing an excluded repository.
    pub fn new_excluded(source_type: RepoSourceKind, identity: impl Into<String>) -> Self {
        Self {
            item_name: String::new(),
            item_tag: String::new(),
            kind: None,
            source_type,
            path: identity.into(),
            sha256: String::new(),
            excluded: true,
            status_message: String::new(),
        }
    }

    /// Create a progress feedback carrying a free-form status message.
    pub fn new_progress(message: impl Into<String>) -> Self {
        Self {
            item_name: String::new(),
            item_tag: String::new(),
            kind: None,
            source_type: RepoSourceKind::Fs,
            path: String::new(),
            sha256: String::new(),
            excluded: false,
            status_message: message.into(),
        }
    }

    pub fn encode(&self) -> Result<NonEmptyPayload> {
        let mut builder = Builder::new_default();
        {
            let mut feedback = builder.init_root::<repo_capnp::repo_refresh_feedback::Builder>();
            feedback.set_item_name(&self.item_name);
            feedback.set_item_tag(&self.item_tag);
            feedback.set_kind(self.kind.map(|k| k.as_str()).unwrap_or(""));
            feedback.set_source_type(self.source_type.as_str());
            feedback.set_path(&self.path);
            feedback.set_sha256(&self.sha256);
            feedback.set_excluded(self.excluded);
            feedback.set_status_message(&self.status_message);
        }
        encode_message_non_empty(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let feedback = reader.get_root::<repo_capnp::repo_refresh_feedback::Reader>()?;
        let source_type_str = feedback.get_source_type()?.to_str()?;
        let source_type = RepoSourceKind::parse(source_type_str).ok_or_else(|| {
            crate::Error::Decoding(format!("unknown source type: {source_type_str}"))
        })?;
        let kind_str = feedback.get_kind()?.to_str()?;
        let kind = if kind_str.is_empty() {
            None
        } else {
            Some(RepoItemKind::parse(kind_str).ok_or_else(|| {
                crate::Error::Decoding(format!("unknown repo item kind: {kind_str}"))
            })?)
        };
        Ok(Self {
            item_name: feedback.get_item_name()?.to_str()?.to_owned(),
            item_tag: feedback.get_item_tag()?.to_str()?.to_owned(),
            kind,
            source_type,
            path: feedback.get_path()?.to_str()?.to_owned(),
            sha256: feedback.get_sha256()?.to_str()?.to_owned(),
            excluded: feedback.get_excluded(),
            status_message: feedback.get_status_message()?.to_str()?.to_owned(),
        })
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
