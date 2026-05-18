//! End-to-end roundtrip tests for the typed `MessengerBackend` API against a
//! real zenoh router. Validates that every (sender × receiver) combination of
//! the wire protocol actually exchanges messages over the bus.
//!
//! Gated on `build_zenoh` because each test spawns a zenohd process. Serialized
//! via the same `ZENOH_SERIAL` mutex pattern as `tests/zenoh.rs` to avoid
//! parallel-startup handshake flakiness.

#![cfg(feature = "build_zenoh")]

use bytes::Bytes;
use pmi::{
    ActionWireReceiver, ActionWireSender, MessengerBackend, Payload, PublisherQoS, SenderTarget,
    ServiceKind, ServiceWireReceiver, ServiceWireSender, SubscriberQoS, Subscription, TopicMessage,
    TopicWireReceiver, TopicWireSender, ZenohAdapter,
};
use std::time::Duration;
use tokio::sync::Mutex;

static ZENOH_SERIAL: Mutex<()> = Mutex::const_new(());

const RECV_TIMEOUT: Duration = Duration::from_secs(5);

async fn wait_for_subscriber_discovery() {
    tokio::time::sleep(Duration::from_millis(500)).await;
}

/// Awaits the next message on `sub` or fails the test after `RECV_TIMEOUT`. The
/// `label` is included in the panic message so reviewers can identify which
/// receiver stalled in CI.
async fn recv_or_timeout(sub: &mut Subscription, label: &str) -> TopicMessage {
    tokio::time::timeout(RECV_TIMEOUT, sub.rx.recv())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for message on {label}"))
        .unwrap_or_else(|| panic!("channel closed before message on {label}"))
}

/// Waits for the next message from any of the four service listen patterns.
/// Panics with the offending arm name on timeout.
async fn select_listen(subs: &mut [Subscription; 4]) -> TopicMessage {
    let [s0, s1, s2, s3] = subs;
    let result = tokio::time::timeout(RECV_TIMEOUT, async {
        tokio::select! {
            Some(msg) = s0.rx.recv() => ("s0", msg),
            Some(msg) = s1.rx.recv() => ("s1", msg),
            Some(msg) = s2.rx.recv() => ("s2", msg),
            Some(msg) = s3.rx.recv() => ("s3", msg),
        }
    })
    .await;
    match result {
        Ok((_label, msg)) => msg,
        Err(_) => panic!("timed out waiting for message on any of s0/s1/s2/s3"),
    }
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
        SenderTarget::node("uvc_camera", "v1").expect("test target"),
        "video_stream",
    )
    .expect("valid wire fields");
    let receiver = TopicWireReceiver::new(
        "core_sub",
        "subscriber_inst",
        Some("core_pub"),
        Some("publisher_inst"),
        Some(SenderTarget::node("uvc_camera", "v1").expect("test target")),
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
        )
        .await
        .unwrap();

    let received = recv_or_timeout(&mut sub, "topic_native_roundtrip sub").await;
    assert_eq!(received.payload(), &body);
    assert_eq!(received.core_node(), "core_pub");
    assert_eq!(received.instance_id(), "publisher_inst");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn topic_iface_roundtrip() {
    let _lock = ZENOH_SERIAL.lock().await;
    let mut instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .unwrap();
    instance.messenger().start_session().await.unwrap();

    let target = SenderTarget::interface("manipulator", "v1-rc2").expect("valid target");
    let sender = TopicWireSender::new("core_pub", "pub_inst", target.clone(), "joint_states")
        .expect("valid wire fields");
    let receiver = TopicWireReceiver::new(
        "core_sub",
        "sub_inst",
        Some("core_pub"),
        Some("pub_inst"),
        Some(target),
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
        )
        .await
        .unwrap();

    let received = recv_or_timeout(&mut sub, "topic_iface_roundtrip sub").await;
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
        SenderTarget::node("uvc_camera", "v1").expect("test target"),
        "frames",
    )
    .expect("valid wire fields");
    // Receiver is fully untargeted: both `from_core_node` and `from_instance_id` None.
    let receiver = TopicWireReceiver::new(
        "subscriber_core",
        "subscriber_inst",
        None,
        None,
        Some(SenderTarget::node("uvc_camera", "v1").expect("test target")),
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
        SenderTarget::node("robot_arm", "v1").expect("test target"),
        "ping",
        ServiceKind::Service,
    )
    .expect("valid wire fields")
}

