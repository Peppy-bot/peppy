//! Transport shims that bridge the capnp wire types in
//! [`core_node_api::encoding`] to the peppylib messenger.
//!
//! `core-node-api` holds the pure wire types (no peppylib dep). This
//! module exposes `poll_*` / `send_*` free functions over those types.

use std::time::Duration;

use config::node::QoSProfile;
use core_node_api::Payload;
use core_node_api::encoding::*;
use core_node_api::names;

use crate::error::Result;
use crate::messaging::ActionGoalHandle;
use crate::{ActionMessenger, MessengerHandle, ServiceMessenger};

async fn poll_core_node_service<Response>(
    request_payload: Payload,
    decode_response: fn(&[u8]) -> core_node_api::Result<Response>,
    messenger: &MessengerHandle,
    bound_core_node: &str,
    as_instance_id: &str,
    service_target_node: &str,
    service_name: &str,
    target_core_node: &str,
    response_timeout: impl Into<Option<Duration>> + Send,
) -> Result<Response> {
    let response = ServiceMessenger::poll(
        messenger,
        bound_core_node,
        as_instance_id,
        service_target_node,
        service_name,
        Some(target_core_node),
        None,
        request_payload,
        response_timeout,
    )
    .await?;
    Ok(decode_response(response.payload().as_ref())?)
}

async fn send_core_node_goal(
    goal_payload: Payload,
    messenger: &MessengerHandle,
    as_core_node: &str,
    as_instance_id: &str,
    action_name: &str,
    target_core_node: Option<&str>,
    target_instance_id: Option<&str>,
    goal_timeout: Duration,
) -> Result<ActionGoalHandle> {
    Ok(ActionMessenger::send_goal(
        messenger,
        as_core_node,
        as_instance_id,
        target_core_node.unwrap_or(as_core_node),
        action_name,
        target_core_node,
        target_instance_id,
        goal_payload,
        QoSProfile::default(),
        goal_timeout,
    )
    .await?)
}

pub async fn poll_info(
    request: &InfoRequest,
    messenger: &MessengerHandle,
    bound_core_node: &str,
    as_instance_id: &str,
    target_core_node: &str,
    response_timeout: impl Into<Option<Duration>> + Send,
) -> Result<InfoResponse> {
    poll_core_node_service(
        request.encode()?,
        InfoResponse::decode,
        messenger,
        bound_core_node,
        as_instance_id,
        target_core_node,
        names::INFO,
        target_core_node,
        response_timeout,
    )
    .await
}

pub async fn poll_stack_list(
    request: &StackListRequest,
    messenger: &MessengerHandle,
    bound_core_node: &str,
    as_instance_id: &str,
    target_core_node: &str,
    response_timeout: impl Into<Option<Duration>> + Send,
) -> Result<StackListResponse> {
    poll_core_node_service(
        request.encode()?,
        StackListResponse::decode,
        messenger,
        bound_core_node,
        as_instance_id,
        target_core_node,
        names::STACK_LIST,
        target_core_node,
        response_timeout,
    )
    .await
}

pub async fn poll_node_reset(
    request: &NodeResetRequest,
    messenger: &MessengerHandle,
    bound_core_node: &str,
    as_instance_id: &str,
    target_core_node: &str,
    response_timeout: impl Into<Option<Duration>> + Send,
) -> Result<NodeResetResponse> {
    poll_core_node_service(
        request.encode()?,
        NodeResetResponse::decode,
        messenger,
        bound_core_node,
        as_instance_id,
        target_core_node,
        names::STACK_RESET,
        target_core_node,
        response_timeout,
    )
    .await
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
    send_core_node_goal(
        goal.encode()?,
        messenger,
        as_core_node,
        as_instance_id,
        names::STACK_LAUNCH_ACTION,
        target_core_node,
        target_instance_id,
        goal_timeout,
    )
    .await
}

pub async fn poll_node_init(
    request: &NodeInitRequest,
    messenger: &MessengerHandle,
    bound_core_node: &str,
    as_instance_id: &str,
    target_core_node: &str,
    response_timeout: impl Into<Option<Duration>> + Send,
) -> Result<NodeInitResponse> {
    poll_core_node_service(
        request.encode()?,
        NodeInitResponse::decode,
        messenger,
        bound_core_node,
        as_instance_id,
        target_core_node,
        names::NODE_INIT,
        target_core_node,
        response_timeout,
    )
    .await
}

pub async fn poll_node_remove(
    request: &NodeRemoveRequest,
    messenger: &MessengerHandle,
    bound_core_node: &str,
    as_instance_id: &str,
    target_core_node: &str,
    response_timeout: impl Into<Option<Duration>> + Send,
) -> Result<NodeRemoveResponse> {
    poll_core_node_service(
        request.encode()?,
        NodeRemoveResponse::decode,
        messenger,
        bound_core_node,
        as_instance_id,
        target_core_node,
        names::NODE_REMOVE,
        target_core_node,
        response_timeout,
    )
    .await
}

