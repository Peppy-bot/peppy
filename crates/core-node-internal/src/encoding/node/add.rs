//! Encoding types for the NodeAdd action (streaming version with feedback).

use crate::Result;
use crate::names;
use crate::node_capnp;
use capnp::message::Builder;
use config::node::QoSProfile;
use gix_url::Url as GitUrl;
use peppylib::messaging::ActionGoalHandle;
use peppylib::types::Payload;
use peppylib::{ActionMessenger, MessengerHandle};
use std::path::PathBuf;
use std::time::Duration;

use crate::encoding::{decode_message, encode_message, optional_text};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeSource {
    Fs(PathBuf),
    Git {
        repo_url: GitUrl,
        repo_path: String,
        repo_ref: Option<String>,
    },
    // Only .tzst (.tar.zstd) archives are supported for the moment
    Http {
        url: url::Url,
        sha256: Option<String>,
    },
}

impl NodeSource {
    pub fn decode_fs(path: &str) -> Result<Self> {
        if path.is_empty() {
            return Err(crate::Error::Decoding("empty filesystem path".to_owned()));
        }
        Ok(Self::Fs(PathBuf::from(path)))
    }

    pub fn decode_git(repo_url_str: &str, repo_path: &str, repo_ref: &str) -> Result<Self> {
        let repo_url = GitUrl::try_from(repo_url_str)
            .map_err(|e| crate::Error::Decoding(format!("invalid git URL: {}", e)))?;
        let repo_ref = repo_ref.trim().to_owned();
        let repo_ref = if repo_ref.is_empty() {
            None
        } else {
            Some(repo_ref)
        };
        Ok(Self::Git {
            repo_url,
            repo_path: repo_path.to_owned(),
            repo_ref,
        })
    }

    pub(crate) fn normalize_http_sha256(sha256: Option<&str>) -> Option<String> {
        sha256
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    }

    pub fn decode_http(url_str: &str, sha256: Option<&str>) -> Result<Self> {
        let url = url::Url::parse(url_str)
            .map_err(|e| crate::Error::Decoding(format!("invalid HTTP URL: {}", e)))?;
        Ok(Self::Http {
            url,
            sha256: Self::normalize_http_sha256(sha256),
        })
    }
}

/// Goal message for the NodeAdd action.
pub struct NodeAddGoal {
    pub source: NodeSource,
    pub git_hash: String,
    pub env_vars: Vec<(String, String)>,
    pub timeout_secs: u64,
    pub variant: Option<NodeSource>,
    pub force: bool,
}

impl NodeAddGoal {
    /// Creates a new NodeAddGoal from a NodeSource.
    pub fn from_source(source: NodeSource, git_hash: impl Into<String>, timeout_secs: u64) -> Self {
        Self {
            source,
            git_hash: git_hash.into(),
            env_vars: Vec::new(),
            timeout_secs,
            variant: None,
            force: false,
        }
    }

    /// Creates a new NodeAddGoal from a filesystem path.
    pub fn new(path: impl Into<PathBuf>, git_hash: impl Into<String>, timeout_secs: u64) -> Self {
        Self {
            source: NodeSource::Fs(path.into()),
            git_hash: git_hash.into(),
            env_vars: Vec::new(),
            timeout_secs,
            variant: None,
            force: false,
        }
    }

    /// Creates a new NodeAddGoal from a Git repository with an optional ref (tag/branch/commit).
    pub fn new_git(
        repo_url: GitUrl,
        repo_path: impl Into<String>,
        repo_ref: Option<String>,
        git_hash: impl Into<String>,
        timeout_secs: u64,
    ) -> Self {
        Self {
            source: NodeSource::Git {
                repo_url,
                repo_path: repo_path.into(),
                repo_ref,
            },
            git_hash: git_hash.into(),
            env_vars: Vec::new(),
            timeout_secs,
            variant: None,
            force: false,
        }
    }

    /// Creates a new NodeAddGoal from an HTTP URL (for .tzst archives).
    pub fn new_http(
        url: url::Url,
        sha256: Option<String>,
        git_hash: impl Into<String>,
        timeout_secs: u64,
    ) -> Self {
        Self {
            source: NodeSource::Http { url, sha256 },
            git_hash: git_hash.into(),
            env_vars: Vec::new(),
            timeout_secs,
            variant: None,
            force: false,
        }
    }

    pub fn with_env_vars(mut self, env_vars: Vec<(String, String)>) -> Self {
        self.env_vars = env_vars;
        self
    }

