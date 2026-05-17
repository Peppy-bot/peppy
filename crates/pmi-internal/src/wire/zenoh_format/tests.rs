//! Snapshot tests for the zenoh wire format functions. These pin the exact
//! keyexpr strings produced for every (role × variant) combination — if a
//! change shifts the bytes, the test catches it. Roundtrip tests against a
//! real router live in `crates/pmi-internal/tests/wire.rs`.

use super::*;
use crate::wire::{Iface, Segment};

/// Test-local shorthand: wrap a `&str` in a validated [`Segment`]. Panics on
/// invalid input — tests use known-good values only.
fn seg(value: &str) -> Segment {
    Segment::try_from(value).expect("test segment value should be valid")
}

// ─── Topics ───────────────────────────────────────────────────────────────

#[test]
fn topic_publish_native_iface() {
    let sender = TopicWireSender {
        as_core_node: seg("core_node_a"),
        as_instance_id: seg("publisher_inst"),
        as_node_name: seg("uvc_camera"),
        iface: Iface::native(),
        as_topic_name: seg("video_stream"),
    };
    assert_eq!(
        ZenohWireFormat::topic_publish(&sender),
        "*/core_node_a/*/publisher_inst/topic/uvc_camera/_/_/video_stream"
    );
}

#[test]
fn topic_publish_with_iface_normalizes_tag() {
    let sender = TopicWireSender {
        as_core_node: seg("core_a"),
        as_instance_id: seg("inst_1"),
        as_node_name: seg("robot_arm"),
        iface: Iface::new("manipulator", "v1-beta-2").expect("valid iface"),
        as_topic_name: seg("joint_states"),
    };
    assert_eq!(
        ZenohWireFormat::topic_publish(&sender),
        "*/core_a/*/inst_1/topic/robot_arm/manipulator/v1_beta_2/joint_states"
    );
}

#[test]
fn topic_subscribe_targeted_native_iface() {
    let receiver = TopicWireReceiver {
        as_core_node: seg("core_subscriber"),
        as_instance_id: seg("sub_inst"),
        to_core_node: Some(seg("core_publisher")),
        to_instance_id: Some(seg("pub_inst")),
        to_node_name: Some(seg("uvc_camera")),
        iface: Iface::native(),
        to_topic: seg("video_stream"),
    };
    assert_eq!(
        ZenohWireFormat::topic_subscribe(&receiver),
        "core_subscriber/core_publisher/sub_inst/pub_inst/topic/uvc_camera/_/_/video_stream"
    );
}

#[test]
fn topic_subscribe_untargeted_uses_single_chunk_wildcard() {
    let receiver = TopicWireReceiver {
        as_core_node: seg("core_subscriber"),
        as_instance_id: seg("sub_inst"),
        to_core_node: None,
        to_instance_id: None,
        to_node_name: Some(seg("uvc_camera")),
        iface: Iface::native(),
        to_topic: seg("video_stream"),
    };
    assert_eq!(
        ZenohWireFormat::topic_subscribe(&receiver),
        "core_subscriber/*/sub_inst/*/topic/uvc_camera/_/_/video_stream"
    );
}

#[test]
fn topic_subscribe_partial_target() {
    let receiver = TopicWireReceiver {
        as_core_node: seg("core_subscriber"),
        as_instance_id: seg("sub_inst"),
        to_core_node: Some(seg("core_publisher")),
        to_instance_id: None,
        to_node_name: Some(seg("robot_arm")),
        iface: Iface::new("manipulator", "v1").expect("valid iface"),
        to_topic: seg("joint_states"),
    };
    assert_eq!(
        ZenohWireFormat::topic_subscribe(&receiver),
        "core_subscriber/core_publisher/sub_inst/*/topic/robot_arm/manipulator/v1/joint_states"
    );
}

#[test]
fn topic_subscribe_external_consumer_wildcards_node_and_iface() {
    let receiver = TopicWireReceiver {
        as_core_node: seg("core_subscriber"),
        as_instance_id: seg("sub_inst"),
        to_core_node: None,
        to_instance_id: None,
        to_node_name: None,
        iface: Iface::wildcard(),
        to_topic: seg("video_stream"),
    };
    assert_eq!(
        ZenohWireFormat::topic_subscribe(&receiver),
        "core_subscriber/*/sub_inst/*/topic/*/*/*/video_stream"
    );
}

// ─── Topics — parse ────────────────────────────────────────────────────────

