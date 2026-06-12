//! End-to-end collision safety tests for the wire refactor. Validates that a
//! publisher emitting as `SenderTarget::Node(name, tag)` is never matched by a
//! subscriber pinned on `SenderTarget::Interface(name, tag)` (or vice-versa),
//! even when both share the same name and tag. The `interface` / `node`
//! discriminator embedded in the wire format is the protocol-level guarantee
//! that makes the two identifier namespaces disjoint.
//!
//! Mirrors the unit-level checks in `wire/zenoh_format/tests.rs` but exercises
//! the full transport stack (zenohd routing + adapter) instead of just the
//! keyexpr string. Gated on `build_zenoh` because each test spawns a zenohd
//! process; serialized via [`common::ZENOH_SERIAL`] to avoid handshake
//! flakiness.

#![cfg(feature = "build_zenoh")]

mod common;
use common::{RECV_TIMEOUT, ZENOH_SERIAL, test_node_target, wait_for_subscriber_discovery};

use bytes::Bytes;
use pmi::{
    MessengerBackend, Payload, PublisherQoS, SenderTarget, ServiceKind, ServiceQueryKind,
    ServiceQueryable, ServiceWireReceiver, ServiceWireSender, SubscriberQoS, Subscription,
    TopicWireReceiver, TopicWireSender, ZenohAdapter,
};
use std::time::Duration;

const NO_MESSAGE_TIMEOUT: Duration = Duration::from_millis(500);

/// Asserts the subscriber receives a payload exactly equal to `expected`.
async fn expect_payload(sub: &mut Subscription, expected: &Bytes, label: &str) {
    let msg = tokio::time::timeout(RECV_TIMEOUT, sub.rx.recv_async())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for message on {label}"))
        .unwrap_or_else(|_| panic!("channel closed before message on {label}"));
    assert_eq!(
        msg.payload(),
        expected,
        "{label}: subscriber received the wrong payload"
    );
}

/// Asserts the subscriber receives no payload within `NO_MESSAGE_TIMEOUT`.
async fn expect_no_payload(sub: &mut Subscription, label: &str) {
    match tokio::time::timeout(NO_MESSAGE_TIMEOUT, sub.rx.recv_async()).await {
        Err(_) => {
            // Timed out — no payload arrived, which is the success case.
        }
        Ok(Ok(msg)) => {
            panic!(
                "{label}: subscriber received an unexpected payload of {} bytes (collision)",
                msg.payload().len()
            );
        }
        Ok(Err(_)) => {
            // Channel closed — also acceptable, no payload arrived.
        }
    }
}

/// Two publishers (one Node, one Interface) emit on the same topic with the
/// same name+tag. Two subscribers pin on each form. Each subscriber must
/// receive ONLY its matching publisher's payload.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn topic_node_vs_interface_no_collision() {
    let _lock = ZENOH_SERIAL.lock().await;
    let mut instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("Failed to start zenohd");
    instance.messenger().start_session().await.unwrap();

    let node_sender = TopicWireSender::new(
        "core_pub",
        "pub_inst_node",
        test_node_target("widget"),
        None,
        "frames",
    )
    .unwrap();
    let iface_sender = TopicWireSender::new(
        "core_pub",
        "pub_inst_iface",
        SenderTarget::interface("widget", "v1").expect("valid interface target"),
        None,
        "frames",
    )
    .unwrap();

    let node_receiver = TopicWireReceiver::new(
        "core_sub",
        "sub_inst_node",
        Some("core_pub"),
        None,
        Some(test_node_target("widget")),
        None,
        "frames",
    )
    .unwrap();
    let iface_receiver = TopicWireReceiver::new(
        "core_sub",
        "sub_inst_iface",
        Some("core_pub"),
        None,
        Some(SenderTarget::interface("widget", "v1").expect("valid interface target")),
        None,
        "frames",
    )
    .unwrap();

    let mut node_sub = instance
        .messenger()
        .subscribe_topic(&node_receiver, SubscriberQoS::Standard)
        .await
        .unwrap();
    let mut iface_sub = instance
        .messenger()
        .subscribe_topic(&iface_receiver, SubscriberQoS::Standard)
        .await
        .unwrap();
    wait_for_subscriber_discovery().await;

    let node_payload = Bytes::from_static(b"from_node_emission");
    let iface_payload = Bytes::from_static(b"from_iface_emission");

    instance
        .messenger()
        .publish_topic(
            &node_sender,
            Payload::from_bytes(node_payload.clone()),
            PublisherQoS::Standard,
            true,
        )
        .await
        .unwrap();
    instance
        .messenger()
        .publish_topic(
            &iface_sender,
            Payload::from_bytes(iface_payload.clone()),
            PublisherQoS::Standard,
            true,
        )
        .await
        .unwrap();

    expect_payload(&mut node_sub, &node_payload, "node-pinned subscriber").await;
    expect_payload(
        &mut iface_sub,
        &iface_payload,
        "interface-pinned subscriber",
    )
    .await;

    // Drain any stragglers: each subscriber should now have nothing more.
    expect_no_payload(&mut node_sub, "node-pinned subscriber (post-drain)").await;
    expect_no_payload(&mut iface_sub, "interface-pinned subscriber (post-drain)").await;
}

