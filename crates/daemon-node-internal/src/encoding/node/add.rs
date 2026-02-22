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

use super::{decode_message, encode_message};

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
    },
}

/// Goal message for the NodeAdd action.
pub struct NodeAddGoal {
    pub source: NodeSource,
    pub git_hash: String,
    pub env_vars: Vec<(String, String)>,
    pub timeout_secs: u64,
}

impl NodeAddGoal {
    /// Creates a new NodeAddGoal from a NodeSource.
    pub fn from_source(source: NodeSource, git_hash: impl Into<String>, timeout_secs: u64) -> Self {
        Self {
            source,
            git_hash: git_hash.into(),
            env_vars: Vec::new(),
            timeout_secs,
        }
    }

    /// Creates a new NodeAddGoal from a filesystem path.
    pub fn new(path: impl Into<PathBuf>, git_hash: impl Into<String>, timeout_secs: u64) -> Self {
        Self {
            source: NodeSource::Fs(path.into()),
            git_hash: git_hash.into(),
            env_vars: Vec::new(),
            timeout_secs,
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
        }
    }

    /// Creates a new NodeAddGoal from an HTTP URL (for .tzst archives).
    pub fn new_http(url: url::Url, git_hash: impl Into<String>, timeout_secs: u64) -> Self {
        Self {
            source: NodeSource::Http { url },
            git_hash: git_hash.into(),
            env_vars: Vec::new(),
            timeout_secs,
        }
    }

    pub fn with_env_vars(mut self, env_vars: Vec<(String, String)>) -> Self {
        self.env_vars = env_vars;
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
                NodeSource::Http { url } => {
                    source.set_http(url.as_str());
                }
            }

            let mut env_vars = goal.reborrow().init_env_vars(self.env_vars.len() as u32);
            for (idx, (key, value)) in self.env_vars.iter().enumerate() {
                let mut env_var = env_vars.reborrow().get(idx as u32);
                env_var.set_key(key);
                env_var.set_value(value);
            }

            goal.reborrow().set_timeout_secs(self.timeout_secs);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        use crate::node_capnp::node_add_goal::source::Which;
        let reader = decode_message(data)?;
        let goal = reader.get_root::<node_capnp::node_add_goal::Reader>()?;
        let source = match goal.get_source().which()? {
            Which::Fs(fs) => NodeSource::Fs(PathBuf::from(fs?.to_str()?)),
            Which::Git(git) => {
                let git = git?;
                let repo_url_str = git.get_repo_url()?.to_str()?;
                let repo_url = GitUrl::try_from(repo_url_str)
                    .map_err(|e| crate::Error::Decoding(format!("invalid git URL: {}", e)))?;
                let repo_path = git.get_repo_path()?.to_str()?.to_owned();
                let repo_ref = git.get_repo_ref()?.to_str()?.trim().to_owned();
                let repo_ref = if repo_ref.is_empty() {
                    None
                } else {
                    Some(repo_ref)
                };
                NodeSource::Git {
                    repo_url,
                    repo_path,
                    repo_ref,
                }
            }
            Which::Http(http) => {
                let url_str = http?.to_str()?;
                let url = url::Url::parse(url_str)
                    .map_err(|e| crate::Error::Decoding(format!("invalid HTTP URL: {}", e)))?;
                NodeSource::Http { url }
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

        Ok(Self {
            source,
            git_hash: goal.get_git_hash()?.to_str()?.to_owned(),
            env_vars,
            timeout_secs: goal.get_timeout_secs(),
        })
    }

    /// Sends the goal to start the NodeAdd action and returns a handle for receiving feedback.
    pub async fn send_goal(
        &self,
        messenger: &MessengerHandle,
        as_daemon_node: &str,
        as_instance_id: &str,
        target_daemon_node: Option<&str>,
        target_instance_id: Option<&str>,
        goal_timeout: Duration,
    ) -> Result<ActionGoalHandle> {
        let goal_payload = self.encode()?;
        let handle = ActionMessenger::send_goal(
            messenger,
            as_daemon_node,
            as_instance_id,
            as_daemon_node, // node_name is the daemon node for this action
            names::NODE_ADD_ACTION,
            target_daemon_node,
            target_instance_id,
            goal_payload,
            QoSProfile::default(),
            goal_timeout,
        )
        .await?;
        Ok(handle)
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
        let rejection_reason_str = response.get_rejection_reason()?.to_str()?;
        let rejection_reason = if rejection_reason_str.is_empty() {
            None
        } else {
            Some(rejection_reason_str.to_owned())
        };
        Ok(Self {
            accepted: response.get_accepted(),
            log_path: PathBuf::from(response.get_log_path()?.to_str()?),
            rejection_reason,
        })
    }
}

/// Feedback message for the NodeAdd action.
/// Represents a single line of output from the add_cmd process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeAddFeedback {
    /// The stream type: "stdout" or "stderr"
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

    pub fn is_stdout(&self) -> bool {
        self.stream == "stdout"
    }

    pub fn is_stderr(&self) -> bool {
        self.stream == "stderr"
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
    pub node_name: String,
    pub node_tag: String,
}

impl NodeAddResult {
    pub fn new(
        snapshot_path: impl Into<PathBuf>,
        log_path: impl Into<PathBuf>,
        success: bool,
        error_message: Option<String>,
        node_name: impl Into<String>,
        node_tag: impl Into<String>,
    ) -> Self {
        Self {
            snapshot_path: snapshot_path.into(),
            log_path: log_path.into(),
            success,
            error_message,
            node_name: node_name.into(),
            node_tag: node_tag.into(),
        }
    }

    pub fn success(
        snapshot_path: impl Into<PathBuf>,
        log_path: impl Into<PathBuf>,
        node_name: impl Into<String>,
        node_tag: impl Into<String>,
    ) -> Self {
        Self::new(snapshot_path, log_path, true, None, node_name, node_tag)
    }

    pub fn failure(log_path: impl Into<PathBuf>, error_message: impl Into<String>) -> Self {
        Self::new(
            PathBuf::new(),
            log_path,
            false,
            Some(error_message.into()),
            "",
            "",
        )
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
            result.set_node_name(&self.node_name);
            result.set_node_tag(&self.node_tag);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let result = reader.get_root::<node_capnp::node_add_result::Reader>()?;
        let error_message_str = result.get_error_message()?.to_str()?;
        let error_message = if error_message_str.is_empty() {
            None
        } else {
            Some(error_message_str.to_owned())
        };
        let snapshot_path = PathBuf::from(result.get_snapshot_path()?.to_str()?);
        let log_path = PathBuf::from(result.get_log_path()?.to_str()?);
        let node_name = result.get_node_name()?.to_str()?.to_owned();
        let node_tag = result.get_node_tag()?.to_str()?.to_owned();
        Ok(Self {
            snapshot_path,
            log_path,
            success: result.get_success(),
            error_message,
            node_name,
            node_tag,
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
