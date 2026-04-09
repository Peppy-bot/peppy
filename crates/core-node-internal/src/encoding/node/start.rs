//! Encoding types for the NodeStart action (streaming version with feedback).

use std::time::Duration;

use capnp::message::Builder;
use config::node::QoSProfile;
use peppylib::messaging::ActionGoalHandle;
use peppylib::types::Payload;
use peppylib::{ActionMessenger, MessengerHandle};

use crate::Result;
use crate::names;
use crate::node_capnp;

use crate::encoding::{decode_message, encode_message, optional_text};

/// Goal message for the NodeStart action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeStartGoal {
    pub runtime_config_json5: String,
    pub node_name: String,
    pub tag: String,
    pub env_vars: Vec<(String, String)>,
    pub timeout_secs: u64,
}

impl NodeStartGoal {
    pub fn new(
        runtime_config_json5: impl Into<String>,
        node_name: impl Into<String>,
        tag: impl Into<String>,
        timeout_secs: u64,
    ) -> Self {
        Self {
            runtime_config_json5: runtime_config_json5.into(),
            node_name: node_name.into(),
            tag: tag.into(),
            env_vars: Vec::new(),
            timeout_secs,
        }
    }

    pub fn with_env_vars(mut self, env_vars: Vec<(String, String)>) -> Self {
        self.env_vars = env_vars;
        self
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut goal = builder.init_root::<node_capnp::node_start_goal::Builder>();
            goal.set_runtime_config_json5(&self.runtime_config_json5);
            goal.set_node_name(&self.node_name);
            goal.set_tag(&self.tag);

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
        let reader = decode_message(data)?;
        let goal = reader.get_root::<node_capnp::node_start_goal::Reader>()?;

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
            runtime_config_json5: goal.get_runtime_config_json5()?.to_str()?.to_owned(),
            node_name: goal.get_node_name()?.to_str()?.to_owned(),
            tag: goal.get_tag()?.to_str()?.to_owned(),
            env_vars,
            timeout_secs: goal.get_timeout_secs(),
        })
    }

    /// Sends the goal to start the NodeStart action and returns a handle for receiving feedback.
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
            names::NODE_START_ACTION,
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

/// Result message for the NodeStart action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeStartResult {
    pub success: bool,
    pub error_message: Option<String>,
    /// Process ID of the started node (None if not available or failed).
    pub pid: Option<u32>,
}

impl NodeStartResult {
    pub fn new(success: bool, error_message: Option<String>, pid: Option<u32>) -> Self {
        Self {
            success,
            error_message,
            pid,
        }
    }

    pub fn success(pid: u32) -> Self {
        Self::new(true, None, Some(pid))
    }

    pub fn failure(error_message: impl Into<String>) -> Self {
        Self::new(false, Some(error_message.into()), None)
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut result = builder.init_root::<node_capnp::node_start_result::Builder>();
            result.set_success(self.success);
            if let Some(ref error_message) = self.error_message {
                result.set_error_message(error_message);
            }
            result.set_pid(self.pid.unwrap_or(0));
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let result = reader.get_root::<node_capnp::node_start_result::Reader>()?;
        let error_message = optional_text(result.get_error_message()?.to_str()?);
        let pid_value = result.get_pid();
        let pid = if pid_value == 0 {
            None
        } else {
            Some(pid_value)
        };
        Ok(Self {
            success: result.get_success(),
            error_message,
            pid,
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
