//! Encoding types for the NodeBuild action (streaming version with feedback).
//!
//! `NodeBuild` uses [`NodeActionGoalResponse`] and [`NodeActionFeedback`] —
//! the shared streaming-node-action schemas defined alongside `NodeAdd`.

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
pub struct NodeBuildResult {
    pub snapshot_path: PathBuf,
    pub log_path: PathBuf,
    pub success: bool,
    pub error_message: Option<String>,
}

impl NodeBuildResult {
    pub fn success(snapshot_path: impl Into<PathBuf>, log_path: impl Into<PathBuf>) -> Self {
        Self {
            snapshot_path: snapshot_path.into(),
            log_path: log_path.into(),
            success: true,
            error_message: None,
        }
    }

    pub fn failure(log_path: impl Into<PathBuf>, error_message: impl Into<String>) -> Self {
        Self {
            snapshot_path: PathBuf::new(),
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
            result.set_snapshot_path(self.snapshot_path.to_string_lossy().as_ref());
            result.set_log_path(self.log_path.to_string_lossy().as_ref());
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
