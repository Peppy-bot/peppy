//! Encoding types for the NodeAdd action (streaming version with feedback).

use std::path::PathBuf;
use std::time::Duration;

use bytes::Bytes;
use capnp::message::Builder;
use config::node::QoSProfile;
use peppylib::messaging::ActionGoalHandle;
use peppylib::{ActionMessenger, MessengerHandle};

use crate::Result;
use crate::names;
use crate::node_capnp;

use super::{decode_message, encode_message};

/// Goal message for the NodeAdd action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeAddGoal {
    pub from_dir: PathBuf,
}

impl NodeAddGoal {
    pub fn new(from_dir: impl Into<PathBuf>) -> Self {
        Self {
            from_dir: from_dir.into(),
        }
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut goal = builder.init_root::<node_capnp::node_add_goal::Builder>();
            goal.set_from_dir(self.from_dir.to_string_lossy().as_ref());
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let goal = reader.get_root::<node_capnp::node_add_goal::Reader>()?;
        Ok(Self {
            from_dir: PathBuf::from(goal.get_from_dir()?.to_str()?),
        })
    }

    /// Sends the goal to start the NodeAdd action and returns a handle for receiving feedback.
    pub async fn send_goal(
        &self,
        messenger: &MessengerHandle,
        as_master_node: &str,
        as_instance_id: &str,
        target_master_node: Option<&str>,
        target_instance_id: Option<&str>,
        goal_timeout: Duration,
    ) -> Result<ActionGoalHandle> {
        let goal_payload = self.encode()?;
        let handle = ActionMessenger::send_goal(
            messenger,
            as_master_node,
            as_instance_id,
            as_master_node, // node_name is the master node for this action
            names::NODE_ADD_ACTION,
            target_master_node,
            target_instance_id,
            goal_payload,
            QoSProfile::default(),
            goal_timeout,
        )
        .await?;
        Ok(handle)
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

    pub fn encode(&self) -> Result<Bytes> {
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
    pub success: bool,
    pub error_message: Option<String>,
}

impl NodeAddResult {
    pub fn new(
        snapshot_path: impl Into<PathBuf>,
        success: bool,
        error_message: Option<String>,
    ) -> Self {
        Self {
            snapshot_path: snapshot_path.into(),
            success,
            error_message,
        }
    }

    pub fn success(snapshot_path: impl Into<PathBuf>) -> Self {
        Self::new(snapshot_path, true, None)
    }

    pub fn failure(error_message: impl Into<String>) -> Self {
        Self::new(PathBuf::new(), false, Some(error_message.into()))
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut result = builder.init_root::<node_capnp::node_add_result::Builder>();
            result.set_success(self.success);
            if let Some(ref error_message) = self.error_message {
                result.set_error_message(error_message);
            }
            result.set_snapshot_path(self.snapshot_path.to_string_lossy().as_ref());
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
        Ok(Self {
            snapshot_path,
            success: result.get_success(),
            error_message,
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
        Self::decode(&response.payload().to_bytes())
    }
}
