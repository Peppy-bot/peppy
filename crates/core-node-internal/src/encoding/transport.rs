//! Transport shims that bridge the capnp wire types in
//! [`core_node_api::encoding`] to the peppylib messenger.
//!
//! These `poll` / `send_goal` methods used to live on the encoding types
//! before they moved to `core-node-api` (which must not depend on
//! peppylib). Each is re-exposed here as an extension trait so callers
//! that `use core_node::encoding::prelude::*;` keep the same
//! `.poll(…)` / `.send_goal(…)` call shape.

use std::time::Duration;

use peppylib::messaging::ActionGoalHandle;
use peppylib::{ActionMessenger, MessengerHandle, ServiceMessenger};

use config::node::QoSProfile;

use core_node_api::encoding::*;
use core_node_api::names;

use crate::Result;

pub trait InfoRequestPollExt {
    fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        target_core_node: &str,
        response_timeout: Duration,
    ) -> impl std::future::Future<Output = Result<InfoResponse>> + Send;
}

impl InfoRequestPollExt for InfoRequest {
    async fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        target_core_node: &str,
        response_timeout: Duration,
    ) -> Result<InfoResponse> {
        let request_payload = self.encode()?;
        let response = ServiceMessenger::poll(
            messenger,
            bound_core_node,
            as_instance_id,
            target_core_node,
            names::INFO,
            Some(target_core_node),
            None,
            request_payload,
            response_timeout,
        )
        .await?;
        Ok(InfoResponse::decode(response.payload().as_ref())?)
    }
}

pub trait StackListRequestPollExt {
    fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        target_core_node: &str,
        response_timeout: Duration,
    ) -> impl std::future::Future<Output = Result<StackListResponse>> + Send;
}

impl StackListRequestPollExt for StackListRequest {
    async fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        target_core_node: &str,
        response_timeout: Duration,
    ) -> Result<StackListResponse> {
        let request_payload = self.encode()?;
        let response = ServiceMessenger::poll(
            messenger,
            bound_core_node,
            as_instance_id,
            target_core_node,
            names::STACK_LIST,
            Some(target_core_node),
            None,
            request_payload,
            response_timeout,
        )
        .await?;
        Ok(StackListResponse::decode(response.payload().as_ref())?)
    }
}

pub trait NodeResetRequestPollExt {
    fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        target_core_node: &str,
        response_timeout: Duration,
    ) -> impl std::future::Future<Output = Result<NodeResetResponse>> + Send;
}

impl NodeResetRequestPollExt for NodeResetRequest {
    async fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        target_core_node: &str,
        response_timeout: Duration,
    ) -> Result<NodeResetResponse> {
        let request_payload = self.encode()?;
        let response = ServiceMessenger::poll(
            messenger,
            bound_core_node,
            as_instance_id,
            target_core_node,
            names::STACK_RESET,
            Some(target_core_node),
            None,
            request_payload,
            response_timeout,
        )
        .await?;
        Ok(NodeResetResponse::decode(response.payload().as_ref())?)
    }
}

pub trait LaunchGoalSendGoalExt {
    fn send_goal(
        &self,
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        target_core_node: Option<&str>,
        target_instance_id: Option<&str>,
        goal_timeout: Duration,
    ) -> impl std::future::Future<Output = Result<ActionGoalHandle>> + Send;
}

impl LaunchGoalSendGoalExt for LaunchGoal {
    async fn send_goal(
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

pub trait NodeInitRequestPollExt {
    fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        target_core_node: &str,
        response_timeout: impl Into<Option<Duration>> + Send,
    ) -> impl std::future::Future<Output = Result<NodeInitResponse>> + Send;
}

impl NodeInitRequestPollExt for NodeInitRequest {
    async fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        target_core_node: &str,
        response_timeout: impl Into<Option<Duration>> + Send,
    ) -> Result<NodeInitResponse> {
        let request_payload = self.encode()?;
        let response = ServiceMessenger::poll(
            messenger,
            bound_core_node,
            as_instance_id,
            target_core_node,
            names::NODE_INIT,
            Some(target_core_node),
            None,
            request_payload,
            response_timeout,
        )
        .await?;
        Ok(NodeInitResponse::decode(response.payload().as_ref())?)
    }
}

pub trait NodeRemoveRequestPollExt {
    fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        target_core_node: &str,
        response_timeout: Duration,
    ) -> impl std::future::Future<Output = Result<NodeRemoveResponse>> + Send;
}

