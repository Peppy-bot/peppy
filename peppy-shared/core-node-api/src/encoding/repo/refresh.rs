use capnp::message::Builder;

use crate::encoding::repo::add::{RepoSourceKind, decode_source_kind};
use crate::encoding::{decode_message, encode_message, encode_message_non_empty};
use crate::repo_capnp;
use crate::{NonEmptyPayload, Payload, Result};

/// Goal message for the RepoRefresh action (empty: refresh all repos).
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

/// The goal response of an accepted repository refresh. Carries no fields:
/// the daemon refuses a goal in the goal envelope, with a failure kind and a
/// message.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RepoRefreshGoalResponse;

impl RepoRefreshGoalResponse {
    pub fn new() -> Self {
        Self
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        builder.init_root::<repo_capnp::repo_refresh_goal_response::Builder>();
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        reader.get_root::<repo_capnp::repo_refresh_goal_response::Reader>()?;
        Ok(Self)
    }
}

/// Kind of item reported by a `RepoRefreshFeedback`. Carried on the wire
/// as a lowercase string so the schema stays human-readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RepoItemKind {
    Node,
    Launcher,
    Contract,
    Pairing,
    McpExposure,
}

impl RepoItemKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RepoItemKind::Node => "node",
            RepoItemKind::Launcher => "launcher",
            RepoItemKind::Contract => "contract",
            RepoItemKind::Pairing => "pairing",
            RepoItemKind::McpExposure => "mcp_exposure",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "node" => Some(RepoItemKind::Node),
            "launcher" => Some(RepoItemKind::Launcher),
            "contract" => Some(RepoItemKind::Contract),
            "pairing" => Some(RepoItemKind::Pairing),
            "mcp_exposure" => Some(RepoItemKind::McpExposure),
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
    /// A node, launcher, contract, pairing, or MCP exposure manifest
    /// discovered in a repository.
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
    /// Free-form status update emitted during the scan (e.g. `Cloning <url>`).
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
                let source_type = decode_source_kind(d.get_source_type()?.to_str()?)?;
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
                let source_type = decode_source_kind(e.get_source_type()?.to_str()?)?;
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

/// The result of a completed repository refresh: it ran, it published the
/// caches, and every repository that read cleanly is current. A failed
/// refresh gives a failure kind and a message, and no result; a cancelled
/// refresh publishes nothing and gives its reason, and no result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoRefreshResult {
    pub total_nodes_found: u32,
    pub total_launchers_found: u32,
    pub total_contracts_found: u32,
    pub total_pairings_found: u32,
    pub total_mcp_exposures_found: u32,
    /// The repositories that could not be read this run, each still
    /// serving the entries it last published, and why. Empty when every
    /// configured repository was read.
    pub read_failure_report: String,
}

impl RepoRefreshResult {
    pub fn new(
        total_nodes_found: u32,
        total_launchers_found: u32,
        total_contracts_found: u32,
        total_pairings_found: u32,
        total_mcp_exposures_found: u32,
    ) -> Self {
        Self {
            total_nodes_found,
            total_launchers_found,
            total_contracts_found,
            total_pairings_found,
            total_mcp_exposures_found,
            read_failure_report: String::new(),
        }
    }

    /// The refresh ran and published its caches, but the repositories that
    /// `report` names could not be read. The counts are the ones the caches
    /// actually hold, retained entries included.
    pub fn with_read_failure_report(mut self, report: impl Into<String>) -> Self {
        self.read_failure_report = report.into();
        self
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut result = builder.init_root::<repo_capnp::repo_refresh_result::Builder>();
            result.set_total_nodes_found(self.total_nodes_found);
            result.set_total_launchers_found(self.total_launchers_found);
            result.set_total_contracts_found(self.total_contracts_found);
            result.set_total_pairings_found(self.total_pairings_found);
            result.set_total_mcp_exposures_found(self.total_mcp_exposures_found);
            result.set_read_failure_report(&self.read_failure_report);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let result = reader.get_root::<repo_capnp::repo_refresh_result::Reader>()?;
        Ok(Self {
            total_nodes_found: result.get_total_nodes_found(),
            total_launchers_found: result.get_total_launchers_found(),
            total_contracts_found: result.get_total_contracts_found(),
            total_pairings_found: result.get_total_pairings_found(),
            total_mcp_exposures_found: result.get_total_mcp_exposures_found(),
            read_failure_report: result.get_read_failure_report()?.to_str()?.to_owned(),
        })
    }
}