pub async fn poll_node_stop(
    request: &NodeStopRequest,
    messenger: &MessengerHandle,
    bound_core_node: &str,
    as_instance_id: &str,
    target_node_name: &str,
    target_core_node: &str,
    response_timeout: impl Into<Option<Duration>> + Send,
) -> Result<NodeStopResponse> {
    poll_core_node_service(
        request.encode()?,
        NodeStopResponse::decode,
        messenger,
        bound_core_node,
        as_instance_id,
        target_node_name,
        names::NODE_STOP,
        target_core_node,
        response_timeout,
    )
    .await
}

pub async fn poll_node_sync(
    request: &NodeSyncRequest,
    messenger: &MessengerHandle,
    bound_core_node: &str,
    as_instance_id: &str,
    target_core_node: &str,
    response_timeout: impl Into<Option<Duration>> + Send,
) -> Result<NodeSyncResponse> {
    poll_core_node_service(
        request.encode()?,
        NodeSyncResponse::decode,
        messenger,
        bound_core_node,
        as_instance_id,
        target_core_node,
        names::NODE_SYNC,
        target_core_node,
        response_timeout,
    )
    .await
}

pub async fn poll_node_info(
    request: &NodeInfoRequest,
    messenger: &MessengerHandle,
    bound_core_node: &str,
    as_instance_id: &str,
    target_core_node: &str,
    response_timeout: impl Into<Option<Duration>> + Send,
) -> Result<NodeInfoResponse> {
    poll_core_node_service(
        request.encode()?,
        NodeInfoResponse::decode,
        messenger,
        bound_core_node,
        as_instance_id,
        target_core_node,
        names::NODE_INFO,
        target_core_node,
        response_timeout,
    )
    .await
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
    send_core_node_goal(
        goal.encode()?,
        messenger,
        as_core_node,
        as_instance_id,
        names::NODE_ADD_ACTION,
        target_core_node,
        target_instance_id,
        goal_timeout,
    )
    .await
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
    send_core_node_goal(
        goal.encode()?,
        messenger,
        as_core_node,
        as_instance_id,
        names::NODE_RUN_ACTION,
        target_core_node,
        target_instance_id,
        goal_timeout,
    )
    .await
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
    send_core_node_goal(
        goal.encode()?,
        messenger,
        as_core_node,
        as_instance_id,
        names::NODE_BUILD_ACTION,
        target_core_node,
        target_instance_id,
        goal_timeout,
    )
    .await
}

pub async fn poll_repo_list(
    request: &RepoListRequest,
    messenger: &MessengerHandle,
    bound_core_node: &str,
    as_instance_id: &str,
    target_core_node: &str,
    response_timeout: impl Into<Option<Duration>> + Send,
) -> Result<RepoListResponse> {
    poll_core_node_service(
        request.encode()?,
        RepoListResponse::decode,
        messenger,
        bound_core_node,
        as_instance_id,
        target_core_node,
        names::REPO_LIST,
        target_core_node,
        response_timeout,
    )
    .await
}

pub async fn poll_repo_add(
    request: &RepoAddRequest,
    messenger: &MessengerHandle,
    bound_core_node: &str,
    as_instance_id: &str,
    target_core_node: &str,
    response_timeout: impl Into<Option<Duration>> + Send,
) -> Result<RepoAddResponse> {
    poll_core_node_service(
        request.encode()?,
        RepoAddResponse::decode,
        messenger,
        bound_core_node,
        as_instance_id,
        target_core_node,
        names::REPO_ADD,
        target_core_node,
        response_timeout,
    )
    .await
}

pub async fn poll_repo_exclude(
    request: &RepoExcludeRequest,
    messenger: &MessengerHandle,
    bound_core_node: &str,
    as_instance_id: &str,
    target_core_node: &str,
    response_timeout: impl Into<Option<Duration>> + Send,
) -> Result<RepoExcludeResponse> {
    poll_core_node_service(
        request.encode()?,
        RepoExcludeResponse::decode,
        messenger,
        bound_core_node,
        as_instance_id,
        target_core_node,
        names::REPO_EXCLUDE,
        target_core_node,
        response_timeout,
    )
    .await
}

pub async fn poll_repo_remove(
    request: &RepoRemoveRequest,
    messenger: &MessengerHandle,
    bound_core_node: &str,
    as_instance_id: &str,
    target_core_node: &str,
    response_timeout: impl Into<Option<Duration>> + Send,
) -> Result<RepoRemoveResponse> {
    poll_core_node_service(
        request.encode()?,
        RepoRemoveResponse::decode,
        messenger,
        bound_core_node,
        as_instance_id,
        target_core_node,
        names::REPO_REMOVE,
        target_core_node,
        response_timeout,
    )
    .await
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
    send_core_node_goal(
        goal.encode()?,
        messenger,
        as_core_node,
        as_instance_id,
        names::REPO_REFRESH_ACTION,
        target_core_node,
        target_instance_id,
        goal_timeout,
    )
    .await
}
