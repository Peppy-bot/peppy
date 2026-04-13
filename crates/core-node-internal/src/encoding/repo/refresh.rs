use std::time::Duration;

use capnp::message::Builder;
use config::node::QoSProfile;
use peppylib::messaging::ActionGoalHandle;
use peppylib::types::Payload;
use peppylib::{ActionMessenger, MessengerHandle};

use crate::Result;
use crate::encoding::repo::add::RepoSourceKind;
use crate::encoding::{decode_message, encode_message, optional_text};
use crate::names;
use crate::repo_capnp;

/// Goal message for the RepoRefresh action (empty — refresh all repos).
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
            names::REPO_REFRESH_ACTION,
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

/// Response to the RepoRefresh goal request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRefreshGoalResponse {
    pub accepted: bool,
    pub rejection_reason: Option<String>,
}

impl RepoRefreshGoalResponse {
    pub fn accepted() -> Self {
        Self {
            accepted: true,
            rejection_reason: None,
        }
    }

    pub fn rejected(reason: impl Into<String>) -> Self {
        Self {
            accepted: false,
            rejection_reason: Some(reason.into()),
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut response =
                builder.init_root::<repo_capnp::repo_refresh_goal_response::Builder>();
            response.set_accepted(self.accepted);
            if let Some(ref reason) = self.rejection_reason {
                response.set_rejection_reason(reason);
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let response = reader.get_root::<repo_capnp::repo_refresh_goal_response::Reader>()?;
        Ok(Self {
            accepted: response.get_accepted(),
            rejection_reason: optional_text(response.get_rejection_reason()?.to_str()?),
        })
    }
}

/// Feedback message for the RepoRefresh action.
/// Represents a single discovered node or an excluded repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRefreshFeedback {
    pub node_name: String,
    pub node_tag: String,
    pub source_type: RepoSourceKind,
    /// Absolute path (fs) or relative path within repo (git)
    pub path: String,
    /// Variant names declared by this node (empty if none).
    pub variants: Vec<String>,
    /// `true` when this feedback represents an excluded repository.
    pub excluded: bool,
}

impl RepoRefreshFeedback {
    pub fn new(
        node_name: impl Into<String>,
        node_tag: impl Into<String>,
        source_type: RepoSourceKind,
        path: impl Into<String>,
        variants: Vec<String>,
    ) -> Self {
        Self {
            node_name: node_name.into(),
            node_tag: node_tag.into(),
            source_type,
            path: path.into(),
            variants,
            excluded: false,
        }
    }

    /// Create a feedback entry representing an excluded repository.
    pub fn new_excluded(source_type: RepoSourceKind, identity: impl Into<String>) -> Self {
        Self {
            node_name: String::new(),
            node_tag: String::new(),
            source_type,
            path: identity.into(),
            variants: Vec::new(),
            excluded: true,
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut feedback = builder.init_root::<repo_capnp::repo_refresh_feedback::Builder>();
            feedback.set_node_name(&self.node_name);
            feedback.set_node_tag(&self.node_tag);
            feedback.set_source_type(self.source_type.as_str());
            feedback.set_path(&self.path);
            feedback.set_excluded(self.excluded);
            let mut variants_builder = feedback.init_variants(self.variants.len() as u32);
            for (i, v) in self.variants.iter().enumerate() {
                variants_builder.set(i as u32, v);
            }
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let feedback = reader.get_root::<repo_capnp::repo_refresh_feedback::Reader>()?;
        let source_type_str = feedback.get_source_type()?.to_str()?;
        let source_type = RepoSourceKind::parse(source_type_str).ok_or_else(|| {
            crate::Error::Decoding(format!("unknown source type: {source_type_str}"))
        })?;
        let variants_reader = feedback.get_variants()?;
        let mut variants = Vec::with_capacity(variants_reader.len() as usize);
        for i in 0..variants_reader.len() {
            variants.push(variants_reader.get(i)?.to_str()?.to_owned());
        }
        Ok(Self {
            node_name: feedback.get_node_name()?.to_str()?.to_owned(),
            node_tag: feedback.get_node_tag()?.to_str()?.to_owned(),
            source_type,
            path: feedback.get_path()?.to_str()?.to_owned(),
            variants,
            excluded: feedback.get_excluded(),
        })
    }
}

/// Result message for the RepoRefresh action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRefreshResult {
    pub success: bool,
    pub error_message: Option<String>,
    pub total_nodes_found: u32,
}

impl RepoRefreshResult {
    pub fn success(total_nodes_found: u32) -> Self {
        Self {
            success: true,
            error_message: None,
            total_nodes_found,
        }
    }

    pub fn failure(message: impl Into<String>) -> Self {
        Self {
            success: false,
            error_message: Some(message.into()),
            total_nodes_found: 0,
        }
    }

    pub fn encode(&self) -> Result<Payload> {
        let mut builder = Builder::new_default();
        {
            let mut result = builder.init_root::<repo_capnp::repo_refresh_result::Builder>();
            result.set_success(self.success);
            if let Some(ref msg) = self.error_message {
                result.set_error_message(msg);
            }
            result.set_total_nodes_found(self.total_nodes_found);
        }
        encode_message(&builder)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let reader = decode_message(data)?;
        let result = reader.get_root::<repo_capnp::repo_refresh_result::Reader>()?;
        Ok(Self {
            success: result.get_success(),
            error_message: optional_text(result.get_error_message()?.to_str()?),
            total_nodes_found: result.get_total_nodes_found(),
        })
    }
}
