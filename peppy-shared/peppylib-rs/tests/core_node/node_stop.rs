//! Integration tests for the hand-written `poll_node_stop` transport shim:
//! its discovery must be scoped to `scope_core_node` (the core node hosting
//! the instance to stop), not to the caller's `bound_core_node` identity.

use std::time::Duration;

use core_node_api::ServiceId;
use core_node_api::encoding::{NodeStopRequest, NodeStopResponse};
use peppylib::core_node::transport::poll_node_stop;
use peppylib::messaging::{MessengerHandle, ProducerRef, SenderTarget, ServiceMessenger};
use peppylib::testing::{EphemeralRouter, wait_service_reachable};

const BOUND_CORE: &str = "local-core";
/// The instance hosting the `node_stop` listener on the bound core node.
const BOUND_DAEMON_INSTANCE: &str = "local_daemon";
const SCOPE_CORE: &str = "remote-core";
const CLIENT_INSTANCE: &str = "test_caller";
const NODE_NAME: &str = "worker_node";
const NODE_TAG: &str = "v1";

/// The per-instance node target a `node_stop` caller addresses: the user
/// node's name + manifest tag, identical on both core nodes so only the
/// discovery scope can disambiguate them.
fn stop_target() -> SenderTarget {
    SenderTarget::node(NODE_NAME, NODE_TAG).expect("test node target")
}

/// Spins up a single-shot `node_stop` listener hosted by `host_core_node`
/// that returns `response` verbatim. The handler decodes the inbound
/// `NodeStopRequest` to assert wire shape, even though it ignores the value.
async fn spawn_node_stop_stub_listener(
    server: MessengerHandle,
    host_core_node: &'static str,
    instance_id: &'static str,
    response: NodeStopResponse,
) {
    let mut endpoint = ServiceMessenger::listen(
        &server,
        host_core_node,
        instance_id,
        stop_target(),
        ServiceId::NodeStop.name(),
    )
    .await
    .expect("listen should succeed");

    tokio::spawn(async move {
        endpoint
            .handle_next_request(|request| async move {
                let payload = request.message().payload();
                let _inbound =
                    NodeStopRequest::decode(payload.as_ref()).expect("decode NodeStopRequest");
                Ok(response.encode().expect("encode NodeStopResponse"))
            })
            .await
            .expect("handle_next_request should succeed");
    });
}

/// Two same-named `node_stop` listeners on different core nodes: the one on
/// `scope_core_node` must win the discovery even though the caller is bound
/// to the core node hosting the decoy. A discovery scoped to
/// `bound_core_node` would let the decoy answer instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_stop_discovery_scopes_to_scope_core_node_not_bound() {
    let router = EphemeralRouter::start().await.expect("start zenoh router");

    let decoy_server = router.connect().await.expect("decoy server handle");
    let remote_server = router.connect().await.expect("remote server handle");
    let client = router.connect().await.expect("client handle");

    spawn_node_stop_stub_listener(
        decoy_server,
        BOUND_CORE,
        BOUND_DAEMON_INSTANCE,
        // The decoy answers a stop that differs from the scope core node's,
        // so the answer names the listener that gave it.
        NodeStopResponse::forced(),
    )
    .await;
    spawn_node_stop_stub_listener(
        remote_server,
        SCOPE_CORE,
        "remote_daemon",
        NodeStopResponse::graceful(),
    )
    .await;

    // No settle sleep: the scoped discovery retries cold-start misses within
    // the response budget, so it waits for the remote listener's queryable to
    // propagate rather than guessing a fixed delay.
    let response = poll_node_stop(
        &NodeStopRequest::new("instance-42"),
        &client,
        BOUND_CORE,
        CLIENT_INSTANCE,
        stop_target(),
        SCOPE_CORE,
        Duration::from_secs(5),
    )
    .await
    .expect("poll_node_stop should reach the scope core node's listener");

    assert_eq!(
        response,
        NodeStopResponse::graceful(),
        "expected the scope core node's listener to answer"
    );
}

/// A listener exists only on the caller's bound core node. Scoping the stop
/// to a core node with no listener must fail unreachable rather than fall
/// back to the bound core node.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_stop_does_not_fall_back_to_bound_core_node() {
    let router = EphemeralRouter::start().await.expect("start zenoh router");

    let server = router.connect().await.expect("server handle");
    let client = router.connect().await.expect("client handle");

    spawn_node_stop_stub_listener(
        server,
        BOUND_CORE,
        BOUND_DAEMON_INSTANCE,
        NodeStopResponse::graceful(),
    )
    .await;

    // Deterministically wait until the bound-core listener is discoverable so
    // the failure below can only mean "wrong scope", never "not yet routed".
    wait_service_reachable(
        &client,
        BOUND_CORE,
        CLIENT_INSTANCE,
        stop_target(),
        ServiceId::NodeStop.name(),
        &ProducerRef::new(BOUND_CORE, BOUND_DAEMON_INSTANCE),
        Duration::from_secs(5),
    )
    .await
    .expect("node_stop stub did not become reachable within 5s");

    poll_node_stop(
        &NodeStopRequest::new("instance-42"),
        &client,
        BOUND_CORE,
        CLIENT_INSTANCE,
        stop_target(),
        SCOPE_CORE,
        Duration::from_secs(1),
    )
    .await
    .expect_err("a scope with no listener must not fall back to the bound core node");
}
