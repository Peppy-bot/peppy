//! Snapshot tests for zenoh wire format functions. These pin the exact
//! keyexpr strings produced for every (role × variant) combination — if a
//! change shifts the bytes, the test catches it. Roundtrip tests against a
//! real router live in `crates/pmi-internal/tests/wire.rs`.

use super::*;
use crate::wire::Iface;

// ─── Topics ───────────────────────────────────────────────────────────────

#[test]
fn topic_publish_native_iface() {
    let sender = TopicWireSender {
        as_core_node: "core_node_a".into(),
        as_instance_id: "publisher_inst".into(),
        as_node_name: "uvc_camera".into(),
        iface: Iface::native(),
        as_topic_name: "video_stream".into(),
    };
    assert_eq!(
        ZenohWire::topic_publish(&sender),
        "*/core_node_a/*/publisher_inst/topic/uvc_camera/_/_/video_stream"
    );
}

#[test]
fn topic_publish_with_iface_normalizes_tag() {
    let sender = TopicWireSender {
        as_core_node: "core_a".into(),
        as_instance_id: "inst_1".into(),
        as_node_name: "robot_arm".into(),
        iface: Iface::new("manipulator", "v1-beta-2"),
        as_topic_name: "joint_states".into(),
    };
    assert_eq!(
        ZenohWire::topic_publish(&sender),
        "*/core_a/*/inst_1/topic/robot_arm/manipulator/v1_beta_2/joint_states"
    );
}

#[test]
fn topic_subscribe_targeted_native_iface() {
    let receiver = TopicWireReceiver {
        as_core_node: "core_subscriber".into(),
        as_instance_id: "sub_inst".into(),
        to_core_node: Some("core_publisher".into()),
        to_instance_id: Some("pub_inst".into()),
        to_node_name: Some("uvc_camera".into()),
        iface: Iface::native(),
        to_topic: "video_stream".into(),
    };
    assert_eq!(
        ZenohWire::topic_subscribe(&receiver),
        "core_subscriber/core_publisher/sub_inst/pub_inst/topic/uvc_camera/_/_/video_stream"
    );
}

#[test]
fn topic_subscribe_untargeted_uses_single_chunk_wildcard() {
    let receiver = TopicWireReceiver {
        as_core_node: "core_subscriber".into(),
        as_instance_id: "sub_inst".into(),
        to_core_node: None,
        to_instance_id: None,
        to_node_name: Some("uvc_camera".into()),
        iface: Iface::native(),
        to_topic: "video_stream".into(),
    };
    assert_eq!(
        ZenohWire::topic_subscribe(&receiver),
        "core_subscriber/*/sub_inst/*/topic/uvc_camera/_/_/video_stream"
    );
}

#[test]
fn topic_subscribe_partial_target() {
    let receiver = TopicWireReceiver {
        as_core_node: "core_subscriber".into(),
        as_instance_id: "sub_inst".into(),
        to_core_node: Some("core_publisher".into()),
        to_instance_id: None,
        to_node_name: Some("robot_arm".into()),
        iface: Iface::new("manipulator", "v1"),
        to_topic: "joint_states".into(),
    };
    assert_eq!(
        ZenohWire::topic_subscribe(&receiver),
        "core_subscriber/core_publisher/sub_inst/*/topic/robot_arm/manipulator/v1/joint_states"
    );
}

#[test]
fn topic_subscribe_external_consumer_wildcards_node_and_iface() {
    let receiver = TopicWireReceiver {
        as_core_node: "core_subscriber".into(),
        as_instance_id: "sub_inst".into(),
        to_core_node: None,
        to_instance_id: None,
        to_node_name: None,
        iface: Iface::wildcard(),
        to_topic: "video_stream".into(),
    };
    assert_eq!(
        ZenohWire::topic_subscribe(&receiver),
        "core_subscriber/*/sub_inst/*/topic/*/*/*/video_stream"
    );
}

// ─── Services — listen patterns ───────────────────────────────────────────

fn sample_service_receiver(kind: ServiceKind) -> ServiceWireReceiver {
    ServiceWireReceiver {
        bound_core_node: "server_core".into(),
        as_instance_id: "server_inst".into(),
        as_node_name: "robot_arm".into(),
        iface: Iface::native(),
        as_service_name: "ping".into(),
        kind,
    }
}

#[test]
fn service_listen_patterns_native_iface_plain_service() {
    let recv = sample_service_receiver(ServiceKind::Service);
    let patterns = ZenohWire::service_listen_patterns(&recv);
    assert_eq!(
        patterns,
        [
            "server_core/*/server_inst/*/service/robot_arm/_/_/ping/request/**".to_string(),
            "server_core/*/_any_/*/service/robot_arm/_/_/ping/request/**".to_string(),
            "_any_/*/server_inst/*/service/robot_arm/_/_/ping/request/**".to_string(),
            "_any_/*/_any_/*/service/robot_arm/_/_/ping/request/**".to_string(),
        ]
    );
}