/// An untargeted subscriber (`from_target: None`) matches BOTH a node-shaped
/// and an interface-shaped publisher with the same name+tag. This locks in
/// the wildcard semantic for the discriminator segment.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn topic_untargeted_subscriber_matches_both_node_and_interface() {
    let _lock = ZENOH_SERIAL.lock().await;
    let mut instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("Failed to start zenohd");
    instance.messenger().start_session().await.unwrap();

    let node_sender = TopicWireSender::new(
        "core_pub",
        "pub_inst_node",
        test_node_target("widget"),
        None,
        "frames",
    )
    .unwrap();
    let iface_sender = TopicWireSender::new(
        "core_pub",
        "pub_inst_iface",
        SenderTarget::interface("widget", "v1").unwrap(),
        None,
        "frames",
    )
    .unwrap();

    let receiver = TopicWireReceiver::new(
        "core_sub",
        "sub_inst",
        Some("core_pub"),
        None,
        None,
        None,
        "frames",
    )
    .unwrap();

    let sub = instance
        .messenger()
        .subscribe_topic(&receiver, SubscriberQoS::Standard)
        .await
        .unwrap();
    wait_for_subscriber_discovery().await;

    let node_payload = Bytes::from_static(b"untargeted_sees_node");
    let iface_payload = Bytes::from_static(b"untargeted_sees_iface");

    instance
        .messenger()
        .publish_topic(
            &node_sender,
            Payload::from_bytes(node_payload.clone()),
            PublisherQoS::Standard,
            true,
        )
        .await
        .unwrap();
    instance
        .messenger()
        .publish_topic(
            &iface_sender,
            Payload::from_bytes(iface_payload.clone()),
            PublisherQoS::Standard,
            true,
        )
        .await
        .unwrap();

    // Collect both payloads (in either order — both publishers race).
    let mut seen = Vec::with_capacity(2);
    for _ in 0..2 {
        let msg = tokio::time::timeout(RECV_TIMEOUT, sub.rx.recv_async())
            .await
            .expect("untargeted subscriber should see both publishers")
            .expect("subscription channel should not close");
        seen.push(msg.payload().clone());
    }
    assert!(
        seen.iter().any(|p| p == &node_payload),
        "untargeted subscriber missed the node publisher's payload"
    );
    assert!(
        seen.iter().any(|p| p == &iface_payload),
        "untargeted subscriber missed the interface publisher's payload"
    );
}