#[test]
fn parse_topic_keyexpr_extracts_caller_addressing() {
    // Inverse of topic_publish: the publisher segments at index 1 and 3.
    let key =
        "core_subscriber/publisher_core/sub_inst/publisher_inst/topic/sensor_node/_/_/temperature";
    let parsed = ZenohWireFormat::parse_topic_keyexpr(key).expect("should parse");
    assert_eq!(parsed.core_node, "publisher_core");
    assert_eq!(parsed.instance_id, "publisher_inst");
}

#[test]
fn parse_topic_keyexpr_roundtrips_through_topic_publish() {
    let sender = TopicWireSender {
        as_core_node: seg("core_a"),
        as_instance_id: seg("inst_1"),
        as_node_name: seg("sensor"),
        iface: Iface::native(),
        as_topic_name: seg("humidity"),
    };
    let key = ZenohWireFormat::topic_publish(&sender);
    // topic_publish wildcards the subscriber side so the first and third
    // segments are `*`. parse_topic_keyexpr reads the publisher side.
    let parsed = ZenohWireFormat::parse_topic_keyexpr(&key).expect("should parse");
    assert_eq!(parsed.core_node, sender.as_core_node.as_str());
    assert_eq!(parsed.instance_id, sender.as_instance_id.as_str());
}

#[test]
fn parse_topic_keyexpr_missing_core_node_errors() {
    let err = ZenohWireFormat::parse_topic_keyexpr("only_one_segment").unwrap_err();
    assert!(matches!(
        err,
        ZenohWireParseError::MissingSegment("caller_core_node")
    ));
}

#[test]
fn parse_topic_keyexpr_empty_core_node_errors() {
    // `splitn(5, '/')` on `"a//c/d/rest"` yields ["a", "", "c", "d", "rest"];
    // the empty caller_core_node at index 1 must be rejected.
    let err = ZenohWireFormat::parse_topic_keyexpr("a//c/d/rest").unwrap_err();
    assert!(matches!(
        err,
        ZenohWireParseError::MissingSegment("caller_core_node")
    ));
}

#[test]
fn parse_topic_keyexpr_missing_instance_id_errors() {
    let err = ZenohWireFormat::parse_topic_keyexpr("a/b/c").unwrap_err();
    assert!(matches!(
        err,
        ZenohWireParseError::MissingSegment("caller_instance_id")
    ));
}

#[test]
fn parse_topic_keyexpr_empty_instance_id_errors() {
    let err = ZenohWireFormat::parse_topic_keyexpr("a/b/c//rest").unwrap_err();
    assert!(matches!(
        err,
        ZenohWireParseError::MissingSegment("caller_instance_id")
    ));
}

#[test]
fn parse_topic_keyexpr_rejects_wildcard_in_caller_core_node() {
    // A bare `*` at the caller-core position is never produced by topic_publish
    // (Segment validation forbids it). Surfacing it as a real address would be
    // a silent protocol violation, so parse must reject it.
    let err = ZenohWireFormat::parse_topic_keyexpr("a/*/c/d/rest").unwrap_err();
    assert!(matches!(
        err,
        ZenohWireParseError::WildcardInCallerSegment("caller_core_node")
    ));
}

#[test]
fn parse_topic_keyexpr_rejects_wildcard_in_caller_instance_id() {
    let err = ZenohWireFormat::parse_topic_keyexpr("a/b/c/*/rest").unwrap_err();
    assert!(matches!(
        err,
        ZenohWireParseError::WildcardInCallerSegment("caller_instance_id")
    ));
}

// ─── Services — listen patterns ───────────────────────────────────────────

fn sample_service_receiver(kind: ServiceKind) -> ServiceWireReceiver {
    ServiceWireReceiver {
        bound_core_node: seg("server_core"),
        as_instance_id: seg("server_inst"),
        as_node_name: seg("robot_arm"),
        iface: Iface::native(),
        as_service_name: seg("ping"),
        kind,
    }
}

#[test]
fn service_listen_patterns_native_iface_plain_service() {
    let recv = sample_service_receiver(ServiceKind::Service);
    let patterns = ZenohWireFormat::service_listen_patterns(&recv);
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
    recv.as_service_name = seg("pick_place");
    let patterns = ZenohWireFormat::service_listen_patterns(&recv);
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
    recv.iface = Iface::new("manipulator", "v2-beta").expect("valid iface");
    let patterns = ZenohWireFormat::service_listen_patterns(&recv);
    assert_eq!(
        patterns[0],
        "server_core/*/server_inst/*/service/robot_arm/manipulator/v2_beta/ping/request/**"
    );
}

