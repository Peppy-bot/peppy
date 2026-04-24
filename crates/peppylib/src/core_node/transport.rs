//! Transport shims that bridge the capnp wire types in
//! [`core_node_api::encoding`] to the peppylib messenger.
//!
//! `core-node-api` holds the pure wire types (no peppylib dep). This
//! module exposes `poll_*` / `send_*` free functions over those types.
//!
//! Each `poll_*` / `send_*` is a one-line macro invocation — the actual
//! routing lives in [`poll_core_node_service`] and [`send_core_node_goal`].
//! Add a new service by appending one `poll_service!` / `send_goal!` line
//! at the bottom of this file.

use std::time::Duration;

use config::node::QoSProfile;
use core_node_api::Payload;
use core_node_api::encoding::*;
use core_node_api::names;

use crate::error::Result;
use crate::messaging::ActionGoalHandle;
use crate::{ActionMessenger, MessengerHandle, ServiceMessenger};

/// Routing parameters for a single service poll. Bundled into a struct so
/// [`poll_core_node_service`] doesn't need a `clippy::too_many_arguments`
/// escape hatch — the helper otherwise reaches 9 positional args.
struct ServiceRoute<'a> {
    messenger: &'a MessengerHandle,
    bound_core_node: &'a str,
    as_instance_id: &'a str,
    target_core_node: &'a str,
    /// `None` routes to the daemon (the common case); `Some(node_name)` routes
    /// to a non-core-node service host (e.g. the per-instance `node_stop`
    /// listener).
    service_target_node: Option<&'a str>,
    service_name: &'a str,
}

/// Routing parameters for a single goal send. Same rationale as
/// [`ServiceRoute`].
struct GoalRoute<'a> {
    messenger: &'a MessengerHandle,
    as_core_node: &'a str,
    as_instance_id: &'a str,
    action_name: &'a str,
    target_core_node: Option<&'a str>,
    target_instance_id: Option<&'a str>,
}

async fn poll_core_node_service<Response>(
    route: ServiceRoute<'_>,
    request_payload: Payload,
    decode_response: fn(&[u8]) -> core_node_api::Result<Response>,
    response_timeout: impl Into<Option<Duration>> + Send,
) -> Result<Response> {
    let response = ServiceMessenger::poll(
        route.messenger,
        route.bound_core_node,
        route.as_instance_id,
        route.service_target_node.unwrap_or(route.target_core_node),
        route.service_name,
        Some(route.target_core_node),
        None,
        request_payload,
        response_timeout,
    )
    .await?;
    decode_response(response.payload().as_ref()).map_err(Into::into)
}

async fn send_core_node_goal(
    route: GoalRoute<'_>,
    goal_payload: Payload,
    goal_timeout: Duration,
) -> Result<ActionGoalHandle> {
    ActionMessenger::send_goal(
        route.messenger,
        route.as_core_node,
        route.as_instance_id,
        route.target_core_node.unwrap_or(route.as_core_node),
        route.action_name,
        route.target_core_node,
        route.target_instance_id,
        goal_payload,
        QoSProfile::default(),
        goal_timeout,
    )
    .await
}

/// Defines a `poll_*` wrapper that encodes `$req`, polls the service named
/// `$service` on the target core node, and decodes the response into `$resp`.
macro_rules! poll_service {
    ($vis:vis $name:ident, $req:ty, $resp:ty, $service:expr) => {
        $vis async fn $name(
            request: &$req,
            messenger: &MessengerHandle,
            bound_core_node: &str,
            as_instance_id: &str,
            target_core_node: &str,
            response_timeout: impl Into<Option<Duration>> + Send,
        ) -> Result<$resp> {
            poll_core_node_service(
                ServiceRoute {
                    messenger,
                    bound_core_node,
                    as_instance_id,
                    target_core_node,
                    service_target_node: None,
                    service_name: $service,
                },
                request.encode()?,
                <$resp>::decode,
                response_timeout,
            )
            .await
        }
    };
}

/// Defines a `send_*` wrapper that encodes `$goal` and sends it as the action
/// named `$action` to the target core node.
macro_rules! send_goal {
    ($vis:vis $name:ident, $goal:ty, $action:expr) => {
        $vis async fn $name(
            goal: &$goal,
            messenger: &MessengerHandle,
            as_core_node: &str,
            as_instance_id: &str,
            target_core_node: Option<&str>,
            target_instance_id: Option<&str>,
            goal_timeout: Duration,
        ) -> Result<ActionGoalHandle> {
            send_core_node_goal(
                GoalRoute {
                    messenger,
                    as_core_node,
                    as_instance_id,
                    action_name: $action,
                    target_core_node,
                    target_instance_id,
                },
                goal.encode()?,
                goal_timeout,
            )
            .await
        }
    };
}

poll_service!(pub poll_info, InfoRequest, InfoResponse, names::INFO);
poll_service!(pub poll_stack_list, StackListRequest, StackListResponse, names::STACK_LIST);
poll_service!(pub poll_node_reset, NodeResetRequest, NodeResetResponse, names::STACK_RESET);
poll_service!(pub poll_node_init, NodeInitRequest, NodeInitResponse, names::NODE_INIT);
poll_service!(pub poll_node_remove, NodeRemoveRequest, NodeRemoveResponse, names::NODE_REMOVE);
poll_service!(pub poll_node_sync, NodeSyncRequest, NodeSyncResponse, names::NODE_SYNC);
poll_service!(pub poll_node_info, NodeInfoRequest, NodeInfoResponse, names::NODE_INFO);
poll_service!(pub poll_repo_list, RepoListRequest, RepoListResponse, names::REPO_LIST);
poll_service!(pub poll_repo_add, RepoAddRequest, RepoAddResponse, names::REPO_ADD);
poll_service!(pub poll_repo_exclude, RepoExcludeRequest, RepoExcludeResponse, names::REPO_EXCLUDE);
poll_service!(pub poll_repo_remove, RepoRemoveRequest, RepoRemoveResponse, names::REPO_REMOVE);

send_goal!(pub send_launch, LaunchGoal, names::STACK_LAUNCH_ACTION);
send_goal!(pub send_node_add, NodeAddGoal, names::NODE_ADD_ACTION);
send_goal!(pub send_node_run, NodeRunGoal, names::NODE_RUN_ACTION);
send_goal!(pub send_node_build, NodeBuildGoal, names::NODE_BUILD_ACTION);
send_goal!(pub send_repo_refresh, RepoRefreshGoal, names::REPO_REFRESH_ACTION);

/// `node_stop` is the only service whose listener is hosted by the per-instance
/// node rather than the daemon, so it routes by `target_node_name` instead of
/// the daemon's core node name. Hand-written for that reason.
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
        ServiceRoute {
            messenger,
            bound_core_node,
            as_instance_id,
            target_core_node,
            service_target_node: Some(target_node_name),
            service_name: names::NODE_STOP,
        },
        request.encode()?,
        NodeStopResponse::decode,
        response_timeout,
    )
    .await
}
