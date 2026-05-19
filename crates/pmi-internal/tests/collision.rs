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
//! process; serialized via `COLLISION_SERIAL` to avoid handshake flakiness.

#![cfg(feature = "build_zenoh")]

use bytes::Bytes;
use pmi::{
    MessengerBackend, Payload, PublisherQoS, SenderTarget, ServiceKind, ServiceWireReceiver,
    ServiceWireSender, SubscriberQoS, Subscription, TopicWireReceiver, TopicWireSender,
    ZenohAdapter,
};
use std::time::Duration;
use tokio::sync::Mutex;

static COLLISION_SERIAL: Mutex<()> = Mutex::const_new(());

fn test_node_target(name: &str) -> SenderTarget {
    SenderTarget::node(name, "v1").expect("test node target")
}

const RECV_TIMEOUT: Duration = Duration::from_secs(5);
const NO_MESSAGE_TIMEOUT: Duration = Duration::from_millis(500);

async fn wait_for_subscriber_discovery() {
    tokio::time::sleep(Duration::from_millis(500)).await;
}

/// Asserts the subscriber receives a payload exactly equal to `expected`.
async fn expect_payload(sub: &mut Subscription, expected: &Bytes, label: &str) {
    let msg = tokio::time::timeout(RECV_TIMEOUT, sub.rx.recv())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for message on {label}"))
        .unwrap_or_else(|| panic!("channel closed before message on {label}"));
    assert_eq!(
        msg.payload(),
        expected,
        "{label}: subscriber received the wrong payload"
    );
}

/// Asserts the subscriber receives no payload within `NO_MESSAGE_TIMEOUT`.
async fn expect_no_payload(sub: &mut Subscription, label: &str) {
    match tokio::time::timeout(NO_MESSAGE_TIMEOUT, sub.rx.recv()).await {
        Err(_) => {
            // Timed out — no payload arrived, which is the success case.
        }
        Ok(Some(msg)) => {
            panic!(
                "{label}: subscriber received an unexpected payload of {} bytes (collision)",
                msg.payload().len()
            );
        }
        Ok(None) => {
            // Channel closed — also acceptable, no payload arrived.
        }
    }
}

/// Two publishers (one Node, one Interface) emit on the same topic with the
/// same name+tag. Two subscribers pin on each form. Each subscriber must
/// receive ONLY its matching publisher's payload.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn topic_node_vs_interface_no_collision() {
    let _lock = COLLISION_SERIAL.lock().await;
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
        )
        .await
        .unwrap();
    instance
        .messenger()
        .publish_topic(
            &iface_sender,
            Payload::from_bytes(iface_payload.clone()),
            PublisherQoS::Standard,
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
    let _lock = COLLISION_SERIAL.lock().await;
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

    let mut sub = instance
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
        )
        .await
        .unwrap();
    instance
        .messenger()
        .publish_topic(
            &iface_sender,
            Payload::from_bytes(iface_payload.clone()),
            PublisherQoS::Standard,
        )
        .await
        .unwrap();

    // Collect both payloads (in either order — both publishers race).
    let mut seen = Vec::with_capacity(2);
    for _ in 0..2 {
        let msg = tokio::time::timeout(RECV_TIMEOUT, sub.rx.recv())
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
    let _lock = COLLISION_SERIAL.lock().await;
    let mut instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("Failed to start zenohd");
    instance.messenger().start_session().await.unwrap();

    let node_server_receiver = ServiceWireReceiver::new(
        "server_core",
        "server_inst_node",
        test_node_target("widget"),
        &[],
        "ping",
        ServiceKind::Service,
    )
    .unwrap();
    let iface_server_receiver = ServiceWireReceiver::new(
        "server_core",
        "server_inst_iface",
        SenderTarget::interface("widget", "v1").unwrap(),
        &[],
        "ping",
        ServiceKind::Service,
    )
    .unwrap();

    let mut node_server_subs = instance
        .messenger()
        .listen_service(&node_server_receiver)
        .await
        .unwrap();
    let mut iface_server_subs = instance
        .messenger()
        .listen_service(&iface_server_receiver)
        .await
        .unwrap();
    wait_for_subscriber_discovery().await;

    let node_caller_sender = ServiceWireSender::new(
        "caller_core",
        "caller_inst",
        Some("server_core"),
        None,
        test_node_target("widget"),
        None,
        "ping",
        ServiceKind::Service,
    )
    .unwrap();
    let iface_caller_sender = ServiceWireSender::new(
        "caller_core",
        "caller_inst",
        Some("server_core"),
        None,
        SenderTarget::interface("widget", "v1").unwrap(),
        None,
        "ping",
        ServiceKind::Service,
    )
    .unwrap();

    // Send one request through each target.
    let node_request_payload = Bytes::from_static(b"to_node_server");
    let iface_request_payload = Bytes::from_static(b"to_iface_server");
    let _node_response_sub = instance
        .messenger()
        .open_service_call(
            &node_caller_sender,
            "rid_node",
            Payload::from_bytes(node_request_payload.clone()),
        )
        .await
        .unwrap();
    let _iface_response_sub = instance
        .messenger()
        .open_service_call(
            &iface_caller_sender,
            "rid_iface",
            Payload::from_bytes(iface_request_payload.clone()),
        )
        .await
        .unwrap();

    // The node server must receive ONLY the node-shaped request.
    let node_msg = recv_first_from_listen_patterns(&mut node_server_subs)
        .await
        .expect("node server should receive its caller's request");
    assert_eq!(
        node_msg.payload(),
        &node_request_payload,
        "node server received the wrong payload (collision)"
    );
    assert_no_further_message(&mut node_server_subs, "node server").await;

    // The interface server must receive ONLY the interface-shaped request.
    let iface_msg = recv_first_from_listen_patterns(&mut iface_server_subs)
        .await
        .expect("interface server should receive its caller's request");
    assert_eq!(
        iface_msg.payload(),
        &iface_request_payload,
        "interface server received the wrong payload (collision)"
    );
    assert_no_further_message(&mut iface_server_subs, "interface server").await;
}

/// Waits for the first message landing on any of the four service listen
/// patterns. Returns `None` on timeout.
async fn recv_first_from_listen_patterns(
    subs: &mut [Subscription; 4],
) -> Option<pmi::TopicMessage> {
    let [s0, s1, s2, s3] = subs;
    tokio::time::timeout(RECV_TIMEOUT, async {
        tokio::select! {
            Some(msg) = s0.rx.recv() => msg,
            Some(msg) = s1.rx.recv() => msg,
            Some(msg) = s2.rx.recv() => msg,
            Some(msg) = s3.rx.recv() => msg,
        }
    })
    .await
    .ok()
}

/// Fails the test if any of the four listen patterns produces another message
/// within `NO_MESSAGE_TIMEOUT`. Used after consuming the expected request to
/// confirm no cross-talk from the opposite target.
async fn assert_no_further_message(subs: &mut [Subscription; 4], label: &str) {
    let [s0, s1, s2, s3] = subs;
    let result = tokio::time::timeout(NO_MESSAGE_TIMEOUT, async {
        tokio::select! {
            msg = s0.rx.recv() => msg,
            msg = s1.rx.recv() => msg,
            msg = s2.rx.recv() => msg,
            msg = s3.rx.recv() => msg,
        }
    })
    .await;
    if let Ok(Some(msg)) = result {
        panic!(
            "{label}: received unexpected cross-talk payload of {} bytes",
            msg.payload().len()
        );
    }
}
