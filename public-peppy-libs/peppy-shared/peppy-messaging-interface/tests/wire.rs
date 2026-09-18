//! End-to-end roundtrip tests for the typed `MessengerBackend` API against a
//! real zenoh router. Validates that every (sender × receiver) combination of
//! the wire protocol actually exchanges messages over the bus.
//!
//! Gated on `build_zenoh` because each test spawns a zenohd process. Serialized
//! via [`common::ZENOH_SERIAL`] to avoid parallel-startup handshake flakiness.

#![cfg(feature = "build_zenoh")]

mod common;
use common::{RECV_TIMEOUT, ZENOH_SERIAL, test_node_target, wait_for_subscriber_discovery};

use bytes::Bytes;
use pmi::{
    ActionWireReceiver, ActionWireSender, IncomingRequest, MessengerBackend, Payload, ProducerRef,
    PublisherQoS, ReplyStream, SenderTarget, ServiceKind, ServiceQueryKind, ServiceQueryable,
    ServiceWireReceiver, ServiceWireSender, SubscriberQoS, Subscription, TopicMessage,
    TopicWireReceiver, TopicWireSender, ZenohAdapter,
};

/// Awaits the next message on `sub` or fails the test after `RECV_TIMEOUT`. The
/// `label` is included in the panic message so reviewers can identify which
/// receiver stalled in CI.
async fn recv_or_timeout(sub: &mut Subscription, label: &str) -> TopicMessage {
    tokio::time::timeout(RECV_TIMEOUT, sub.rx.recv_async())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for message on {label}"))
        .unwrap_or_else(|_| panic!("channel closed before message on {label}"))
}

/// Waits for the next inbound request on a service queryable's fan-in
/// channel. Panics on timeout.
async fn recv_request(queryable: &mut ServiceQueryable) -> IncomingRequest {
    tokio::time::timeout(RECV_TIMEOUT, queryable.rx.recv_async())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for inbound service request"))
        .unwrap_or_else(|_| panic!("service queryable channel closed before request arrived"))
}

/// Waits for the next reply on a service `ReplyStream` and returns its
/// underlying [`TopicMessage`] (caller-visible payload + responder identity).
/// Panics on timeout. Tests in this file send a single `Response`-kind
/// reply per request, so callers don't need to inspect the reply kind.
async fn recv_reply(stream: &mut ReplyStream, label: &str) -> TopicMessage {
    tokio::time::timeout(RECV_TIMEOUT, stream.rx.recv())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for reply on {label}"))
        .unwrap_or_else(|| panic!("reply stream closed before message on {label}"))
        .into_message()
}

