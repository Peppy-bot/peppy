//! Transport shims that bridge the capnp wire types in
//! [`core_node_api::encoding`] to the peppylib messenger.
//!
//! `core-node-api` holds the pure wire types (no peppylib dep). This
//! module exposes `poll_*` / `send_*` free functions over those types.

use std::time::Duration;

use config::node::QoSProfile;
use core_node_api::encoding::*;
use core_node_api::names;

use crate::error::Result;
use crate::messaging::ActionGoalHandle;
use crate::{ActionMessenger, MessengerHandle, ServiceMessenger};

pub async fn poll_info(
    request: &InfoRequest,
    messenger: &MessengerHandle,
    bound_core_node: &str,
    as_instance_id: &str,
    target_core_node: &str,
    response_timeout: Duration,
) -> Result<InfoResponse> {
    let request_payload = request.encode()?;
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

pub async fn poll_stack_list(
    request: &StackListRequest,
    messenger: &MessengerHandle,
    bound_core_node: &str,
    as_instance_id: &str,
    target_core_node: &str,
    response_timeout: Duration,
) -> Result<StackListResponse> {
    let request_payload = request.encode()?;
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

pub async fn poll_node_reset(
    request: &NodeResetRequest,
    messenger: &MessengerHandle,
    bound_core_node: &str,
    as_instance_id: &str,
    target_core_node: &str,
    response_timeout: Duration,
) -> Result<NodeResetResponse> {
    let request_payload = request.encode()?;
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

pub async fn send_launch(
    goal: &LaunchGoal,
    messenger: &MessengerHandle,
    as_core_node: &str,
    as_instance_id: &str,
    target_core_node: Option<&str>,
    target_instance_id: Option<&str>,
    goal_timeout: Duration,
) -> Result<ActionGoalHandle> {
    let goal_payload = goal.encode()?;
    let handle = ActionMessenger::send_goal(
        messenger,
        as_core_node,
        as_instance_id,
        target_core_node.unwrap_or(as_core_node),
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

pub async fn poll_node_init(
    request: &NodeInitRequest,
    messenger: &MessengerHandle,
    bound_core_node: &str,
    as_instance_id: &str,
    target_core_node: &str,
    response_timeout: impl Into<Option<Duration>> + Send,
) -> Result<NodeInitResponse> {
    let request_payload = request.encode()?;
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

pub async fn poll_node_remove(
    request: &NodeRemoveRequest,
    messenger: &MessengerHandle,
    bound_core_node: &str,
    as_instance_id: &str,
    target_core_node: &str,
    response_timeout: Duration,
) -> Result<NodeRemoveResponse> {
    let request_payload = request.encode()?;
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

pub async fn poll_node_stop(
    request: &NodeStopRequest,
    messenger: &MessengerHandle,
    bound_core_node: &str,
    as_instance_id: &str,
    target_node_name: &str,
    target_core_node: &str,
    response_timeout: Duration,
) -> Result<NodeStopResponse> {
    let request_payload = request.encode()?;
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

pub async fn poll_node_sync(
    request: &NodeSyncRequest,
    messenger: &MessengerHandle,
    bound_core_node: &str,
    as_instance_id: &str,
    target_core_node: &str,
    response_timeout: Duration,
) -> Result<NodeSyncResponse> {
    let request_payload = request.encode()?;
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

pub async fn poll_node_info(
    request: &NodeInfoRequest,
    messenger: &MessengerHandle,
    bound_core_node: &str,
    as_instance_id: &str,
    target_core_node: &str,
    response_timeout: Duration,
) -> Result<NodeInfoResponse> {
    let request_payload = request.encode()?;
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

pub async fn send_node_add(
    goal: &NodeAddGoal,
    messenger: &MessengerHandle,
    as_core_node: &str,
    as_instance_id: &str,
    target_core_node: Option<&str>,
    target_instance_id: Option<&str>,
    goal_timeout: Duration,
) -> Result<ActionGoalHandle> {
    let goal_payload = goal.encode()?;
    let handle = ActionMessenger::send_goal(
        messenger,
        as_core_node,
        as_instance_id,
        target_core_node.unwrap_or(as_core_node),
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

pub async fn send_node_run(
    goal: &NodeRunGoal,
    messenger: &MessengerHandle,
    as_core_node: &str,
    as_instance_id: &str,
    target_core_node: Option<&str>,
    target_instance_id: Option<&str>,
    goal_timeout: Duration,
) -> Result<ActionGoalHandle> {
    let goal_payload = goal.encode()?;
    let handle = ActionMessenger::send_goal(
        messenger,
        as_core_node,
        as_instance_id,
        target_core_node.unwrap_or(as_core_node),
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

pub async fn send_node_build(
    goal: &NodeBuildGoal,
    messenger: &MessengerHandle,
    as_core_node: &str,
    as_instance_id: &str,
    target_core_node: Option<&str>,
    target_instance_id: Option<&str>,
    goal_timeout: Duration,
) -> Result<ActionGoalHandle> {
    let goal_payload = goal.encode()?;
    let handle = ActionMessenger::send_goal(
        messenger,
        as_core_node,
        as_instance_id,
        target_core_node.unwrap_or(as_core_node),
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

pub async fn poll_repo_list(
    request: &RepoListRequest,
    messenger: &MessengerHandle,
    bound_core_node: &str,
    as_instance_id: &str,
    target_core_node: &str,
    response_timeout: Duration,
) -> Result<RepoListResponse> {
    let request_payload = request.encode()?;
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

pub async fn poll_repo_add(
    request: &RepoAddRequest,
    messenger: &MessengerHandle,
    bound_core_node: &str,
    as_instance_id: &str,
    target_core_node: &str,
    response_timeout: Duration,
) -> Result<RepoAddResponse> {
    let request_payload = request.encode()?;
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

pub async fn poll_repo_exclude(
    request: &RepoExcludeRequest,
    messenger: &MessengerHandle,
    bound_core_node: &str,
    as_instance_id: &str,
    target_core_node: &str,
    response_timeout: Duration,
) -> Result<RepoExcludeResponse> {
    let request_payload = request.encode()?;
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

pub async fn poll_repo_remove(
    request: &RepoRemoveRequest,
    messenger: &MessengerHandle,
    bound_core_node: &str,
    as_instance_id: &str,
    target_core_node: &str,
    response_timeout: Duration,
) -> Result<RepoRemoveResponse> {
    let request_payload = request.encode()?;
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

pub async fn send_repo_refresh(
    goal: &RepoRefreshGoal,
    messenger: &MessengerHandle,
    as_core_node: &str,
    as_instance_id: &str,
    target_core_node: Option<&str>,
    target_instance_id: Option<&str>,
    goal_timeout: Duration,
) -> Result<ActionGoalHandle> {
    let goal_payload = goal.encode()?;
    let handle = ActionMessenger::send_goal(
        messenger,
        as_core_node,
        as_instance_id,
        target_core_node.unwrap_or(as_core_node),
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