#[test]
fn service_listen_patterns_action_goal_appends_suffix() {
    let mut recv = sample_service_receiver(ServiceKind::ActionGoal);
    recv.as_service_name = "pick_place".into();
    let patterns = ZenohWire::service_listen_patterns(&recv);
    assert_eq!(
        patterns[0],
        "server_core/*/server_inst/*/action/robot_arm/_/_/pick_place/goal/request/**"
    );
    assert_eq!(
        patterns[3],
        "_any_/*/_any_/*/action/robot_arm/_/_/pick_place/goal/request/**"
    );
}

#[test]
fn service_listen_patterns_conformed_iface_normalizes_tag() {
    let mut recv = sample_service_receiver(ServiceKind::Service);
    recv.iface = Iface::new("manipulator", "v2-beta");
    let patterns = ZenohWire::service_listen_patterns(&recv);
    assert_eq!(
        patterns[0],
        "server_core/*/server_inst/*/service/robot_arm/manipulator/v2_beta/ping/request/**"
    );
}

// ─── Services — request publish ───────────────────────────────────────────

fn sample_service_sender(kind: ServiceKind) -> ServiceWireSender {
    ServiceWireSender {
        bound_core_node: "caller_core".into(),
        as_instance_id: "caller_inst".into(),
        to_core_node: Some("target_core".into()),
        to_instance_id: Some("target_inst".into()),
        to_node_name: "robot_arm".into(),
        iface: Iface::native(),
        to_service_name: "ping".into(),
        kind,
    }
}

#[test]
fn service_request_publish_specific_target() {
    let sender = sample_service_sender(ServiceKind::Service);
    assert_eq!(
        ZenohWire::service_request_publish(&sender, "abc123"),
        "target_core/caller_core/target_inst/caller_inst/service/robot_arm/_/_/ping/request/abc123"
    );
}

#[test]
fn service_request_publish_broadcast_instance() {
    let mut sender = sample_service_sender(ServiceKind::Service);
    sender.to_instance_id = None;
    assert_eq!(
        ZenohWire::service_request_publish(&sender, "abc123"),
        "target_core/caller_core/_any_/caller_inst/service/robot_arm/_/_/ping/request/abc123"
    );
}

#[test]
fn service_request_publish_broadcast_core() {
    let mut sender = sample_service_sender(ServiceKind::Service);
    sender.to_core_node = None;
    assert_eq!(
        ZenohWire::service_request_publish(&sender, "abc123"),
        "_any_/caller_core/target_inst/caller_inst/service/robot_arm/_/_/ping/request/abc123"
    );
}

#[test]
fn service_request_publish_full_broadcast() {
    let mut sender = sample_service_sender(ServiceKind::Service);
    sender.to_core_node = None;
    sender.to_instance_id = None;
    assert_eq!(
        ZenohWire::service_request_publish(&sender, "abc123"),
        "_any_/caller_core/_any_/caller_inst/service/robot_arm/_/_/ping/request/abc123"
    );
}

#[test]
fn service_request_publish_action_goal() {
    let mut sender = sample_service_sender(ServiceKind::ActionGoal);
    sender.to_service_name = "pick_place".into();
    assert_eq!(
        ZenohWire::service_request_publish(&sender, "rid_42"),
        "target_core/caller_core/target_inst/caller_inst/action/robot_arm/_/_/pick_place/goal/request/rid_42"
    );
}

// ─── Services — response subscribe ────────────────────────────────────────

#[test]
fn service_response_subscribe_native_iface() {
    let sender = sample_service_sender(ServiceKind::Service);
    assert_eq!(
        ZenohWire::service_response_subscribe(&sender, "abc123"),
        "caller_core/*/caller_inst/*/service/robot_arm/_/_/ping/response/abc123"
    );
}

#[test]
fn service_response_subscribe_action_result() {
    let mut sender = sample_service_sender(ServiceKind::ActionResult);
    sender.to_service_name = "pick_place".into();
    sender.iface = Iface::new("manipulator", "v1");
    assert_eq!(
        ZenohWire::service_response_subscribe(&sender, "rid_42"),
        "caller_core/*/caller_inst/*/action/robot_arm/manipulator/v1/pick_place/result/response/rid_42"
    );
}

// ─── Services — parse_received_request (server-side response publish) ─────

#[test]
fn parse_received_request_round_trips_specific() {
    let receiver = sample_service_receiver(ServiceKind::Service);
    let request =
        "server_core/caller_core/server_inst/caller_inst/service/robot_arm/_/_/ping/request/abc123";
    let parsed = ZenohWire::parse_received_request(&receiver, request).expect("should parse");
    assert_eq!(parsed.request_id, "abc123");
    assert_eq!(
        parsed.response_keyexpr,
        "caller_core/server_core/caller_inst/server_inst/service/robot_arm/_/_/ping/response/abc123"
    );
}

