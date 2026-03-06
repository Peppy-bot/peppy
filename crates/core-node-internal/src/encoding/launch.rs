//! Encoding types for the Launch action (streaming version with feedback).

use std::path::PathBuf;
use std::time::Duration;

use capnp::message::Builder;
use config::node::QoSProfile;
use peppylib::messaging::ActionGoalHandle;
use peppylib::types::Payload;
use peppylib::{ActionMessenger, MessengerHandle};

use crate::Result;
use crate::launch_capnp;
use crate::names;

use super::{decode_message, encode_message};

/// Default idle timeout in seconds for operations (used as fallback when 0 is received on the wire).
const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 600;
/// Default max timeout in seconds for operations (used as fallback when 0 is received on the wire).
const DEFAULT_MAX_TIMEOUT_SECS: u64 = 3600;

/// Applies a default value when a timeout field is 0 (Cap'n Proto defaults unset UInt64 to 0).
fn with_timeout_default(value: u64, default: u64) -> u64 {
    if value == 0 { default } else { value }
}

/// Goal message for the Launch action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchGoal {
    pub peppy_launch_file_path: PathBuf,
    pub env_vars: Vec<(String, String)>,
    pub node_add_idle_timeout_secs: u64,
    pub node_start_idle_timeout_secs: u64,
    pub max_timeout_secs: u64,
}

impl LaunchGoal {
    pub fn new(
        peppy_launch_file_path: impl Into<PathBuf>,
        node_add_idle_timeout_secs: u64,
        node_start_idle_timeout_secs: u64,
        max_timeout_secs: u64,
    ) -> Self {
        Self {
            peppy_launch_file_path: peppy_launch_file_path.into(),
            env_vars: Vec::new(),
            node_add_idle_timeout_secs,
            node_start_idle_timeout_secs,
            max_timeout_secs,
        }
    }

    pub fn with_env_vars(mut self, env_vars: Vec<(String, String)>) -> Self {
        self.env_vars = env_vars;
        self
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut goal = builder.init_root::<launch_capnp::launch_goal::Builder>();
            goal.set_peppy_launch_file_path(self.peppy_launch_file_path.to_string_lossy());

            let mut env_vars = goal.reborrow().init_env_vars(self.env_vars.len() as u32);
            for (idx, (key, value)) in self.env_vars.iter().enumerate() {
                let mut env_var = env_vars.reborrow().get(idx as u32);
                env_var.set_key(key);
                env_var.set_value(value);
            }

            goal.reborrow()
                .set_node_add_idle_timeout_secs(self.node_add_idle_timeout_secs);
            goal.reborrow()
                .set_node_start_idle_timeout_secs(self.node_start_idle_timeout_secs);
            goal.reborrow().set_max_timeout_secs(self.max_timeout_secs);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let goal = reader.get_root::<launch_capnp::launch_goal::Reader>()?;

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
            peppy_launch_file_path: PathBuf::from(goal.get_peppy_launch_file_path()?.to_str()?),
            env_vars,
            node_add_idle_timeout_secs: with_timeout_default(
                goal.get_node_add_idle_timeout_secs(),
                DEFAULT_IDLE_TIMEOUT_SECS,
            ),
            node_start_idle_timeout_secs: with_timeout_default(
                goal.get_node_start_idle_timeout_secs(),
                DEFAULT_IDLE_TIMEOUT_SECS,
            ),
            max_timeout_secs: with_timeout_default(
                goal.get_max_timeout_secs(),
                DEFAULT_MAX_TIMEOUT_SECS,
            ),
        })
    }

    /// Sends the goal to start the Launch action and returns a handle for receiving feedback.
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
            names::STACK_LAUNCH_ACTION,
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

/// Response to the Launch goal request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchGoalResponse {
    pub accepted: bool,
    pub log_path: PathBuf,
    pub rejection_reason: Option<String>,
}

impl LaunchGoalResponse {
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
            let mut response = builder.init_root::<launch_capnp::launch_goal_response::Builder>();
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
        let response = reader.get_root::<launch_capnp::launch_goal_response::Reader>()?;
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchFeedbackStep {
    LauncherStep,
    AddingNode,
    StartingNode,
}
/// Feedback message for the Launch action.
/// Represents a single line of output from the launch process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchFeedback {
    /// The stream type: "stdout" or "stderr"
    pub stream: String,
    /// The line of output
    pub line: String,
    /// The step in the launch process this feedback is from
    pub step: LaunchFeedbackStep,
}

impl LaunchFeedback {
    pub fn stdout(line: impl Into<String>, step: LaunchFeedbackStep) -> Self {
        Self {
            stream: "stdout".to_string(),
            line: line.into(),
            step,
        }
    }

    pub fn stderr(line: impl Into<String>, step: LaunchFeedbackStep) -> Self {
        Self {
            stream: "stderr".to_string(),
            line: line.into(),
            step,
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
            let mut feedback = builder.init_root::<launch_capnp::launch_feedback::Builder>();
            feedback.set_stream(&self.stream);
            feedback.set_line(&self.line);
            feedback.set_step(match self.step {
                LaunchFeedbackStep::LauncherStep => launch_capnp::LaunchFeedbackStep::LauncherStep,
                LaunchFeedbackStep::AddingNode => launch_capnp::LaunchFeedbackStep::AddingNode,
                LaunchFeedbackStep::StartingNode => launch_capnp::LaunchFeedbackStep::StartingNode,
            });
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let feedback = reader.get_root::<launch_capnp::launch_feedback::Reader>()?;
        let step = match feedback.get_step()? {
            launch_capnp::LaunchFeedbackStep::LauncherStep => LaunchFeedbackStep::LauncherStep,
            launch_capnp::LaunchFeedbackStep::AddingNode => LaunchFeedbackStep::AddingNode,
            launch_capnp::LaunchFeedbackStep::StartingNode => LaunchFeedbackStep::StartingNode,
        };
        Ok(Self {
            stream: feedback.get_stream()?.to_str()?.to_owned(),
            line: feedback.get_line()?.to_str()?.to_owned(),
            step,
        })
    }
}

/// Result message for the Launch action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchResult {
    pub success: bool,
    pub log_path: PathBuf,
    pub error_message: Option<String>,
}

impl LaunchResult {
    pub fn new(success: bool, log_path: impl Into<PathBuf>, error_message: Option<String>) -> Self {
        Self {
            success,
            log_path: log_path.into(),
            error_message,
        }
    }

    pub fn success(log_path: impl Into<PathBuf>) -> Self {
        Self::new(true, log_path, None)
    }

    pub fn failure(log_path: impl Into<PathBuf>, error_message: impl Into<String>) -> Self {
        Self::new(false, log_path, Some(error_message.into()))
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut result = builder.init_root::<launch_capnp::launch_result::Builder>();
            result.set_success(self.success);
            result.set_log_path(self.log_path.to_string_lossy().as_ref());
            if let Some(ref error_message) = self.error_message {
                result.set_error_message(error_message);
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let result = reader.get_root::<launch_capnp::launch_result::Reader>()?;
        let error_message_str = result.get_error_message()?.to_str()?;
        let error_message = if error_message_str.is_empty() {
            None
        } else {
            Some(error_message_str.to_owned())
        };
        let log_path = PathBuf::from(result.get_log_path()?.to_str()?);
        Ok(Self {
            success: result.get_success(),
            log_path,
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
        Self::decode(response.payload().as_ref())
    }
}