// ─── Services — request publish ───────────────────────────────────────────

fn sample_service_sender(kind: ServiceKind) -> ServiceWireSender {
    ServiceWireSender {
        bound_core_node: seg("caller_core"),
        as_instance_id: seg("caller_inst"),
        to_core_node: Some(seg("target_core")),
        to_instance_id: Some(seg("target_inst")),
        to_node_name: seg("robot_arm"),
        iface: Iface::native(),
        to_service_name: seg("ping"),
        kind,
    }
}

#[test]
fn service_request_publish_specific_target() {
    let sender = sample_service_sender(ServiceKind::Service);
    assert_eq!(
        ZenohWireFormat::service_request_publish(&sender, "abc123"),
        "target_core/caller_core/target_inst/caller_inst/service/robot_arm/_/_/ping/request/abc123"
    );
}

#[test]
fn service_request_publish_broadcast_instance() {
    let mut sender = sample_service_sender(ServiceKind::Service);
    sender.to_instance_id = None;
    assert_eq!(
        ZenohWireFormat::service_request_publish(&sender, "abc123"),
        "target_core/caller_core/_any_/caller_inst/service/robot_arm/_/_/ping/request/abc123"
    );
}

#[test]
fn service_request_publish_broadcast_core() {
    let mut sender = sample_service_sender(ServiceKind::Service);
    sender.to_core_node = None;
    assert_eq!(
        ZenohWireFormat::service_request_publish(&sender, "abc123"),
        "_any_/caller_core/target_inst/caller_inst/service/robot_arm/_/_/ping/request/abc123"
    );
}

#[test]
fn service_request_publish_full_broadcast() {
    let mut sender = sample_service_sender(ServiceKind::Service);
    sender.to_core_node = None;
    sender.to_instance_id = None;
    assert_eq!(
        ZenohWireFormat::service_request_publish(&sender, "abc123"),
        "_any_/caller_core/_any_/caller_inst/service/robot_arm/_/_/ping/request/abc123"
    );
}

#[test]
fn service_request_publish_action_goal() {
    let mut sender = sample_service_sender(ServiceKind::ActionGoal);
    sender.to_service_name = seg("pick_place");
    assert_eq!(
        ZenohWireFormat::service_request_publish(&sender, "rid_42"),
        "target_core/caller_core/target_inst/caller_inst/action/robot_arm/_/_/pick_place/goal/request/rid_42"
    );
}

// ─── Services — response subscribe ────────────────────────────────────────

#[test]
fn service_response_subscribe_native_iface() {
    let sender = sample_service_sender(ServiceKind::Service);
    assert_eq!(
        ZenohWireFormat::service_response_subscribe(&sender, "abc123"),
        "caller_core/*/caller_inst/*/service/robot_arm/_/_/ping/response/abc123"
    );
}

#[test]
fn service_response_subscribe_action_result() {
    let mut sender = sample_service_sender(ServiceKind::ActionResult);
    sender.to_service_name = seg("pick_place");
    sender.iface = Iface::new("manipulator", "v1").expect("valid iface");
    assert_eq!(
        ZenohWireFormat::service_response_subscribe(&sender, "rid_42"),
        "caller_core/*/caller_inst/*/action/robot_arm/manipulator/v1/pick_place/result/response/rid_42"
    );
}

// ─── Services — parse_received_request (server-side response publish) ─────

#[test]
fn parse_received_request_round_trips_specific() {
    let receiver = sample_service_receiver(ServiceKind::Service);
    let request =
        "server_core/caller_core/server_inst/caller_inst/service/robot_arm/_/_/ping/request/abc123";
    let parsed = ZenohWireFormat::parse_received_request(&receiver, request).expect("should parse");
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
    let parsed = ZenohWireFormat::parse_received_request(&receiver, request).expect("should parse");
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
    let err = ZenohWireFormat::parse_received_request(&receiver, request).unwrap_err();
    assert!(matches!(
        err,
        ZenohWireParseError::ServiceRootMismatch { .. }
    ));
}

#[test]
fn parse_received_request_rejects_missing_request_marker() {
    let receiver = sample_service_receiver(ServiceKind::Service);
    let request =
        "server_core/caller_core/server_inst/caller_inst/service/robot_arm/_/_/ping/wrong/abc";
    let err = ZenohWireFormat::parse_received_request(&receiver, request).unwrap_err();
    assert_eq!(err, ZenohWireParseError::NotARequest);
}