/// Two service servers bind to the same name+tag — one as Node, one as
/// Interface. A caller targeting Node must reach only the node server, and a
/// caller targeting Interface must reach only the interface server.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_node_vs_interface_no_collision() {
    let _lock = ZENOH_SERIAL.lock().await;
    let mut instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("Failed to start zenohd");
    instance.messenger().start_session().await.unwrap();

    let node_server_receiver = ServiceWireReceiver::new(
        "server_core",
        "server_inst_node",
        test_node_target("widget"),
        "ping",
        ServiceKind::Service,
    )
    .unwrap();
    let iface_server_receiver = ServiceWireReceiver::new(
        "server_core",
        "server_inst_iface",
        SenderTarget::interface("widget", "v1").unwrap(),
        "ping",
        ServiceKind::Service,
    )
    .unwrap();

    let mut node_server_queryable = instance
        .messenger()
        .listen_service(&node_server_receiver)
        .await
        .unwrap();
    let mut iface_server_queryable = instance
        .messenger()
        .listen_service(&iface_server_receiver)
        .await
        .unwrap();
    wait_for_subscriber_discovery().await;

    // Full-wildcard targets: the SenderTarget kind (node vs interface) must
    // be the only thing routing each call to its matching server, which is
    // exactly the collision this test guards against.
    let node_caller_sender = ServiceWireSender::new(
        "caller_core",
        "caller_inst",
        None,
        test_node_target("widget"),
        "ping",
        ServiceKind::Service,
    )
    .unwrap();
    let iface_caller_sender = ServiceWireSender::new(
        "caller_core",
        "caller_inst",
        None,
        SenderTarget::interface("widget", "v1").unwrap(),
        "ping",
        ServiceKind::Service,
    )
    .unwrap();

    // Issue one get through each target — held alive for the duration of the
    // test so dropping the reply stream doesn't cancel the query before the
    // server side has a chance to observe it.
    let node_request_payload = Bytes::from_static(b"to_node_server");
    let iface_request_payload = Bytes::from_static(b"to_iface_server");
    let _node_replies = instance
        .messenger()
        .call_service(
            &node_caller_sender,
            Payload::from_bytes(node_request_payload.clone()),
            ServiceQueryKind::UserRequest,
            Some(RECV_TIMEOUT),
        )
        .await
        .unwrap();
    let _iface_replies = instance
        .messenger()
        .call_service(
            &iface_caller_sender,
            Payload::from_bytes(iface_request_payload.clone()),
            ServiceQueryKind::UserRequest,
            Some(RECV_TIMEOUT),
        )
        .await
        .unwrap();

    // The node server must receive ONLY the node-shaped query.
    let node_request = recv_first_query(&mut node_server_queryable)
        .await
        .expect("node server should receive its caller's query");
    assert_eq!(
        node_request.payload.to_bytes(),
        node_request_payload,
        "node server received the wrong payload (collision)"
    );
    assert_no_further_query(&mut node_server_queryable, "node server").await;

    // The interface server must receive ONLY the interface-shaped query.
    let iface_request = recv_first_query(&mut iface_server_queryable)
        .await
        .expect("interface server should receive its caller's query");
    assert_eq!(
        iface_request.payload.to_bytes(),
        iface_request_payload,
        "interface server received the wrong payload (collision)"
    );
    assert_no_further_query(&mut iface_server_queryable, "interface server").await;
}

/// Waits for the first inbound request on the service queryable's fan-in
/// channel. Returns `None` on timeout.
async fn recv_first_query(queryable: &mut ServiceQueryable) -> Option<pmi::IncomingRequest> {
    tokio::time::timeout(RECV_TIMEOUT, queryable.rx.recv_async())
        .await
        .ok()
        .and_then(|r| r.ok())
}

/// Fails the test if the queryable yields another request within
/// `NO_MESSAGE_TIMEOUT`. Used after consuming the expected query to confirm
/// no cross-talk from the opposite target.
async fn assert_no_further_query(queryable: &mut ServiceQueryable, label: &str) {
    if let Ok(Ok(req)) = tokio::time::timeout(NO_MESSAGE_TIMEOUT, queryable.rx.recv_async()).await {
        panic!(
            "{label}: received unexpected cross-talk payload of {} bytes",
            req.payload.len()
        );
    }
}