#[test]
fn parse_received_request_response_addresses_caller_with_broadcast_request() {
    let receiver = sample_service_receiver(ServiceKind::Service);
    let request = "_any_/caller_core/_any_/caller_inst/service/robot_arm/_/_/ping/request/abc123";
    let parsed = ZenohWire::parse_received_request(&receiver, request).expect("should parse");
    // Response goes to the caller's real identity even when the request came in on
    // a broadcast pattern; the receiver's own bound fields fill the responder slots.
    assert_eq!(
        parsed.response_keyexpr,
        "caller_core/server_core/caller_inst/server_inst/service/robot_arm/_/_/ping/response/abc123"
    );
}

#[test]
fn parse_received_request_rejects_service_root_mismatch() {
    let receiver = sample_service_receiver(ServiceKind::Service);
    let request = "server_core/caller_core/server_inst/caller_inst/service/different_node/_/_/ping/request/abc";
    let err = ZenohWire::parse_received_request(&receiver, request).unwrap_err();
    assert!(matches!(err, WireParseError::ServiceRootMismatch { .. }));
}

#[test]
fn parse_received_request_rejects_missing_request_marker() {
    let receiver = sample_service_receiver(ServiceKind::Service);
    let request =
        "server_core/caller_core/server_inst/caller_inst/service/robot_arm/_/_/ping/wrong/abc";
    let err = ZenohWire::parse_received_request(&receiver, request).unwrap_err();
    assert_eq!(err, WireParseError::NotARequest);
}

#[test]
fn parse_received_request_rejects_trailing_segments() {
    let receiver = sample_service_receiver(ServiceKind::Service);
    let request = "server_core/caller_core/server_inst/caller_inst/service/robot_arm/_/_/ping/request/abc/extra";
    let err = ZenohWire::parse_received_request(&receiver, request).unwrap_err();
    assert_eq!(err, WireParseError::UnexpectedTrailing);
}

#[test]
fn parse_received_request_rejects_too_short() {
    let receiver = sample_service_receiver(ServiceKind::Service);
    let request = "only/two/segments";
    let err = ZenohWire::parse_received_request(&receiver, request).unwrap_err();
    assert!(matches!(err, WireParseError::MissingSegment(_)));
}

#[test]
fn parse_received_request_action_cancel_round_trips() {
    let mut receiver = sample_service_receiver(ServiceKind::ActionCancel);
    receiver.as_service_name = "pick_place".into();
    let request = "server_core/caller_core/server_inst/caller_inst/action/robot_arm/_/_/pick_place/cancel/request/rid_42";
    let parsed = ZenohWire::parse_received_request(&receiver, request).expect("should parse");
    assert_eq!(parsed.request_id, "rid_42");
    assert_eq!(
        parsed.response_keyexpr,
        "caller_core/server_core/caller_inst/server_inst/action/robot_arm/_/_/pick_place/cancel/response/rid_42"
    );
}

// ─── Actions — feedback ───────────────────────────────────────────────────

fn sample_action_receiver() -> ActionWireReceiver {
    ActionWireReceiver {
        bound_core_node: "server_core".into(),
        as_instance_id: "server_inst".into(),
        as_node_name: "robot_arm".into(),
        iface: Iface::native(),
        as_action_name: "pick_place".into(),
    }
}

fn sample_action_sender() -> ActionWireSender {
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

#[test]
fn action_feedback_publish_native_iface() {
    let recv = sample_action_receiver();
    assert_eq!(
        ZenohWire::action_feedback_publish(&recv, "goal_xyz"),
        "*/server_core/*/server_inst/action/robot_arm/_/_/pick_place/feedback/server_inst/goal_xyz"
    );
}

#[test]
fn action_feedback_publish_normalizes_iface_tag() {
    let mut recv = sample_action_receiver();
    recv.iface = Iface::new("manipulator", "v1-rc1");
    assert_eq!(
        ZenohWire::action_feedback_publish(&recv, "goal_xyz"),
        "*/server_core/*/server_inst/action/robot_arm/manipulator/v1_rc1/pick_place/feedback/server_inst/goal_xyz"
    );
}

#[test]
fn action_feedback_subscribe_targeted() {
    let sender = sample_action_sender();
    assert_eq!(
        ZenohWire::action_feedback_subscribe(&sender, "goal_xyz"),
        "client_core/server_core/client_inst/server_inst/action/robot_arm/_/_/pick_place/feedback/server_inst/goal_xyz"
    );
}

#[test]
fn action_feedback_subscribe_untargeted() {
    let mut sender = sample_action_sender();
    sender.to_core_node = None;
    sender.to_instance_id = None;
    assert_eq!(
        ZenohWire::action_feedback_subscribe(&sender, "goal_xyz"),
        "client_core/*/client_inst/*/action/robot_arm/_/_/pick_place/feedback/*/goal_xyz"
    );
}

#[test]
fn action_feedback_subscribe_partial_target_uses_wildcard_only_for_missing() {
    let mut sender = sample_action_sender();
    sender.to_core_node = Some("server_core".into());
    sender.to_instance_id = None;
    assert_eq!(
        ZenohWire::action_feedback_subscribe(&sender, "goal_xyz"),
        "client_core/server_core/client_inst/*/action/robot_arm/_/_/pick_place/feedback/*/goal_xyz"
    );
}