#[test]
fn parse_received_request_rejects_trailing_segments() {
    let receiver = sample_service_receiver(ServiceKind::Service);
    let request = "server_core/caller_core/server_inst/caller_inst/service/robot_arm/_/_/ping/request/abc/extra";
    let err = ZenohWireFormat::parse_received_request(&receiver, request).unwrap_err();
    assert_eq!(err, ZenohWireParseError::UnexpectedTrailing);
}

#[test]
fn parse_received_request_rejects_too_short() {
    let receiver = sample_service_receiver(ServiceKind::Service);
    let request = "only/two/segments";
    let err = ZenohWireFormat::parse_received_request(&receiver, request).unwrap_err();
    assert!(matches!(err, ZenohWireParseError::MissingSegment(_)));
}

#[test]
fn parse_received_request_action_cancel_round_trips() {
    let mut receiver = sample_service_receiver(ServiceKind::ActionCancel);
    receiver.as_service_name = seg("pick_place");
    let request = "server_core/caller_core/server_inst/caller_inst/action/robot_arm/_/_/pick_place/cancel/request/rid_42";
    let parsed = ZenohWireFormat::parse_received_request(&receiver, request).expect("should parse");
    assert_eq!(parsed.request_id, "rid_42");
    assert_eq!(
        parsed.response_keyexpr,
        "caller_core/server_core/caller_inst/server_inst/action/robot_arm/_/_/pick_place/cancel/response/rid_42"
    );
}

// ─── Actions — feedback ───────────────────────────────────────────────────

fn sample_action_receiver() -> ActionWireReceiver {
    ActionWireReceiver {
        bound_core_node: seg("server_core"),
        as_instance_id: seg("server_inst"),
        as_node_name: seg("robot_arm"),
        iface: Iface::native(),
        as_action_name: seg("pick_place"),
    }
}

fn sample_action_sender() -> ActionWireSender {
    ActionWireSender {
        as_core_node: seg("client_core"),
        as_instance_id: seg("client_inst"),
        to_core_node: Some(seg("server_core")),
        to_instance_id: Some(seg("server_inst")),
        to_node_name: seg("robot_arm"),
        iface: Iface::native(),
        to_action_name: seg("pick_place"),
    }
}

#[test]
fn action_feedback_publish_native_iface() {
    let recv = sample_action_receiver();
    assert_eq!(
        ZenohWireFormat::action_feedback_publish(&recv, "goal_xyz"),
        "*/server_core/*/server_inst/action/robot_arm/_/_/pick_place/feedback/server_inst/goal_xyz"
    );
}

#[test]
fn action_feedback_publish_normalizes_iface_tag() {
    let mut recv = sample_action_receiver();
    recv.iface = Iface::new("manipulator", "v1-rc1").expect("valid iface");
    assert_eq!(
        ZenohWireFormat::action_feedback_publish(&recv, "goal_xyz"),
        "*/server_core/*/server_inst/action/robot_arm/manipulator/v1_rc1/pick_place/feedback/server_inst/goal_xyz"
    );
}

#[test]
fn action_feedback_subscribe_targeted() {
    let sender = sample_action_sender();
    assert_eq!(
        ZenohWireFormat::action_feedback_subscribe(&sender, "goal_xyz"),
        "client_core/server_core/client_inst/server_inst/action/robot_arm/_/_/pick_place/feedback/server_inst/goal_xyz"
    );
}

#[test]
fn action_feedback_subscribe_untargeted() {
    let mut sender = sample_action_sender();
    sender.to_core_node = None;
    sender.to_instance_id = None;
    assert_eq!(
        ZenohWireFormat::action_feedback_subscribe(&sender, "goal_xyz"),
        "client_core/*/client_inst/*/action/robot_arm/_/_/pick_place/feedback/*/goal_xyz"
    );
}

#[test]
fn action_feedback_subscribe_partial_target_uses_wildcard_only_for_missing() {
    let mut sender = sample_action_sender();
    sender.to_core_node = Some(seg("server_core"));
    sender.to_instance_id = None;
    assert_eq!(
        ZenohWireFormat::action_feedback_subscribe(&sender, "goal_xyz"),
        "client_core/server_core/client_inst/*/action/robot_arm/_/_/pick_place/feedback/*/goal_xyz"
    );
}
