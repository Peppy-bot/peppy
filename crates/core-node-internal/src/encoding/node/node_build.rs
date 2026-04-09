//! Encoding types for the NodeBuild action (streaming version with feedback).

use crate::Result;
use crate::encoding::{decode_message, encode_message, optional_text};
use crate::names;
use crate::node_capnp;
use capnp::message::Builder;
use config::node::QoSProfile;
use peppylib::messaging::ActionGoalHandle;
use peppylib::types::Payload;
use peppylib::{ActionMessenger, MessengerHandle};
use std::path::PathBuf;
use std::time::Duration;

/// Goal message for the NodeBuild action. Identifies an already-`Added`
/// entity by (name, tag). Env vars and prepared working dir live on the
/// entity itself (set by `node_add`), so the build goal does not carry
/// them on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeBuildGoal {
    pub node_name: String,
    pub node_tag: String,
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
            timeout_secs,
            force: false,
        }
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
            goal.set_timeout_secs(self.timeout_secs);
            goal.set_force(self.force);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let goal = reader.get_root::<node_capnp::node_build_goal::Reader>()?;
        Ok(Self {
            node_name: goal.get_node_name()?.to_str()?.to_owned(),
            node_tag: goal.get_node_tag()?.to_str()?.to_owned(),
            timeout_secs: goal.get_timeout_secs(),
            force: goal.get_force(),
        })
    }

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
            as_core_node,
            names::NODE_BUILD_ACTION,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeBuildFeedback {
    pub stream: String,
    pub line: String,
}

impl NodeBuildFeedback {
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
            let mut feedback = builder.init_root::<node_capnp::node_build_feedback::Builder>();
            feedback.set_stream(&self.stream);
            feedback.set_line(&self.line);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let feedback = reader.get_root::<node_capnp::node_build_feedback::Reader>()?;
        Ok(Self {
            stream: feedback.get_stream()?.to_str()?.to_owned(),
            line: feedback.get_line()?.to_str()?.to_owned(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeBuildResult {
    pub snapshot_path: PathBuf,
    pub log_path: PathBuf,
    pub success: bool,
    pub error_message: Option<String>,
    pub node_name: Option<String>,
    pub node_tag: Option<String>,
}

impl NodeBuildResult {
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
            let mut result = builder.init_root::<node_capnp::node_build_result::Builder>();
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
        let result = reader.get_root::<node_capnp::node_build_result::Reader>()?;
        Ok(Self {
            snapshot_path: PathBuf::from(result.get_snapshot_path()?.to_str()?),
            log_path: PathBuf::from(result.get_log_path()?.to_str()?),
            success: result.get_success(),
            error_message: optional_text(result.get_error_message()?.to_str()?),
            node_name: optional_text(result.get_node_name()?.to_str()?),
            node_tag: optional_text(result.get_node_tag()?.to_str()?),
        })
    }

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