impl NodeRemoveRequestPollExt for NodeRemoveRequest {
    async fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        target_core_node: &str,
        response_timeout: Duration,
    ) -> Result<NodeRemoveResponse> {
        let request_payload = self.encode()?;
        let response = ServiceMessenger::poll(
            messenger,
            bound_core_node,
            as_instance_id,
            target_core_node,
            names::NODE_REMOVE,
            Some(target_core_node),
            None,
            request_payload,
            response_timeout,
        )
        .await?;
        Ok(NodeRemoveResponse::decode(response.payload().as_ref())?)
    }
}

pub trait NodeStopRequestPollExt {
    fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        target_node_name: &str,
        target_core_node: &str,
        response_timeout: Duration,
    ) -> impl std::future::Future<Output = Result<NodeStopResponse>> + Send;
}

impl NodeStopRequestPollExt for NodeStopRequest {
    async fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        target_node_name: &str,
        target_core_node: &str,
        response_timeout: Duration,
    ) -> Result<NodeStopResponse> {
        let request_payload = self.encode()?;
        let response = ServiceMessenger::poll(
            messenger,
            bound_core_node,
            as_instance_id,
            target_node_name,
            names::NODE_STOP,
            Some(target_core_node),
            None,
            request_payload,
            response_timeout,
        )
        .await?;
        Ok(NodeStopResponse::decode(response.payload().as_ref())?)
    }
}

pub trait NodeSyncRequestPollExt {
    fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        target_core_node: &str,
        response_timeout: Duration,
    ) -> impl std::future::Future<Output = Result<NodeSyncResponse>> + Send;
}

impl NodeSyncRequestPollExt for NodeSyncRequest {
    async fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        target_core_node: &str,
        response_timeout: Duration,
    ) -> Result<NodeSyncResponse> {
        let request_payload = self.encode()?;
        let response = ServiceMessenger::poll(
            messenger,
            bound_core_node,
            as_instance_id,
            target_core_node,
            names::NODE_SYNC,
            Some(target_core_node),
            None,
            request_payload,
            response_timeout,
        )
        .await?;
        Ok(NodeSyncResponse::decode(response.payload().as_ref())?)
    }
}

pub trait NodeInfoRequestPollExt {
    fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        target_core_node: &str,
        response_timeout: Duration,
    ) -> impl std::future::Future<Output = Result<NodeInfoResponse>> + Send;
}

impl NodeInfoRequestPollExt for NodeInfoRequest {
    async fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        target_core_node: &str,
        response_timeout: Duration,
    ) -> Result<NodeInfoResponse> {
        let request_payload = self.encode()?;
        let response = ServiceMessenger::poll(
            messenger,
            bound_core_node,
            as_instance_id,
            target_core_node,
            names::NODE_INFO,
            Some(target_core_node),
            None,
            request_payload,
            response_timeout,
        )
        .await?;
        Ok(NodeInfoResponse::decode(response.payload().as_ref())?)
    }
}

pub trait NodeAddGoalSendGoalExt {
    fn send_goal(
        &self,
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        target_core_node: Option<&str>,
        target_instance_id: Option<&str>,
        goal_timeout: Duration,
    ) -> impl std::future::Future<Output = Result<ActionGoalHandle>> + Send;
}

