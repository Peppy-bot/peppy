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
    ActionWireReceiver, ActionWireSender, Iface, MessengerBackend, Payload, PublisherQoS,
    ServiceKind, ServiceWireReceiver, ServiceWireSender, SubscriberQoS, Subscription, TopicMessage,
    TopicWireReceiver, TopicWireSender, ZenohAdapter,
};
use std::time::Duration;
use tokio::sync::Mutex;

static ZENOH_SERIAL: Mutex<()> = Mutex::const_new(());

async fn wait_for_subscriber_discovery() {
    tokio::time::sleep(Duration::from_millis(500)).await;
}

/// Waits for the next message from any of the four service listen patterns.
async fn select_listen(subs: &mut [Subscription; 4]) -> TopicMessage {
    let [s0, s1, s2, s3] = subs;
    tokio::select! {
        Some(msg) = s0.rx.recv() => msg,
        Some(msg) = s1.rx.recv() => msg,
        Some(msg) = s2.rx.recv() => msg,
        Some(msg) = s3.rx.recv() => msg,
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

    let sender = TopicWireSender {
        as_core_node: "core_pub".into(),
        as_instance_id: "publisher_inst".into(),
        as_node_name: "uvc_camera".into(),
        iface: Iface::native(),
        as_topic_name: "video_stream".into(),
    };
    let receiver = TopicWireReceiver {
        as_core_node: "core_sub".into(),
        as_instance_id: "subscriber_inst".into(),
        to_core_node: Some("core_pub".into()),
        to_instance_id: Some("publisher_inst".into()),
        to_node_name: "uvc_camera".into(),
        iface: Iface::native(),
        to_topic: "video_stream".into(),
    };

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

    let received = sub.rx.recv().await.expect("topic message");
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

    let iface = Iface::new("manipulator", "v1-rc2");
    let sender = TopicWireSender {
        as_core_node: "core_pub".into(),
        as_instance_id: "pub_inst".into(),
        as_node_name: "robot_arm".into(),
        iface: iface.clone(),
        as_topic_name: "joint_states".into(),
    };
    let receiver = TopicWireReceiver {
        as_core_node: "core_sub".into(),
        as_instance_id: "sub_inst".into(),
        to_core_node: Some("core_pub".into()),
        to_instance_id: Some("pub_inst".into()),
        to_node_name: "robot_arm".into(),
        iface,
        to_topic: "joint_states".into(),
    };

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

    let received = sub.rx.recv().await.expect("topic message");
    assert_eq!(received.payload(), &body);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn topic_wildcard_subscriber() {
    let _lock = ZENOH_SERIAL.lock().await;
    let mut instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .unwrap();
    instance.messenger().start_session().await.unwrap();

    let sender = TopicWireSender {
        as_core_node: "any_publisher_core".into(),
        as_instance_id: "any_publisher_inst".into(),
        as_node_name: "uvc_camera".into(),
        iface: Iface::native(),
        as_topic_name: "frames".into(),
    };
    // Receiver is fully untargeted — both `to_core_node` and `to_instance_id` None.
    let receiver = TopicWireReceiver {
        as_core_node: "subscriber_core".into(),
        as_instance_id: "subscriber_inst".into(),
        to_core_node: None,
        to_instance_id: None,
        to_node_name: "uvc_camera".into(),
        iface: Iface::native(),
        to_topic: "frames".into(),
    };

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

    let received = sub.rx.recv().await.expect("topic message");
    assert_eq!(received.payload(), &body);
}

// ─── Services ─────────────────────────────────────────────────────────────

fn service_receiver() -> ServiceWireReceiver {
    ServiceWireReceiver {
        bound_core_node: "server_core".into(),
        as_instance_id: "server_inst".into(),
        as_node_name: "robot_arm".into(),
        iface: Iface::native(),
        as_service_name: "ping".into(),
        kind: ServiceKind::Service,
    }
}

fn service_sender(to_core_node: Option<&str>, to_instance_id: Option<&str>) -> ServiceWireSender {
    ServiceWireSender {
        bound_core_node: "client_core".into(),
        as_instance_id: "client_inst".into(),
        to_core_node: to_core_node.map(str::to_string),
        to_instance_id: to_instance_id.map(str::to_string),
        to_node_name: "robot_arm".into(),
        iface: Iface::native(),
        to_service_name: "ping".into(),
        kind: ServiceKind::Service,
    }
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
    let response = response_sub.rx.recv().await.expect("response");
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
    ActionWireReceiver {
        bound_core_node: "server_core".into(),
        as_instance_id: "server_inst".into(),
        as_node_name: "robot_arm".into(),
        iface: Iface::native(),
        as_action_name: "pick_place".into(),
    }
}

fn action_sender() -> ActionWireSender {
    ActionWireSender {
        as_core_node: "client_core".into(),
        as_instance_id: "client_inst".into(),
        to_core_node: Some("server_core".into()),
        to_instance_id: Some("server_inst".into()),
        to_node_name: "robot_arm".into(),
        iface: Iface::native(),
        to_action_name: "pick_place".into(),
    }
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
    let goal_response = goal_response_sub.rx.recv().await.expect("goal response");
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
    let feedback = feedback_sub.rx.recv().await.expect("feedback");
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
    let result_response = result_response_sub.rx.recv().await.expect("result");
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

    let response = cancel_response_sub.rx.recv().await.expect("response");
    assert_eq!(response.payload(), &Bytes::from_static(b"cancel_accepted"));
}
