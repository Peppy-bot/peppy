//! Integration tests for the hand-written `poll_node_stop` transport shim:
//! its discovery must be scoped to `scope_core_node` (the core node hosting
//! the instance to stop), not to the caller's `bound_core_node` identity.

use std::time::{Duration, Instant};

use core_node_api::ServiceId;
use core_node_api::encoding::{NodeStopRequest, NodeStopResponse};
use peppylib::core_node::transport::poll_node_stop;
use peppylib::messaging::{MessengerHandle, SenderTarget, ServiceMessenger, ServiceTarget};
use pmi::ZenohAdapter;

const BOUND_CORE: &str = "local-core";
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
/// to the core node hosting the decoy. With the scope derived from
/// `bound_core_node` (the old behavior) the decoy would answer instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_stop_discovery_scopes_to_scope_core_node_not_bound() {
    let router = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("start zenoh router");

    let decoy_server = MessengerHandle::connect(&router.host, router.port)
        .await
        .expect("decoy server handle");
    let remote_server = MessengerHandle::connect(&router.host, router.port)
        .await
        .expect("remote server handle");
    let client = MessengerHandle::connect(&router.host, router.port)
        .await
        .expect("client handle");

    spawn_node_stop_stub_listener(
        decoy_server,
        BOUND_CORE,
        "local_daemon",
        NodeStopResponse::failure("listener on the bound core node answered"),
    )
    .await;
    spawn_node_stop_stub_listener(
        remote_server,
        SCOPE_CORE,
        "remote_daemon",
        NodeStopResponse::success(),
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

    assert!(
        response.success,
        "expected the scope core node's listener to answer, got: {:?}",
        response.error_message,
    );
}

/// A listener exists only on the caller's bound core node. Scoping the stop
/// to a core node with no listener must fail unreachable rather than fall
/// back to the bound core node.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_stop_does_not_fall_back_to_bound_core_node() {
    let router = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("start zenoh router");

    let server = MessengerHandle::connect(&router.host, router.port)
        .await
        .expect("server handle");
    let client = MessengerHandle::connect(&router.host, router.port)
        .await
        .expect("client handle");

    spawn_node_stop_stub_listener(
        server,
        BOUND_CORE,
        "local_daemon",
        NodeStopResponse::success(),
    )
    .await;

    // Deterministically wait until the bound-core listener is discoverable so
    // the failure below can only mean "wrong scope", never "not yet routed".
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if ServiceMessenger::is_reachable(
            &client,
            BOUND_CORE,
            CLIENT_INSTANCE,
            stop_target(),
            ServiceId::NodeStop.name(),
            ServiceTarget::CoreNode(BOUND_CORE),
        )
        .await
        .expect("reachability check should succeed")
        {
            break;
        }
        if Instant::now() >= deadline {
            panic!("node_stop stub did not become reachable within 5s");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

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