impl NodeAddGoalSendGoalExt for NodeAddGoal {
    async fn send_goal(
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

pub trait NodeRunGoalSendGoalExt {
    fn send_goal(
        &self,
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        target_core_node: Option<&str>,
        target_instance_id: Option<&str>,
        goal_timeout: Duration,
    ) -> impl std::future::Future<Output = Result<ActionGoalHandle>> + Send;
}

impl NodeRunGoalSendGoalExt for NodeRunGoal {
    async fn send_goal(
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
            names::NODE_RUN_ACTION,
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

pub trait NodeBuildGoalSendGoalExt {
    fn send_goal(
        &self,
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        target_core_node: Option<&str>,
        target_instance_id: Option<&str>,
        goal_timeout: Duration,
    ) -> impl std::future::Future<Output = Result<ActionGoalHandle>> + Send;
}

impl NodeBuildGoalSendGoalExt for NodeBuildGoal {
    async fn send_goal(
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

pub trait RepoListRequestPollExt {
    fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        target_core_node: &str,
        response_timeout: Duration,
    ) -> impl std::future::Future<Output = Result<RepoListResponse>> + Send;
}

impl RepoListRequestPollExt for RepoListRequest {
    async fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        target_core_node: &str,
        response_timeout: Duration,
    ) -> Result<RepoListResponse> {
        let request_payload = self.encode()?;
        let response = ServiceMessenger::poll(
            messenger,
            bound_core_node,
            as_instance_id,
            target_core_node,
            names::REPO_LIST,
            Some(target_core_node),
            None,
            request_payload,
            response_timeout,
        )
        .await?;
        Ok(RepoListResponse::decode(response.payload().as_ref())?)
    }
}

pub trait RepoAddRequestPollExt {
    fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        target_core_node: &str,
        response_timeout: Duration,
    ) -> impl std::future::Future<Output = Result<RepoAddResponse>> + Send;
}

impl RepoAddRequestPollExt for RepoAddRequest {
    async fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        target_core_node: &str,
        response_timeout: Duration,
    ) -> Result<RepoAddResponse> {
        let request_payload = self.encode()?;
        let response = ServiceMessenger::poll(
            messenger,
            bound_core_node,
            as_instance_id,
            target_core_node,
            names::REPO_ADD,
            Some(target_core_node),
            None,
            request_payload,
            response_timeout,
        )
        .await?;
        Ok(RepoAddResponse::decode(response.payload().as_ref())?)
    }
}

pub trait RepoExcludeRequestPollExt {
    fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        target_core_node: &str,
        response_timeout: Duration,
    ) -> impl std::future::Future<Output = Result<RepoExcludeResponse>> + Send;
}

impl RepoExcludeRequestPollExt for RepoExcludeRequest {
    async fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        target_core_node: &str,
        response_timeout: Duration,
    ) -> Result<RepoExcludeResponse> {
        let request_payload = self.encode()?;
        let response = ServiceMessenger::poll(
            messenger,
            bound_core_node,
            as_instance_id,
            target_core_node,
            names::REPO_EXCLUDE,
            Some(target_core_node),
            None,
            request_payload,
            response_timeout,
        )
        .await?;
        Ok(RepoExcludeResponse::decode(response.payload().as_ref())?)
    }
}

pub trait RepoRemoveRequestPollExt {
    fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        target_core_node: &str,
        response_timeout: Duration,
    ) -> impl std::future::Future<Output = Result<RepoRemoveResponse>> + Send;
}

impl RepoRemoveRequestPollExt for RepoRemoveRequest {
    async fn poll(
        &self,
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        target_core_node: &str,
        response_timeout: Duration,
    ) -> Result<RepoRemoveResponse> {
        let request_payload = self.encode()?;
        let response = ServiceMessenger::poll(
            messenger,
            bound_core_node,
            as_instance_id,
            target_core_node,
            names::REPO_REMOVE,
            Some(target_core_node),
            None,
            request_payload,
            response_timeout,
        )
        .await?;
        Ok(RepoRemoveResponse::decode(response.payload().as_ref())?)
    }
}

pub trait RepoRefreshGoalSendGoalExt {
    fn send_goal(
        &self,
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        target_core_node: Option<&str>,
        target_instance_id: Option<&str>,
        goal_timeout: Duration,
    ) -> impl std::future::Future<Output = Result<ActionGoalHandle>> + Send;
}

impl RepoRefreshGoalSendGoalExt for RepoRefreshGoal {
    async fn send_goal(
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

/// Glob-import to bring every transport extension trait into scope.
pub mod prelude {
    pub use super::InfoRequestPollExt;
    pub use super::LaunchGoalSendGoalExt;
    pub use super::NodeAddGoalSendGoalExt;
    pub use super::NodeBuildGoalSendGoalExt;
    pub use super::NodeInfoRequestPollExt;
    pub use super::NodeInitRequestPollExt;
    pub use super::NodeRemoveRequestPollExt;
    pub use super::NodeResetRequestPollExt;
    pub use super::NodeRunGoalSendGoalExt;
    pub use super::NodeStopRequestPollExt;
    pub use super::NodeSyncRequestPollExt;
    pub use super::RepoAddRequestPollExt;
    pub use super::RepoExcludeRequestPollExt;
    pub use super::RepoListRequestPollExt;
    pub use super::RepoRefreshGoalSendGoalExt;
    pub use super::RepoRemoveRequestPollExt;
    pub use super::StackListRequestPollExt;
}
