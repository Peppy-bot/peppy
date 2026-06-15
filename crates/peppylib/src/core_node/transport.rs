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
use crate::messaging::{ActionGoalHandle, SenderTarget, ServiceTarget};
use crate::{ActionMessenger, MessengerHandle, ServiceMessenger};

/// Routing parameters for a single service poll. Bundled into a struct so
/// [`poll_core_node_service`] doesn't need a `clippy::too_many_arguments`
/// escape hatch — the helper otherwise reaches 9 positional args.
///
/// Control-plane calls always discover (no producer pin at the messenger):
/// the daemon's services listen under a random per-boot instance_id no
/// caller can know up front, and the daemon stays addressed through
/// `to_target` (its identity rides in the service root), so discovery
/// resolves exactly that daemon's endpoint. The exception is `node_stop`,
/// whose listener may be hosted by a per-instance node: user node names
/// are not unique across daemons, so its service root cannot pin the
/// route by itself and the discovery is additionally scoped to the
/// caller's bound core node (`ServiceTarget::CoreNode`).
struct ServiceRoute<'a> {
    messenger: &'a MessengerHandle,
    bound_core_node: &'a str,
    as_instance_id: &'a str,
    /// Target of the service. For daemon-hosted core_node services this is
    /// `SenderTarget::node(to_core_node, CORE_NODE_TAG)`. For per-instance
    /// services (e.g. `node_stop`) it carries the target node's name+tag.
    to_target: SenderTarget,
    service_name: &'a str,
    /// Producer scope of the discovery. `ServiceTarget::Any` for daemon
    /// services (the service root already pins the route); `node_stop`
    /// narrows it to the bound core node, see the struct doc.
    target: ServiceTarget<'a>,
}

/// Routing parameters for a single goal send. Same rationale as
/// [`ServiceRoute`].
struct GoalRoute<'a> {
    messenger: &'a MessengerHandle,
    as_core_node: &'a str,
    as_instance_id: &'a str,
    action_name: &'a str,
    /// Core node whose daemon hosts the action; used only to build the
    /// `SenderTarget` service root. `None` falls back to the caller's own
    /// core_node.
    to_core_node: Option<&'a str>,
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
        route.to_target,
        route.service_name,
        route.target,
        request_payload,
        response_timeout,
    )
    .await?;
    decode_response(response.payload_bytes().as_ref()).map_err(Into::into)
}

async fn send_core_node_goal(
    route: GoalRoute<'_>,
    goal_payload: Payload,
    goal_timeout: Duration,
) -> Result<ActionGoalHandle> {
    let to_core = route.to_core_node.unwrap_or(route.as_core_node);
    ActionMessenger::send_goal(
        route.messenger,
        route.as_core_node,
        route.as_instance_id,
        SenderTarget::node(to_core, names::CORE_NODE_TAG)?,
        route.action_name,
        None,
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
            to_core_node: &str,
            response_timeout: impl Into<Option<Duration>> + Send,
        ) -> Result<$resp> {
            poll_core_node_service(
                ServiceRoute {
                    messenger,
                    bound_core_node,
                    as_instance_id,
                    to_target: SenderTarget::node(to_core_node, names::CORE_NODE_TAG)?,
                    service_name: $service,
                    target: ServiceTarget::Any,
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
            to_core_node: Option<&str>,
            goal_timeout: Duration,
        ) -> Result<ActionGoalHandle> {
            send_core_node_goal(
                GoalRoute {
                    messenger,
                    as_core_node,
                    as_instance_id,
                    action_name: $action,
                    to_core_node,
                },
                goal.encode()?,
                goal_timeout,
            )
            .await
        }
    };
}

poll_service!(pub poll_clock, ClockRequest, ClockResponse, names::CLOCK);
poll_service!(pub poll_info, InfoRequest, InfoResponse, names::INFO);
poll_service!(pub poll_stack_list, StackListRequest, StackListResponse, names::STACK_LIST);
poll_service!(pub poll_datastore_store, DatastoreStoreRequest, DatastoreStoreResponse, names::DATASTORE_STORE);
poll_service!(pub poll_datastore_get, DatastoreGetRequest, DatastoreGetResponse, names::DATASTORE_GET);
poll_service!(pub poll_datastore_list, DatastoreListRequest, DatastoreListResponse, names::DATASTORE_LIST);
poll_service!(pub poll_datastore_remove, DatastoreRemoveRequest, DatastoreRemoveResponse, names::DATASTORE_REMOVE);
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
send_goal!(pub send_stack_benchmark, StackBenchmarkGoal, names::STACK_BENCHMARK_ACTION);

/// `node_stop` is the only service whose listener may be hosted by a
/// per-instance node rather than the daemon, so it routes by an explicit
/// `to_target` (name + tag) instead of defaulting to the daemon's core_node
/// identity. Hand-written for that reason.
///
/// User node names are not unique across daemons, so the `to_target`
/// service root alone cannot pin the route: on a multi-daemon network a
/// wildcard discovery could be won by a same-named listener on a foreign
/// core node, which would answer "unknown instance" while the right reply
/// is dropped. The discovery is therefore scoped to `bound_core_node` —
/// the caller stops an instance through the daemon it is bound to, so its
/// bound core node is the correct scope for both daemon-hosted and
/// per-instance listeners.
pub async fn poll_node_stop(
    request: &NodeStopRequest,
    messenger: &MessengerHandle,
    bound_core_node: &str,
    as_instance_id: &str,
    to_target: SenderTarget,
    response_timeout: impl Into<Option<Duration>> + Send,
) -> Result<NodeStopResponse> {
    poll_core_node_service(
        ServiceRoute {
            messenger,
            bound_core_node,
            as_instance_id,
            to_target,
            service_name: names::NODE_STOP,
            target: ServiceTarget::CoreNode(bound_core_node),
        },
        request.encode()?,
        NodeStopResponse::decode,
        response_timeout,
    )
    .await
}