fn service_sender(to_core_node: Option<&str>, to_instance_id: Option<&str>) -> ServiceWireSender {
    ServiceWireSender::new(
        "client_core",
        "client_inst",
        to_core_node,
        to_instance_id,
        SenderTarget::node("robot_arm", "v1").expect("test target"),
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
    let mut subs = instance
        .messenger()
        .listen_service(&receiver)
        .await
        .unwrap();
    wait_for_subscriber_discovery().await;

    let request_id = "req_1";
    let request_payload = Payload::from_bytes(Bytes::from_static(b"ping?"));
    let mut response_sub = instance
        .messenger()
        .open_service_call(&sender, request_id, request_payload)
        .await
        .unwrap();

    // Server: wait for the request on whichever pattern fired.
    let received_request = select_listen(&mut subs).await;
    let parsed_id = instance
        .messenger()
        .parse_service_request_id(&receiver, received_request.key_expr())
        .unwrap();
    assert_eq!(parsed_id, request_id);

    // Server: respond with the parsed key as the address descriptor.
    let response_body = Bytes::from_static(b"pong");
    instance
        .messenger()
        .publish_service_response(
            &receiver,
            received_request.key_expr(),
            Payload::from_bytes(response_body.clone()),
        )
        .await
        .unwrap();

    // Client: receive response.
    let response = recv_or_timeout(&mut response_sub, "service response_sub").await;
    assert_eq!(response.payload(), &response_body);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_specific_request_response() {
    let _lock = ZENOH_SERIAL.lock().await;
    run_service_roundtrip(service_sender(Some("server_core"), Some("server_inst"))).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_broadcast_any_instance() {
    let _lock = ZENOH_SERIAL.lock().await;
    run_service_roundtrip(service_sender(Some("server_core"), None)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_broadcast_any_core() {
    let _lock = ZENOH_SERIAL.lock().await;
    run_service_roundtrip(service_sender(None, Some("server_inst"))).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_full_broadcast() {
    let _lock = ZENOH_SERIAL.lock().await;
    run_service_roundtrip(service_sender(None, None)).await;
}

// ─── Actions ──────────────────────────────────────────────────────────────

fn action_receiver() -> ActionWireReceiver {
    ActionWireReceiver::new(
        "server_core",
        "server_inst",
        SenderTarget::node("robot_arm", "v1").expect("test target"),
        "pick_place",
    )
    .expect("valid wire fields")
}

fn action_sender() -> ActionWireSender {
    ActionWireSender::new(
        "client_core",
        "client_inst",
        Some("server_core"),
        Some("server_inst"),
        SenderTarget::node("robot_arm", "v1").expect("test target"),
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

    let mut goal_subs = instance
        .messenger()
        .listen_service(&server.goal_service())
        .await
        .unwrap();
    let mut result_subs = instance
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

    // Client sends the goal.
    let goal_payload = Payload::from_bytes(Bytes::from_static(b"goal_data"));
    let mut goal_response_sub = instance
        .messenger()
        .open_service_call(&client.goal_service(), "rid_goal", goal_payload)
        .await
        .unwrap();

    // Server: receive goal, ack with response.
    let goal_request = select_listen(&mut goal_subs).await;
    let goal_request_id = instance
        .messenger()
        .parse_service_request_id(&server.goal_service(), goal_request.key_expr())
        .unwrap();
    assert_eq!(goal_request_id, "rid_goal");
    instance
        .messenger()
        .publish_service_response(
            &server.goal_service(),
            goal_request.key_expr(),
            Payload::from_bytes(Bytes::from_static(b"goal_accepted")),
        )
        .await
        .unwrap();

    // Client receives goal response.
    let goal_response = recv_or_timeout(&mut goal_response_sub, "goal_response_sub").await;
    assert_eq!(
        goal_response.payload(),
        &Bytes::from_static(b"goal_accepted")
    );

    // Server publishes feedback for the goal.
    let feedback_pub = instance
        .messenger()
        .declare_action_feedback_publisher(&server, goal_id, PublisherQoS::Important)
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
    let mut result_response_sub = instance
        .messenger()
        .open_service_call(&client.result_service(), "rid_result", result_payload)
        .await
        .unwrap();
    let result_request = select_listen(&mut result_subs).await;
    instance
        .messenger()
        .publish_service_response(
            &server.result_service(),
            result_request.key_expr(),
            Payload::from_bytes(Bytes::from_static(b"result=done")),
        )
        .await
        .unwrap();
    let result_response = recv_or_timeout(&mut result_response_sub, "result_response_sub").await;
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

    let mut cancel_subs = instance
        .messenger()
        .listen_service(&server.cancel_service())
        .await
        .unwrap();
    wait_for_subscriber_discovery().await;

    let cancel_payload = Payload::from_bytes(Bytes::from_static(b"cancel_goal_xyz"));
    let mut cancel_response_sub = instance
        .messenger()
        .open_service_call(&client.cancel_service(), "rid_cancel", cancel_payload)
        .await
        .unwrap();

    let cancel_request = select_listen(&mut cancel_subs).await;
    instance
        .messenger()
        .publish_service_response(
            &server.cancel_service(),
            cancel_request.key_expr(),
            Payload::from_bytes(Bytes::from_static(b"cancel_accepted")),
        )
        .await
        .unwrap();

    let response = recv_or_timeout(&mut cancel_response_sub, "cancel_response_sub").await;
    assert_eq!(response.payload(), &Bytes::from_static(b"cancel_accepted"));
}