impl crate::encoding::Wire for RepoRefreshGoal {
    type Root = crate::repo_capnp::repo_refresh_goal::Owned;
}

impl crate::encoding::Wire for RepoRefreshGoalResponse {
    type Root = crate::repo_capnp::repo_refresh_goal_response::Owned;
}

impl crate::encoding::Wire for RepoRefreshFeedback {
    type Root = crate::repo_capnp::repo_refresh_feedback::Owned;
}

impl crate::encoding::Wire for RepoRefreshResult {
    type Root = crate::repo_capnp::repo_refresh_result::Owned;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::decoding_error;

    #[test]
    fn goal_roundtrips() {
        let goal = RepoRefreshGoal;
        let bytes = goal.encode().expect("encode");
        assert_eq!(RepoRefreshGoal::decode(&bytes).expect("decode"), goal);
    }

    #[test]
    fn goal_decode_rejects_malformed() {
        assert!(RepoRefreshGoal::decode(b"not capnp").is_err());
    }

    #[test]
    fn goal_response_roundtrips() {
        let bytes = RepoRefreshGoalResponse::new().encode().expect("encode");
        assert_eq!(
            RepoRefreshGoalResponse::decode(&bytes).expect("decode"),
            RepoRefreshGoalResponse
        );
    }

    #[test]
    fn goal_response_decode_rejects_malformed() {
        assert!(RepoRefreshGoalResponse::decode(b"not capnp").is_err());
    }

    #[test]
    fn item_kind_as_str_parse_roundtrips() {
        for kind in [
            RepoItemKind::Node,
            RepoItemKind::Launcher,
            RepoItemKind::Contract,
            RepoItemKind::Pairing,
            RepoItemKind::McpExposure,
        ] {
            assert_eq!(RepoItemKind::parse(kind.as_str()), Some(kind));
        }
    }

    #[test]
    fn item_kind_as_str_values() {
        assert_eq!(RepoItemKind::Node.as_str(), "node");
        assert_eq!(RepoItemKind::Launcher.as_str(), "launcher");
        assert_eq!(RepoItemKind::Contract.as_str(), "contract");
        assert_eq!(RepoItemKind::Pairing.as_str(), "pairing");
        assert_eq!(RepoItemKind::McpExposure.as_str(), "mcp_exposure");
    }

    #[test]
    fn item_kind_parse_rejects_unknown() {
        assert_eq!(RepoItemKind::parse("bogus"), None);
        assert_eq!(RepoItemKind::parse(""), None);
        assert_eq!(RepoItemKind::parse("Node"), None);
    }

    #[test]
    fn item_kind_display_matches_as_str() {
        assert_eq!(RepoItemKind::Launcher.to_string(), "launcher");
    }

    #[test]
    fn feedback_discovered_roundtrips() {
        let feedback = RepoRefreshFeedback::Discovered {
            kind: RepoItemKind::Node,
            item_name: "planner".to_string(),
            item_tag: "v1".to_string(),
            source_type: RepoSourceKind::Git,
            path: "nodes/planner/manifest.json5".to_string(),
            sha256: "abc123".to_string(),
        };
        let bytes = feedback.encode().expect("encode").into_inner();
        assert_eq!(
            RepoRefreshFeedback::decode(bytes.as_ref()).expect("decode"),
            feedback
        );
    }

