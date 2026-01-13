//! Encoding types for the NodeStart action (streaming version with feedback).

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

/// Goal message for the NodeStart action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeStartGoal {
    pub runtime_config_json5: String,
    pub node_name: String,
    pub tag: String,
}

impl NodeStartGoal {
    pub fn new(
        runtime_config_json5: impl Into<String>,
        node_name: impl Into<String>,
        tag: impl Into<String>,
    ) -> Self {
        Self {
            runtime_config_json5: runtime_config_json5.into(),
            node_name: node_name.into(),
            tag: tag.into(),
        }
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut goal = builder.init_root::<node_capnp::node_start_goal::Builder>();
            goal.set_runtime_config_json5(&self.runtime_config_json5);
            goal.set_node_name(&self.node_name);
            goal.set_tag(&self.tag);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let goal = reader.get_root::<node_capnp::node_start_goal::Reader>()?;
        Ok(Self {
            runtime_config_json5: goal.get_runtime_config_json5()?.to_str()?.to_owned(),
            node_name: goal.get_node_name()?.to_str()?.to_owned(),
            tag: goal.get_tag()?.to_str()?.to_owned(),
        })
    }

    /// Sends the goal to start the NodeStart action and returns a handle for receiving feedback.
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
            names::NODE_START_ACTION,
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

/// Feedback message for the NodeStart action.
/// Represents a single line of output from the start_cmd process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeStartFeedback {
    /// The stream type: "stdout" or "stderr"
    pub stream: String,
    /// The line of output
    pub line: String,
}

impl NodeStartFeedback {
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
            let mut feedback = builder.init_root::<node_capnp::node_start_feedback::Builder>();
            feedback.set_stream(&self.stream);
            feedback.set_line(&self.line);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let feedback = reader.get_root::<node_capnp::node_start_feedback::Reader>()?;
        Ok(Self {
            stream: feedback.get_stream()?.to_str()?.to_owned(),
            line: feedback.get_line()?.to_str()?.to_owned(),
        })
    }
}

/// Result message for the NodeStart action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeStartResult {
    pub success: bool,
    pub error_message: Option<String>,
}

impl NodeStartResult {
    pub fn new(success: bool, error_message: Option<String>) -> Self {
        Self {
            success,
            error_message,
        }
    }

    pub fn success() -> Self {
        Self::new(true, None)
    }

    pub fn failure(error_message: impl Into<String>) -> Self {
        Self::new(false, Some(error_message.into()))
    }

    pub fn encode(&self) -> Result<Bytes> {
        let mut builder = Builder::new_default();
        {
            let mut result = builder.init_root::<node_capnp::node_start_result::Builder>();
            result.set_success(self.success);
            if let Some(ref error_message) = self.error_message {
                result.set_error_message(error_message);
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let result = reader.get_root::<node_capnp::node_start_result::Reader>()?;
        let error_message_str = result.get_error_message()?.to_str()?;
        let error_message = if error_message_str.is_empty() {
            None
        } else {
            Some(error_message_str.to_owned())
        };
        Ok(Self {
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
