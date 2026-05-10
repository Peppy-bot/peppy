//! Encoding types for the NodeBuild action (streaming version with feedback).

use crate::encoding::{
    capnp_list_len, decode_message, encode_message, encode_message_non_empty, optional_text,
};
use crate::node_capnp;
use crate::{NonEmptyPayload, Payload, Result};
use capnp::message::Builder;
use std::path::PathBuf;

/// Which output stream a feedback line came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FeedbackStream {
    Stdout,
    Stderr,
    /// Out-of-band warning emitted by the daemon itself.
    Warning,
}

impl FeedbackStream {
    pub fn as_str(&self) -> &'static str {
        match self {
            FeedbackStream::Stdout => "stdout",
            FeedbackStream::Stderr => "stderr",
            FeedbackStream::Warning => "warning",
        }
    }

    pub(crate) fn to_capnp(self) -> node_capnp::FeedbackStream {
        match self {
            FeedbackStream::Stdout => node_capnp::FeedbackStream::Stdout,
            FeedbackStream::Stderr => node_capnp::FeedbackStream::Stderr,
            FeedbackStream::Warning => node_capnp::FeedbackStream::Warning,
        }
    }

    pub(crate) fn from_capnp(value: node_capnp::FeedbackStream) -> Self {
        match value {
            node_capnp::FeedbackStream::Stdout => FeedbackStream::Stdout,
            node_capnp::FeedbackStream::Stderr => FeedbackStream::Stderr,
            node_capnp::FeedbackStream::Warning => FeedbackStream::Warning,
        }
    }
}

/// Goal message for the NodeBuild action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeBuildGoal {
    pub node_name: String,
    pub node_tag: String,
    pub env_vars: Vec<(String, String)>,
    pub timeout_secs: u64,
    pub force: bool,
}

impl NodeBuildGoal {
    pub fn new(
        node_name: impl Into<String>,
        node_tag: impl Into<String>,
        timeout_secs: u64,
    ) -> Self {
        Self {
            node_name: node_name.into(),
            node_tag: node_tag.into(),
            env_vars: Vec::new(),
            timeout_secs,
            force: false,
        }
    }

    pub fn with_env_vars(mut self, env_vars: Vec<(String, String)>) -> Self {
        self.env_vars = env_vars;
        self
    }

    pub fn with_force(mut self, force: bool) -> Self {
        self.force = force;
        self
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut goal = builder.init_root::<node_capnp::node_build_goal::Builder>();
            goal.set_node_name(&self.node_name);
            goal.set_node_tag(&self.node_tag);

            let env_var_count = capnp_list_len(self.env_vars.len(), "NodeBuildGoal.env_vars")?;
            let mut env_vars = goal.reborrow().init_env_vars(env_var_count);
            for (idx, (key, value)) in self.env_vars.iter().enumerate() {
                let mut env_var = env_vars.reborrow().get(idx as u32);
                env_var.set_key(key);
                env_var.set_value(value);
            }

            goal.reborrow().set_timeout_secs(self.timeout_secs);
            goal.reborrow().set_force(self.force);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let goal = reader.get_root::<node_capnp::node_build_goal::Reader>()?;

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
            node_name: goal.get_node_name()?.to_str()?.to_owned(),
            node_tag: goal.get_node_tag()?.to_str()?.to_owned(),
            env_vars,
            timeout_secs: goal.get_timeout_secs(),
            force: goal.get_force(),
        })
    }
}

/// Response to the NodeBuild goal request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeBuildGoalResponse {
    pub accepted: bool,
    pub log_path: PathBuf,
    pub rejection_reason: Option<String>,
}

impl NodeBuildGoalResponse {
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
            let mut response = builder.init_root::<node_capnp::node_build_goal_response::Builder>();
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
        let response = reader.get_root::<node_capnp::node_build_goal_response::Reader>()?;
        Ok(Self {
            accepted: response.get_accepted(),
            log_path: PathBuf::from(response.get_log_path()?.to_str()?),
            rejection_reason: optional_text(response.get_rejection_reason()?.to_str()?),
        })
    }
}

/// Feedback message for the NodeBuild action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeBuildFeedback {
    pub stream: FeedbackStream,
    pub line: String,
}

impl NodeBuildFeedback {
    pub fn from_stream(stream: FeedbackStream, line: impl Into<String>) -> Self {
        Self {
            stream,
            line: line.into(),
        }
    }

    pub fn stdout(line: impl Into<String>) -> Self {
        Self::from_stream(FeedbackStream::Stdout, line)
    }

    pub fn stderr(line: impl Into<String>) -> Self {
        Self::from_stream(FeedbackStream::Stderr, line)
    }

    pub fn warning(line: impl Into<String>) -> Self {
        Self::from_stream(FeedbackStream::Warning, line)
    }

    pub fn is_stdout(&self) -> bool {
        self.stream == FeedbackStream::Stdout
    }

    pub fn is_stderr(&self) -> bool {
        self.stream == FeedbackStream::Stderr
    }

    pub fn is_warning(&self) -> bool {
        self.stream == FeedbackStream::Warning
    }

    pub fn encode(&self) -> Result<NonEmptyPayload> {
        let mut builder = Builder::new_default();
        {
            let mut feedback = builder.init_root::<node_capnp::node_build_feedback::Builder>();
            feedback.set_stream(self.stream.to_capnp());
            feedback.set_line(&self.line);
        }
        encode_message_non_empty(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let feedback = reader.get_root::<node_capnp::node_build_feedback::Reader>()?;
        Ok(Self {
            stream: FeedbackStream::from_capnp(feedback.get_stream()?),
            line: feedback.get_line()?.to_str()?.to_owned(),
        })
    }
}

/// Result message for the NodeBuild action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeBuildResult {
    pub artifact_path: PathBuf,
    pub log_path: PathBuf,
    pub success: bool,
    pub error_message: Option<String>,
}

impl NodeBuildResult {
    pub fn success(artifact_path: impl Into<PathBuf>, log_path: impl Into<PathBuf>) -> Self {
        Self {
            artifact_path: artifact_path.into(),
            log_path: log_path.into(),
            success: true,
            error_message: None,
        }
    }

    pub fn failure(log_path: impl Into<PathBuf>, error_message: impl Into<String>) -> Self {
        Self {
            artifact_path: PathBuf::new(),
            log_path: log_path.into(),
            success: false,
            error_message: Some(error_message.into()),
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut result = builder.init_root::<node_capnp::node_build_result::Builder>();
            result.set_success(self.success);
            if let Some(ref error_message) = self.error_message {
                result.set_error_message(error_message);
            }
            result.set_artifact_path(self.artifact_path.to_string_lossy().as_ref());
            result.set_log_path(self.log_path.to_string_lossy().as_ref());
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let result = reader.get_root::<node_capnp::node_build_result::Reader>()?;
        Ok(Self {
            artifact_path: PathBuf::from(result.get_artifact_path()?.to_str()?),
            log_path: PathBuf::from(result.get_log_path()?.to_str()?),
            success: result.get_success(),
            error_message: optional_text(result.get_error_message()?.to_str()?),
        })
    }
}