    #[test]
    fn feedback_discovered_empty_tag_roundtrips() {
        // Launchers carry an empty tag.
        let feedback = RepoRefreshFeedback::Discovered {
            kind: RepoItemKind::Launcher,
            item_name: "bringup".to_string(),
            item_tag: String::new(),
            source_type: RepoSourceKind::Fs,
            path: "/abs/path/launcher.json5".to_string(),
            sha256: "def456".to_string(),
        };
        let bytes = feedback.encode().expect("encode").into_inner();
        assert_eq!(
            RepoRefreshFeedback::decode(bytes.as_ref()).expect("decode"),
            feedback
        );
    }

    #[test]
    fn feedback_excluded_roundtrips() {
        let feedback = RepoRefreshFeedback::Excluded {
            source_type: RepoSourceKind::Git,
            identity: "https://github.com/org/repo@main".to_string(),
        };
        let bytes = feedback.encode().expect("encode").into_inner();
        assert_eq!(
            RepoRefreshFeedback::decode(bytes.as_ref()).expect("decode"),
            feedback
        );
    }

    #[test]
    fn feedback_progress_roundtrips() {
        let feedback = RepoRefreshFeedback::Progress {
            message: "Cloning https://example.com/repo".to_string(),
        };
        let bytes = feedback.encode().expect("encode").into_inner();
        assert_eq!(
            RepoRefreshFeedback::decode(bytes.as_ref()).expect("decode"),
            feedback
        );
    }

    /// Both feedback arms that name a source refuse a source type outside
    /// the two kinds.
    #[test]
    fn feedback_decode_refuses_an_unknown_source_type() {
        let mut discovered = Builder::new_default();
        {
            let feedback = discovered.init_root::<repo_capnp::repo_refresh_feedback::Builder>();
            let mut payload = feedback.init_payload().init_discovered();
            payload.set_kind(RepoItemKind::Node.as_str());
            payload.set_source_type("bogus");
        }
        let mut excluded = Builder::new_default();
        {
            let feedback = excluded.init_root::<repo_capnp::repo_refresh_feedback::Builder>();
            feedback
                .init_payload()
                .init_excluded()
                .set_source_type("bogus");
        }
        for builder in [discovered, excluded] {
            let payload = encode_message(&builder).expect("encode raw feedback");
            assert_eq!(
                decoding_error(RepoRefreshFeedback::decode(payload.as_ref())),
                "unknown source type: bogus"
            );
        }
    }

    #[test]
    fn feedback_decode_rejects_malformed() {
        assert!(RepoRefreshFeedback::decode(b"not capnp").is_err());
    }

    #[test]
    fn result_roundtrips() {
        let result = RepoRefreshResult::new(3, 1, 2, 4, 5);
        assert_eq!(result.total_nodes_found, 3);
        assert_eq!(result.total_launchers_found, 1);
        assert_eq!(result.total_contracts_found, 2);
        assert_eq!(result.total_pairings_found, 4);
        assert_eq!(result.total_mcp_exposures_found, 5);
        assert!(result.read_failure_report.is_empty());
        let bytes = result.encode().expect("encode");
        assert_eq!(RepoRefreshResult::decode(&bytes).expect("decode"), result);
    }

    /// A refresh that read most of its repositories reports the counts it
    /// published *and* the ones it could not read, so a caller can act on
    /// the problem without treating the run as a failure.
    #[test]
    fn result_with_read_failure_report_roundtrips() {
        let result = RepoRefreshResult::new(3, 1, 2, 4, 5)
            .with_read_failure_report("1 of the configured repositories could not be updated.");
        assert_eq!(result.total_nodes_found, 3);
        assert_eq!(
            result.read_failure_report,
            "1 of the configured repositories could not be updated."
        );
        let bytes = result.encode().expect("encode");
        assert_eq!(RepoRefreshResult::decode(&bytes).expect("decode"), result);
    }

    #[test]
    fn result_decode_rejects_malformed() {
        assert!(RepoRefreshResult::decode(b"not capnp").is_err());
    }
}