    pub fn with_variant_name(mut self, name: impl Into<String>) -> Self {
        self.variant = Some(NodeSource::Fs(PathBuf::from(name.into())));
        self
    }

    pub fn with_variant_source(mut self, source: NodeSource) -> Self {
        self.variant = Some(source);
        self
    }

    pub fn with_force(mut self, force: bool) -> Self {
        self.force = force;
        self
    }

    /// Returns the filesystem path if the source is `Fs`, otherwise `None`.
    pub fn fs_path(&self) -> Option<&PathBuf> {
        match &self.source {
            NodeSource::Fs(path) => Some(path),
            NodeSource::Git { .. } | NodeSource::Http { .. } => None,
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut goal = builder.init_root::<node_capnp::node_add_goal::Builder>();
            goal.set_git_hash(&self.git_hash);
            let mut source = goal.reborrow().init_source();
            match &self.source {
                NodeSource::Fs(path) => {
                    source.set_fs(path.to_string_lossy().as_ref());
                }
                NodeSource::Git {
                    repo_url,
                    repo_path,
                    repo_ref,
                } => {
                    let mut git = source.init_git();
                    git.set_repo_url(repo_url.to_bstring().to_string());
                    git.set_repo_path(repo_path);
                    git.set_repo_ref(repo_ref.as_deref().unwrap_or(""));
                }
                NodeSource::Http { url, sha256 } => {
                    source.set_http(url.as_str());
                    if let Some(digest) = NodeSource::normalize_http_sha256(sha256.as_deref()) {
                        goal.reborrow().set_http_sha256(&digest);
                    }
                }
            }

            let mut env_vars = goal.reborrow().init_env_vars(self.env_vars.len() as u32);
            for (idx, (key, value)) in self.env_vars.iter().enumerate() {
                let mut env_var = env_vars.reborrow().get(idx as u32);
                env_var.set_key(key);
                env_var.set_value(value);
            }

            goal.reborrow().set_timeout_secs(self.timeout_secs);
            goal.reborrow().set_force(self.force);

            if let Some(ref variant) = self.variant {
                let mut variant_builder = goal.reborrow().init_variant();
                let mut variant_source = variant_builder.reborrow().init_source();
                match variant {
                    NodeSource::Fs(name) => {
                        variant_source.set_fs(name.to_string_lossy().as_ref());
                    }
                    NodeSource::Git {
                        repo_url,
                        repo_path,
                        repo_ref,
                    } => {
                        let mut git = variant_source.init_git();
                        git.set_repo_url(repo_url.to_bstring().to_string());
                        git.set_repo_path(repo_path);
                        git.set_repo_ref(repo_ref.as_deref().unwrap_or(""));
                    }
                    NodeSource::Http { url, sha256 } => {
                        variant_source.set_http(url.as_str());
                        if let Some(digest) = NodeSource::normalize_http_sha256(sha256.as_deref()) {
                            variant_builder.set_http_sha256(&digest);
                        }
                    }
                }
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        use crate::node_capnp::node_add_goal::source::Which;
        let reader = decode_message(data)?;
        let goal = reader.get_root::<node_capnp::node_add_goal::Reader>()?;
        let source = match goal.get_source().which()? {
            Which::Fs(fs) => NodeSource::decode_fs(fs?.to_str()?)?,
            Which::Git(git) => {
                let git = git?;
                NodeSource::decode_git(
                    git.get_repo_url()?.to_str()?,
                    git.get_repo_path()?.to_str()?,
                    git.get_repo_ref()?.to_str()?,
                )?
            }
            Which::Http(http) => {
                NodeSource::decode_http(http?.to_str()?, Some(goal.get_http_sha256()?.to_str()?))?
            }
        };

        let env_vars_reader = goal.get_env_vars()?;
        let mut env_vars = Vec::with_capacity(env_vars_reader.len() as usize);
        for idx in 0..env_vars_reader.len() {
            let env_var = env_vars_reader.get(idx);
            env_vars.push((
                env_var.get_key()?.to_str()?.to_owned(),
                env_var.get_value()?.to_str()?.to_owned(),
            ));
        }

        let variant = if goal.has_variant() {
            use crate::node_capnp::node_add_variant_source::source::Which;
            let variant_reader = goal.get_variant()?;
            match variant_reader.get_source().which()? {
                Which::Fs(fs) => Some(NodeSource::decode_fs(fs?.to_str()?)?),
                Which::Git(git) => {
                    let git = git?;
                    Some(NodeSource::decode_git(
                        git.get_repo_url()?.to_str()?,
                        git.get_repo_path()?.to_str()?,
                        git.get_repo_ref()?.to_str()?,
                    )?)
                }
                Which::Http(http) => Some(NodeSource::decode_http(
                    http?.to_str()?,
                    Some(variant_reader.get_http_sha256()?.to_str()?),
                )?),
            }
        } else {
            None
        };

        Ok(Self {
            source,
            git_hash: goal.get_git_hash()?.to_str()?.to_owned(),
            env_vars,
            timeout_secs: goal.get_timeout_secs(),
            variant,
            force: goal.get_force(),
        })
    }

    /// Sends the goal to start the NodeAdd action and returns a handle for receiving feedback.
    pub async fn send_goal(
        &self,
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        target_core_node: Option<&str>,
        target_instance_id: Option<&str>,
        goal_timeout: Duration,
    ) -> Result<ActionGoalHandle> {
        let goal_payload = self.encode()?;
        let handle = ActionMessenger::send_goal(
            messenger,
            as_core_node,
            as_instance_id,
            as_core_node, // node_name is the core node for this action
            names::NODE_ADD_ACTION,
            target_core_node,
            target_instance_id,
            goal_payload,
            QoSProfile::default(),
            goal_timeout,
        )
        .await?;
        Ok(handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_fs_rejects_empty_path() {
        let result = NodeSource::decode_fs("");
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("empty filesystem path"),
            "unexpected error: {err_msg}"
        );
    }

    #[test]
    fn decode_fs_accepts_non_empty_path() {
        let source = NodeSource::decode_fs("/some/path").expect("should accept non-empty path");
        assert_eq!(source, NodeSource::Fs(PathBuf::from("/some/path")));
    }

    #[test]
    fn node_add_goal_decode_rejects_empty_fs_source() {
        // Encode a goal with an empty Fs path (bypass decode_fs by using NodeSource::Fs directly)
        let goal = NodeAddGoal {
            source: NodeSource::Fs(PathBuf::from("")),
            git_hash: "hash".to_owned(),
            env_vars: vec![],
            timeout_secs: 30,
            variant: None,
            force: false,
        };
        let encoded = goal.encode().expect("encoding should succeed");
        let result = NodeAddGoal::decode(&encoded);
        assert!(result.is_err(), "decoding an empty Fs source should fail");
    }

    #[test]
    fn node_add_goal_http_source_roundtrips_sha256() {
        let url = url::Url::parse("https://example.com/node.tar.zst").unwrap();
        let sha256 = "a".repeat(64);

        let encoded = NodeAddGoal::new_http(url.clone(), Some(sha256.clone()), "git-hash", 42)
            .encode()
            .expect("encoding should succeed");
        let decoded = NodeAddGoal::decode(&encoded).expect("decoding should succeed");

        assert_eq!(
            decoded.source,
            NodeSource::Http {
                url,
                sha256: Some(sha256)
            }
        );
    }

    #[test]
    fn node_add_goal_http_variant_roundtrips_sha256() {
        let url = url::Url::parse("https://example.com/variant.tar.zst").unwrap();
        let sha256 = "b".repeat(64);

        let encoded = NodeAddGoal::new("/some/path", "git-hash", 42)
            .with_variant_source(NodeSource::Http {
                url: url.clone(),
                sha256: Some(sha256.clone()),
            })
            .encode()
            .expect("encoding should succeed");
        let decoded = NodeAddGoal::decode(&encoded).expect("decoding should succeed");

        assert_eq!(
            decoded.variant,
            Some(NodeSource::Http {
                url,
                sha256: Some(sha256)
            })
        );
    }
}

/// Response to the NodeAdd goal request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeAddGoalResponse {
    pub accepted: bool,
    pub log_path: PathBuf,
    pub rejection_reason: Option<String>,
}

impl NodeAddGoalResponse {
    pub fn accepted(log_path: impl Into<PathBuf>) -> Self {
        Self {
            accepted: true,
            log_path: log_path.into(),
            rejection_reason: None,
        }
    }

    pub fn rejected(reason: impl Into<String>) -> Self {
        Self {
            accepted: false,
            log_path: PathBuf::new(),
            rejection_reason: Some(reason.into()),
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response = builder.init_root::<node_capnp::node_add_goal_response::Builder>();
            response.set_accepted(self.accepted);
            response.set_log_path(self.log_path.to_string_lossy().as_ref());
            if let Some(ref reason) = self.rejection_reason {
                response.set_rejection_reason(reason);
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<node_capnp::node_add_goal_response::Reader>()?;
        Ok(Self {
            accepted: response.get_accepted(),
            log_path: PathBuf::from(response.get_log_path()?.to_str()?),
            rejection_reason: optional_text(response.get_rejection_reason()?.to_str()?),
        })
    }
}

/// Feedback message for the NodeAdd action.
/// Represents a single line of output from the add_cmd process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeAddFeedback {
    /// The stream type: "stdout", "stderr" or "warning"
    pub stream: String,
    /// The line of output
    pub line: String,
}

impl NodeAddFeedback {
    pub fn stdout(line: impl Into<String>) -> Self {
        Self {
            stream: "stdout".to_string(),
            line: line.into(),
        }
    }

    pub fn stderr(line: impl Into<String>) -> Self {
        Self {
            stream: "stderr".to_string(),
            line: line.into(),
        }
    }

    pub fn warning(line: impl Into<String>) -> Self {
        Self {
            stream: "warning".to_string(),
            line: line.into(),
        }
    }

    pub fn is_stdout(&self) -> bool {
        self.stream == "stdout"
    }

    pub fn is_stderr(&self) -> bool {
        self.stream == "stderr"
    }

    pub fn is_warning(&self) -> bool {
        self.stream == "warning"
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut feedback = builder.init_root::<node_capnp::node_add_feedback::Builder>();
            feedback.set_stream(&self.stream);
            feedback.set_line(&self.line);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let feedback = reader.get_root::<node_capnp::node_add_feedback::Reader>()?;
        Ok(Self {
            stream: feedback.get_stream()?.to_str()?.to_owned(),
            line: feedback.get_line()?.to_str()?.to_owned(),
        })
    }
}

/// Result message for the NodeAdd action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeAddResult {
    pub snapshot_path: PathBuf,
    pub log_path: PathBuf,
    pub success: bool,
    pub error_message: Option<String>,
    pub node_name: Option<String>,
    pub node_tag: Option<String>,
}

impl NodeAddResult {
    pub fn success(
        snapshot_path: impl Into<PathBuf>,
        log_path: impl Into<PathBuf>,
        node_name: impl Into<String>,
        node_tag: impl Into<String>,
    ) -> Self {
        Self {
            snapshot_path: snapshot_path.into(),
            log_path: log_path.into(),
            success: true,
            error_message: None,
            node_name: Some(node_name.into()),
            node_tag: Some(node_tag.into()),
        }
    }

    pub fn failure(log_path: impl Into<PathBuf>, error_message: impl Into<String>) -> Self {
        Self {
            snapshot_path: PathBuf::new(),
            log_path: log_path.into(),
            success: false,
            error_message: Some(error_message.into()),
            node_name: None,
            node_tag: None,
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut result = builder.init_root::<node_capnp::node_add_result::Builder>();
            result.set_success(self.success);
            if let Some(ref error_message) = self.error_message {
                result.set_error_message(error_message);
            }
            result.set_snapshot_path(self.snapshot_path.to_string_lossy().as_ref());
            result.set_log_path(self.log_path.to_string_lossy().as_ref());
            if let Some(ref node_name) = self.node_name {
                result.set_node_name(node_name);
            }
            if let Some(ref node_tag) = self.node_tag {
                result.set_node_tag(node_tag);
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let result = reader.get_root::<node_capnp::node_add_result::Reader>()?;
        let snapshot_path = PathBuf::from(result.get_snapshot_path()?.to_str()?);
        let log_path = PathBuf::from(result.get_log_path()?.to_str()?);
        Ok(Self {
            snapshot_path,
            log_path,
            success: result.get_success(),
            error_message: optional_text(result.get_error_message()?.to_str()?),
            node_name: optional_text(result.get_node_name()?.to_str()?),
            node_tag: optional_text(result.get_node_tag()?.to_str()?),
        })
    }

    /// Request the result from a completed action.
    pub async fn request_result(
        messenger: &MessengerHandle,
        action_handle: &ActionGoalHandle,
        result_timeout: Duration,
    ) -> Result<Self> {
        let response =
            ActionMessenger::request_result(messenger, action_handle, result_timeout).await?;
        Self::decode(response.payload().as_ref())
    }
}