// ─── Topics ───────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn topic_native_roundtrip() {
    let _lock = ZENOH_SERIAL.lock().await;
    let mut instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("Failed to start zenohd process");
    instance.messenger().start_session().await.unwrap();

    let sender = TopicWireSender::new(
        "core_pub",
        "publisher_inst",
        test_node_target("uvc_camera"),
        None,
        "video_stream",
    )
    .expect("valid wire fields");
    let receiver = TopicWireReceiver::new(
        "core_sub",
        "subscriber_inst",
        Some("core_pub"),
        Some("publisher_inst"),
        Some(test_node_target("uvc_camera")),
        None,
        "video_stream",
    )
    .expect("valid wire fields");

    let mut sub = instance
        .messenger()
        .subscribe_topic(&receiver, SubscriberQoS::Standard)
        .await
        .unwrap();
    wait_for_subscriber_discovery().await;

    let body = Bytes::from_static(b"native_frame");
    instance
        .messenger()
        .publish_topic(
            &sender,
            Payload::from_bytes(body.clone()),
            PublisherQoS::Standard,
            true,
        )
        .await
        .unwrap();

    let received = recv_or_timeout(&mut sub, "topic_native_roundtrip sub").await;
    assert_eq!(received.payload(), &body);
    assert_eq!(received.core_node(), "core_pub");
    assert_eq!(received.instance_id(), "publisher_inst");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn topic_contract_roundtrip() {
    let _lock = ZENOH_SERIAL.lock().await;
    let mut instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .unwrap();
    instance.messenger().start_session().await.unwrap();

    let target = SenderTarget::contract("manipulator", "v1-rc2").expect("valid target");
    let sender = TopicWireSender::new("core_pub", "pub_inst", target.clone(), None, "joint_states")
        .expect("valid wire fields");
    let receiver = TopicWireReceiver::new(
        "core_sub",
        "sub_inst",
        Some("core_pub"),
        Some("pub_inst"),
        Some(target),
        None,
        "joint_states",
    )
    .expect("valid wire fields");

    let mut sub = instance
        .messenger()
        .subscribe_topic(&receiver, SubscriberQoS::Standard)
        .await
        .unwrap();
    wait_for_subscriber_discovery().await;

    let body = Bytes::from_static(b"q=[0.1,0.2,0.3]");
    instance
        .messenger()
        .publish_topic(
            &sender,
            Payload::from_bytes(body.clone()),
            PublisherQoS::Standard,
            true,
        )
        .await
        .unwrap();

    let received = recv_or_timeout(&mut sub, "topic_contract_roundtrip sub").await;
    assert_eq!(received.payload(), &body);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn topic_wildcard_subscriber() {
    let _lock = ZENOH_SERIAL.lock().await;
    let mut instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .unwrap();
    instance.messenger().start_session().await.unwrap();

    let sender = TopicWireSender::new(
        "any_publisher_core",
        "any_publisher_inst",
        test_node_target("uvc_camera"),
        None,
        "frames",
    )
    .expect("valid wire fields");
    // Receiver is fully untargeted: both `from_core_node` and `from_instance_id` None.
    let receiver = TopicWireReceiver::new(
        "subscriber_core",
        "subscriber_inst",
        None,
        None,
        Some(test_node_target("uvc_camera")),
        None,
        "frames",
    )
    .expect("valid wire fields");

    let mut sub = instance
        .messenger()
        .subscribe_topic(&receiver, SubscriberQoS::Standard)
        .await
        .unwrap();
    wait_for_subscriber_discovery().await;

    let body = Bytes::from_static(b"frame_42");
    instance
        .messenger()
        .publish_topic(
            &sender,
            Payload::from_bytes(body.clone()),
            PublisherQoS::Standard,
            true,
        )
        .await
        .unwrap();

    let received = recv_or_timeout(&mut sub, "topic_wildcard_subscriber sub").await;
    assert_eq!(received.payload(), &body);
}

// ─── Services ─────────────────────────────────────────────────────────────

fn service_receiver() -> ServiceWireReceiver {
    ServiceWireReceiver::new(
        "server_core",
        "server_inst",
        test_node_target("robot_arm"),
        "ping",
        ServiceKind::Service,
    )
    .expect("valid wire fields")
}

fn service_sender(target: Option<&ProducerRef>) -> ServiceWireSender {
    ServiceWireSender::new(
        "client_core",
        "client_inst",
        target,
        test_node_target("robot_arm"),
        "ping",
        ServiceKind::Service,
    )
    .expect("valid wire fields")
}

async fn run_service_roundtrip(sender: ServiceWireSender) {
    let mut instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .unwrap();
    instance.messenger().start_session().await.unwrap();

    let receiver = service_receiver();
    let mut queryable = instance
        .messenger()
        .listen_service(&receiver)
        .await
        .unwrap();
    wait_for_subscriber_discovery().await;

    let request_payload = Payload::from_bytes(Bytes::from_static(b"ping?"));
    let mut reply_stream = instance
        .messenger()
        .call_service(
            &sender,
            request_payload,
            ServiceQueryKind::UserRequest,
            Some(RECV_TIMEOUT),
        )
        .await
        .unwrap();

    // Server: wait for the request and verify the producer-bound link_id.
    let incoming = recv_request(&mut queryable).await;
    assert_eq!(incoming.link_id, pmi::DEFAULT_LINK_ID);
    assert_eq!(incoming.kind, ServiceQueryKind::UserRequest);

    // Server: respond via the token.
    let response_body = Bytes::from_static(b"pong");
    incoming
        .token
        .respond_response(Payload::from_bytes(response_body.clone()))
        .await
        .unwrap();

    // Client: drain replies until the user payload arrives (Zenoh delivers
    // every reply with `ConsolidationMode::None`, but in this test only one
    // reply is sent — no ACK because the adapter no longer auto-ACKs.)
    let response = recv_reply(&mut reply_stream, "service reply_stream").await;
    assert_eq!(response.payload(), &response_body);
}

// Only two target shapes exist on the wire now: a full `(core_node,
// instance_id)` pin and a full wildcard (the discovery probe shape).
// Half-pinned selectors are unrepresentable at the constructor.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_specific_request_response() {
    let _lock = ZENOH_SERIAL.lock().await;
    run_service_roundtrip(service_sender(Some(&ProducerRef::new(
        "server_core",
        "server_inst",
    ))))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_full_broadcast() {
    let _lock = ZENOH_SERIAL.lock().await;
    run_service_roundtrip(service_sender(None)).await;
}

// ─── Actions ──────────────────────────────────────────────────────────────

fn action_receiver() -> ActionWireReceiver {
    ActionWireReceiver::new(
        "server_core",
        "server_inst",
        test_node_target("robot_arm"),
        "pick_place",
    )
    .expect("valid wire fields")
}

fn action_sender() -> ActionWireSender {
    ActionWireSender::new(
        "client_core",
        "client_inst",
        Some(&ProducerRef::new("server_core", "server_inst")),
        test_node_target("robot_arm"),
        "pick_place",
    )
    .expect("valid wire fields")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn action_goal_feedback_result() {
    let _lock = ZENOH_SERIAL.lock().await;
    let mut instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .unwrap();
    instance.messenger().start_session().await.unwrap();

    let server = action_receiver();
    let client = action_sender();
    let goal_id = "goal_xyz";

    let mut goal_queryable = instance
        .messenger()
        .listen_service(&server.goal_service())
        .await
        .unwrap();
    let mut result_queryable = instance
        .messenger()
        .listen_service(&server.result_service())
        .await
        .unwrap();
    wait_for_subscriber_discovery().await;

    // Client subscribes to feedback BEFORE sending the goal — otherwise early
    // feedback can be lost in tight in-process tests.
    let mut feedback_sub = instance
        .messenger()
        .subscribe_action_feedback(&client, goal_id, SubscriberQoS::Standard)
        .await
        .unwrap();
    wait_for_subscriber_discovery().await;

    // Client sends the goal — the reply stream stays alive until we've
    // received the goal response.
    let goal_payload = Payload::from_bytes(Bytes::from_static(b"goal_data"));
    let mut goal_replies = instance
        .messenger()
        .call_service(
            &client.goal_service(),
            goal_payload,
            ServiceQueryKind::UserRequest,
            Some(RECV_TIMEOUT),
        )
        .await
        .unwrap();

    // Server: receive goal request, respond.
    let goal_request = recv_request(&mut goal_queryable).await;
    assert_eq!(goal_request.link_id, pmi::DEFAULT_LINK_ID);
    goal_request
        .token
        .respond_response(Payload::from_bytes(Bytes::from_static(b"goal_accepted")))
        .await
        .unwrap();

    // Client receives goal response.
    let goal_response = recv_reply(&mut goal_replies, "goal_replies").await;
    assert_eq!(
        goal_response.payload(),
        &Bytes::from_static(b"goal_accepted")
    );

    // Server publishes feedback for the goal.
    let feedback_pub = instance
        .messenger()
        .declare_action_feedback_publisher(
            &server,
            pmi::DEFAULT_LINK_ID,
            goal_id,
            PublisherQoS::Important,
        )
        .unwrap();
    feedback_pub
        .publish(Bytes::from_static(b"progress=0.5"))
        .await
        .unwrap();

    // Client receives feedback.
    let feedback = recv_or_timeout(&mut feedback_sub, "feedback_sub").await;
    assert_eq!(feedback.payload(), &Bytes::from_static(b"progress=0.5"));

    // Client polls result service.
    let result_payload = Payload::from_bytes(Bytes::from_static(b"result_query"));
    let mut result_replies = instance
        .messenger()
        .call_service(
            &client.result_service(),
            result_payload,
            ServiceQueryKind::UserRequest,
            Some(RECV_TIMEOUT),
        )
        .await
        .unwrap();
    let result_request = recv_request(&mut result_queryable).await;
    result_request
        .token
        .respond_response(Payload::from_bytes(Bytes::from_static(b"result=done")))
        .await
        .unwrap();
    let result_response = recv_reply(&mut result_replies, "result_replies").await;
    assert_eq!(
        result_response.payload(),
        &Bytes::from_static(b"result=done")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn action_cancel_roundtrip() {
    let _lock = ZENOH_SERIAL.lock().await;
    let mut instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .unwrap();
    instance.messenger().start_session().await.unwrap();

    let server = action_receiver();
    let client = action_sender();

    let mut cancel_queryable = instance
        .messenger()
        .listen_service(&server.cancel_service())
        .await
        .unwrap();
    wait_for_subscriber_discovery().await;

    let cancel_payload = Payload::from_bytes(Bytes::from_static(b"cancel_goal_xyz"));
    let mut cancel_replies = instance
        .messenger()
        .call_service(
            &client.cancel_service(),
            cancel_payload,
            ServiceQueryKind::UserRequest,
            Some(RECV_TIMEOUT),
        )
        .await
        .unwrap();

    let cancel_request = recv_request(&mut cancel_queryable).await;
    cancel_request
        .token
        .respond_response(Payload::from_bytes(Bytes::from_static(b"cancel_accepted")))
        .await
        .unwrap();

    let response = recv_reply(&mut cancel_replies, "cancel_replies").await;
    assert_eq!(response.payload(), &Bytes::from_static(b"cancel_accepted"));
}
