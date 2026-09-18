//! Transport shims that bridge the capnp wire types in
//! [`core_node_api::encoding`] to the peppylib messenger.
//!
//! `core-node-api` holds the pure wire types (no peppylib dep) and, emitted
//! from each registry entry, the [`ServiceRequest`] / [`ActionGoal`] impl
//! pairing a request/goal codec with its response type and wire id. The
//! generic [`poll`] and [`send_goal`] here are bounded on those impls, so a
//! method added to the registry is callable through this module with no
//! per-method wrapper — and a wrong request/response pairing is
//! unrepresentable rather than merely tested for.
//!
//! The exclusions are declared in the registry itself and get no impl:
//! `SpawnedNode`-hosted services (their queryable doesn't live under the
//! daemon's service root) and `routing: bespoke` entries — of those,
//! `node_stop` keeps the hand-written [`poll_node_stop`] below.

use std::time::Duration;

use config::node::QoSProfile;
use core_node_api::Payload;
use core_node_api::encoding::*;
use core_node_api::names;
use core_node_api::{ActionGoal, ServiceId, ServiceRequest};

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
/// route by itself and the discovery is additionally scoped to the core
/// node hosting the target instance (`ServiceTarget::CoreNode`).
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
    /// narrows it to the target's core node, see the struct doc.
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

/// Polls a daemon-hosted core-node service. The request codec's
/// [`ServiceRequest`] impl (emitted by the registry) supplies the wire name
/// and the response type, so the request argument alone determines the
/// method: `poll(&StackListRequest::new(), …)` returns a `StackListResponse`.
pub async fn poll<R: ServiceRequest>(
    request: &R,
    messenger: &MessengerHandle,
    bound_core_node: &str,
    as_instance_id: &str,
    to_core_node: &str,
    response_timeout: impl Into<Option<Duration>> + Send,
) -> Result<R::Response> {
    poll_core_node_service(
        ServiceRoute {
            messenger,
            bound_core_node,
            as_instance_id,
            to_target: SenderTarget::node(to_core_node, names::CORE_NODE_TAG)?,
            service_name: R::ID.name(),
            target: ServiceTarget::Any,
        },
        request.encode_request()?,
        R::decode_response,
        response_timeout,
    )
    .await
}

/// Sends a goal to a daemon-hosted core-node action. The goal codec's
/// [`ActionGoal`] impl (emitted by the registry) supplies the wire name:
/// `send_goal(&LaunchGoal { … }, …)` starts a `stack_launch`.
pub async fn send_goal<G: ActionGoal>(
    goal: &G,
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
            action_name: G::ID.name(),
            to_core_node,
        },
        goal.encode_goal()?,
        goal_timeout,
    )
    .await
}

/// `node_stop` is the only service whose listener may be hosted by a
/// per-instance node rather than the daemon, so it routes by an explicit
/// `to_target` (name + tag) instead of defaulting to the daemon's core_node
/// identity. Hand-written for that reason.
///
/// User node names are not unique across daemons, so the `to_target`
/// service root alone cannot pin the route: on a multi-daemon network a
/// wildcard discovery could be won by a same-named listener on a foreign
/// core node, which would answer "unknown instance" while the right reply
/// is dropped. The discovery is therefore scoped to `scope_core_node` —
/// the core node hosting the instance to stop — which pins the route for
/// both daemon-hosted and per-instance listeners.
///
/// The two core-node parameters play distinct roles: `bound_core_node` is
/// the caller's identity (the core node it is bound to, riding in the
/// request's sender address), while `scope_core_node` is the discovery
/// scope. They coincide when stopping through the local daemon and differ
/// when targeting a remote one.
pub async fn poll_node_stop(
    request: &NodeStopRequest,
    messenger: &MessengerHandle,
    bound_core_node: &str,
    as_instance_id: &str,
    to_target: SenderTarget,
    scope_core_node: &str,
    response_timeout: impl Into<Option<Duration>> + Send,
) -> Result<NodeStopResponse> {
    poll_core_node_service(
        ServiceRoute {
            messenger,
            bound_core_node,
            as_instance_id,
            to_target,
            service_name: ServiceId::NodeStop.name(),
            target: ServiceTarget::CoreNode(scope_core_node),
        },
        request.encode()?,
        NodeStopResponse::decode,
        response_timeout,
    )
    .await
}

// The goal-result fetch that used to live here is
// `ActionMessenger::request_result_body` (`messaging/actions.rs`): it touches
// no capnp type, so it belongs beside `request_result` and `ResultStatus`
// rather than in this capnp-to-messenger shim, and every peppylib user reaches
// it without importing the core-node transport.
//
// The feedback DRAIN deliberately stays with each caller: a CLI bounds it by
// the user's `--idle-timeout`, while a coordinator relaying a peer's output
// into a launch stream bounds it by that launch's own budget. Those policies
// differ, so sharing them would mean parameterizing away the whole loop.
