//! Snapshot tests for the zenoh wire format functions. These pin the exact
//! keyexpr strings produced for every (role × variant) combination — if a
//! change shifts the bytes, the test catches it. Roundtrip tests against a
//! real router live in `crates/pmi-internal/tests/wire.rs`.

use super::*;
use crate::wire::{Segment, SenderTarget};

/// Test-local shorthand: wrap a `&str` in a validated [`Segment`]. Panics on
/// invalid input — tests use known-good values only.
fn seg(value: &str) -> Segment {
    Segment::try_from(value).expect("test segment value should be valid")
}

/// Test-local shorthand: build a node-shaped target. Panics on invalid input.
fn node(name: &str, tag: &str) -> SenderTarget {
    SenderTarget::node(name, tag).expect("test node target should be valid")
}

/// Test-local shorthand: build an interface-shaped target. Panics on invalid input.
fn iface(name: &str, tag: &str) -> SenderTarget {
    SenderTarget::interface(name, tag).expect("test interface target should be valid")
}

/// Test-local shorthand: encoded bytes for a default UserRequest query
/// attachment (no exclusion set). Lets selector-shape tests focus on
/// keyexpr parsing without restating the attachment payload at every
/// call site.
fn user_request_attachment_bytes() -> bytes::Bytes {
    ServiceQueryAttachment {
        kind: ServiceQueryKind::UserRequest,
    }
    .encode()
}

// ─── Topics ───────────────────────────────────────────────────────────────

#[test]
fn topic_publish_node_target() {
    let sender = TopicWireSender {
        as_core_node: seg("core_node_a"),
        as_instance_id: seg("publisher_inst"),
        as_target: node("uvc_camera", "v1"),
        link_id: Segment::default_link_id(),
        as_topic_name: seg("video_stream"),
    };
    assert_eq!(
        ZenohWireFormat::topic_publish(&sender),
        "*/core_node_a/*/publisher_inst/topic/node/uvc_camera/v1/_/video_stream"
    );
}

#[test]
fn topic_publish_with_interface_normalizes_tag() {
    let sender = TopicWireSender {
        as_core_node: seg("core_a"),
        as_instance_id: seg("inst_1"),
        as_target: iface("manipulator", "v1-beta-2"),
        link_id: Segment::default_link_id(),
        as_topic_name: seg("joint_states"),
    };
    assert_eq!(
        ZenohWireFormat::topic_publish(&sender),
        "*/core_a/*/inst_1/topic/interface/manipulator/v1_beta_2/_/joint_states"
    );
}

#[test]
fn topic_publish_with_concrete_link_id() {
    let sender = TopicWireSender {
        as_core_node: seg("core_a"),
        as_instance_id: seg("inst_1"),
        as_target: iface("depth_camera", "v1"),
        link_id: seg("wrist_left_camera"),
        as_topic_name: seg("video_stream"),
    };
    assert_eq!(
        ZenohWireFormat::topic_publish(&sender),
        "*/core_a/*/inst_1/topic/interface/depth_camera/v1/wrist_left_camera/video_stream"
    );
}

#[test]
fn topic_subscribe_targeted_node() {
    let receiver = TopicWireReceiver {
        as_core_node: seg("core_subscriber"),
        as_instance_id: seg("sub_inst"),
        from_core_node: Some(seg("core_publisher")),
        from_instance_id: Some(seg("pub_inst")),
        from_target: Some(node("uvc_camera", "v1")),
        from_link_id: None,
        to_topic: seg("video_stream"),
        defers_secondary_drop: false,
    };
    assert_eq!(
        ZenohWireFormat::topic_subscribe(&receiver),
        "core_subscriber/core_publisher/sub_inst/pub_inst/topic/node/uvc_camera/v1/*/video_stream"
    );
}

#[test]
fn topic_subscribe_with_concrete_link_id() {
    let receiver = TopicWireReceiver {
        as_core_node: seg("core_subscriber"),
        as_instance_id: seg("sub_inst"),
        from_core_node: None,
        from_instance_id: None,
        from_target: Some(iface("depth_camera", "v1")),
        from_link_id: Some(seg("wrist_left_camera")),
        to_topic: seg("video_stream"),
        defers_secondary_drop: false,
    };
    assert_eq!(
        ZenohWireFormat::topic_subscribe(&receiver),
        "core_subscriber/*/sub_inst/*/topic/interface/depth_camera/v1/wrist_left_camera/video_stream"
    );
}

#[test]
fn topic_subscribe_untargeted_publisher_core_uses_wildcard() {
    let receiver = TopicWireReceiver {
        as_core_node: seg("core_subscriber"),
        as_instance_id: seg("sub_inst"),
        from_core_node: None,
        from_instance_id: None,
        from_target: Some(node("uvc_camera", "v1")),
        from_link_id: None,
        to_topic: seg("video_stream"),
        defers_secondary_drop: false,
    };
    assert_eq!(
        ZenohWireFormat::topic_subscribe(&receiver),
        "core_subscriber/*/sub_inst/*/topic/node/uvc_camera/v1/*/video_stream"
    );
}

#[test]
fn topic_subscribe_interface_target() {
    let receiver = TopicWireReceiver {
        as_core_node: seg("core_subscriber"),
        as_instance_id: seg("sub_inst"),
        from_core_node: Some(seg("core_publisher")),
        from_instance_id: None,
        from_target: Some(iface("manipulator", "v1")),
        from_link_id: None,
        to_topic: seg("joint_states"),
        defers_secondary_drop: false,
    };
    assert_eq!(
        ZenohWireFormat::topic_subscribe(&receiver),
        "core_subscriber/core_publisher/sub_inst/*/topic/interface/manipulator/v1/*/joint_states"
    );
}

#[test]
fn topic_subscribe_fully_untargeted_wildcards_all_slots() {
    let receiver = TopicWireReceiver {
        as_core_node: seg("core_subscriber"),
        as_instance_id: seg("sub_inst"),
        from_core_node: None,
        from_instance_id: None,
        from_target: None,
        from_link_id: None,
        to_topic: seg("video_stream"),
        defers_secondary_drop: false,
    };
    assert_eq!(
        ZenohWireFormat::topic_subscribe(&receiver),
        "core_subscriber/*/sub_inst/*/topic/*/*/*/*/video_stream"
    );
}

// ─── Topics — parse ────────────────────────────────────────────────────────

#[test]
fn parse_topic_keyexpr_extracts_caller_addressing() {
    let key = "core_subscriber/publisher_core/sub_inst/publisher_inst/topic/node/sensor_node/v1/_/temperature";
    let parsed = ZenohWireFormat::parse_topic_keyexpr(key).expect("should parse");
    assert_eq!(parsed.core_node, "publisher_core");
    assert_eq!(parsed.instance_id, "publisher_inst");
}

#[test]
fn parse_topic_keyexpr_roundtrips_through_topic_publish() {
    let sender = TopicWireSender {
        as_core_node: seg("core_a"),
        as_instance_id: seg("inst_1"),
        as_target: node("sensor", "v1"),
        link_id: Segment::default_link_id(),
        as_topic_name: seg("humidity"),
    };
    let key = ZenohWireFormat::topic_publish(&sender);
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

// ─── Services — queryable declare ─────────────────────────────────────────

fn sample_service_receiver(kind: ServiceKind) -> ServiceWireReceiver {
    ServiceWireReceiver::new(
        "server_core",
        "server_inst",
        node("robot_arm", "v1"),
        "ping",
        kind,
    )
    .expect("valid sample service receiver")
}

#[test]
fn service_queryable_declare_node_identity_plain_service() {
    // One queryable per `listen_service`. The link_id slot is `*` so the
    // queryable absorbs both `*` (from `from_any` consumers) and `_` (the
    // default literal post-binding-map) at that wire slot.
    let recv = sample_service_receiver(ServiceKind::Service);
    let key = ZenohWireFormat::service_queryable_declare(&recv);
    assert_eq!(
        key,
        "server_core/*/server_inst/*/service/node/robot_arm/v1/*/ping"
    );
}

#[test]
fn service_queryable_declare_action_goal_appends_suffix() {
    let recv = ServiceWireReceiver::new(
        "server_core",
        "server_inst",
        node("robot_arm", "v1"),
        "pick_place",
        ServiceKind::ActionGoal,
    )
    .expect("valid receiver");
    assert_eq!(
        ZenohWireFormat::service_queryable_declare(&recv),
        "server_core/*/server_inst/*/action/node/robot_arm/v1/*/pick_place/goal"
    );
}

#[test]
fn service_queryable_declare_interface_identity_normalizes_tag() {
    let recv = ServiceWireReceiver::new(
        "server_core",
        "server_inst",
        iface("manipulator", "v2-beta"),
        "ping",
        ServiceKind::Service,
    )
    .expect("valid receiver");
    assert_eq!(
        ZenohWireFormat::service_queryable_declare(&recv),
        "server_core/*/server_inst/*/service/interface/manipulator/v2_beta/*/ping"
    );
}

// ─── Services — get selector ──────────────────────────────────────────────

fn sample_service_sender(kind: ServiceKind) -> ServiceWireSender {
    ServiceWireSender {
        bound_core_node: seg("caller_core"),
        as_instance_id: seg("caller_inst"),
        target_core_node: Some(seg("target_core")),
        target_instance_id: Some(seg("target_inst")),
        to_target: node("robot_arm", "v1"),
        to_service_name: seg("ping"),
        kind,
    }
}

#[test]
fn service_get_selector_specific_target() {
    let sender = sample_service_sender(ServiceKind::Service);
    assert_eq!(
        ZenohWireFormat::service_get_selector(&sender),
        "target_core/caller_core/target_inst/caller_inst/service/node/robot_arm/v1/*/ping"
    );
}

#[test]
fn service_get_selector_broadcast_instance() {
    // `target_instance_id: None` becomes the Zenoh single-chunk wildcard, not
    // the legacy `_any_` literal — `session.get` accepts wildcards.
    let mut sender = sample_service_sender(ServiceKind::Service);
    sender.target_instance_id = None;
    assert_eq!(
        ZenohWireFormat::service_get_selector(&sender),
        "target_core/caller_core/*/caller_inst/service/node/robot_arm/v1/*/ping"
    );
}

#[test]
fn service_get_selector_broadcast_core() {
    let mut sender = sample_service_sender(ServiceKind::Service);
    sender.target_core_node = None;
    assert_eq!(
        ZenohWireFormat::service_get_selector(&sender),
        "*/caller_core/target_inst/caller_inst/service/node/robot_arm/v1/*/ping"
    );
}

#[test]
fn service_get_selector_full_broadcast() {
    let mut sender = sample_service_sender(ServiceKind::Service);
    sender.target_core_node = None;
    sender.target_instance_id = None;
    assert_eq!(
        ZenohWireFormat::service_get_selector(&sender),
        "*/caller_core/*/caller_inst/service/node/robot_arm/v1/*/ping"
    );
}

#[test]
fn service_get_selector_scoped_to_core_node() {
    // The core-without-instance half-address: `scoped_to_core_node` pins the
    // target core slot and wildcards the instance slot, regardless of what
    // the sender carried before.
    let sender = sample_service_sender(ServiceKind::Service)
        .scoped_to_core_node("other_core")
        .expect("valid core node segment");
    assert_eq!(
        ZenohWireFormat::service_get_selector(&sender),
        "other_core/caller_core/*/caller_inst/service/node/robot_arm/v1/*/ping"
    );
}

#[test]
fn service_get_selector_always_wildcards_link_id_slot() {
    // The link_id wire slot is always `*` — producers advertise under `_`
    // and Zenoh's matcher unifies the two. There is no caller-side knob to
    // pin a concrete literal.
    let sender = sample_service_sender(ServiceKind::Service);
    let key = ZenohWireFormat::service_get_selector(&sender);
    assert!(
        key.contains("/robot_arm/v1/*/ping"),
        "selector must wildcard the link_id slot: {key}"
    );
    assert!(
        !key.contains("/robot_arm/v1/_/ping"),
        "selector must NOT pin the default link_id literal: {key}"
    );
}

#[test]
fn service_get_selector_action_goal() {
    let mut sender = sample_service_sender(ServiceKind::ActionGoal);
    sender.to_service_name = seg("pick_place");
    assert_eq!(
        ZenohWireFormat::service_get_selector(&sender),
        "target_core/caller_core/target_inst/caller_inst/action/node/robot_arm/v1/*/pick_place/goal"
    );
}

// ─── Services — reply keyexpr ─────────────────────────────────────────────

#[test]
fn service_reply_keyexpr_topic_shape_addresses_caller() {
    // The reply keyexpr is topic-shape (caller at segments 0/2, responder
    // at segments 1/3) so the caller's `parse_topic_keyexpr` surfaces the
    // responder's identity via `Message::core_node()` / `instance_id()`.
    let receiver = sample_service_receiver(ServiceKind::Service);
    assert_eq!(
        ZenohWireFormat::service_reply_keyexpr(&receiver, "_", "caller_core", "caller_inst"),
        "caller_core/server_core/caller_inst/server_inst/service/node/robot_arm/v1/_/ping"
    );
}

#[test]
fn service_reply_keyexpr_action_result_appends_suffix() {
    let receiver = ServiceWireReceiver::new(
        "server_core",
        "server_inst",
        iface("manipulator", "v1"),
        "pick_place",
        ServiceKind::ActionResult,
    )
    .expect("valid receiver");
    assert_eq!(
        ZenohWireFormat::service_reply_keyexpr(&receiver, "_", "caller_core", "caller_inst"),
        "caller_core/server_core/caller_inst/server_inst/action/interface/manipulator/v1/_/pick_place/result"
    );
}

// ─── Services — parse_inbound_query ───────────────────────────────────────

#[test]
fn parse_inbound_query_extracts_caller_identity_and_literal_link_id() {
    let receiver = sample_service_receiver(ServiceKind::Service);
    let query = "server_core/caller_core/server_inst/caller_inst/service/node/robot_arm/v1/_/ping";
    let parsed =
        ZenohWireFormat::parse_inbound_query(&receiver, query, &user_request_attachment_bytes())
            .expect("should parse");
    assert_eq!(parsed.caller_core, "caller_core");
    assert_eq!(parsed.caller_inst, "caller_inst");
    assert_eq!(
        parsed.link_id, "_",
        "default-link-id producer selector should surface `_` literal"
    );
    assert_eq!(parsed.kind, ServiceQueryKind::UserRequest);
}

#[test]
fn parse_inbound_query_accepts_wildcard_in_target_slots_and_link_id() {
    // A `from_any` consumer's selector wildcards the to_core/to_inst slots
    // and the link_id slot with Zenoh `*`. The producer-side parser
    // surfaces the `*` at the link_id slot so the adapter's dispatcher can
    // claim a bound link_id; the to_core/to_inst slots are ignored here
    // because Zenoh keyexpr matching has already routed the query.
    let receiver = sample_service_receiver(ServiceKind::Service);
    let query = "*/caller_core/*/caller_inst/service/node/robot_arm/v1/*/ping";
    let parsed =
        ZenohWireFormat::parse_inbound_query(&receiver, query, &user_request_attachment_bytes())
            .expect("should parse");
    assert_eq!(parsed.caller_core, "caller_core");
    assert_eq!(parsed.caller_inst, "caller_inst");
    assert_eq!(
        parsed.link_id, "*",
        "from_any consumer's selector should surface `*` at the link_id slot"
    );
}

#[test]
fn parse_inbound_query_surfaces_probe_kind_from_attachment() {
    // The probe kind discriminator lives on the attachment. A user payload
    // that happens to start with the legacy probe sentinel bytes no longer
    // reaches this layer — discrimination happens entirely on the
    // attachment, not the payload.
    let receiver = sample_service_receiver(ServiceKind::Service);
    let query = "*/caller_core/*/caller_inst/service/node/robot_arm/v1/*/ping";
    let probe_attachment = ServiceQueryAttachment {
        kind: ServiceQueryKind::Probe,
    }
    .encode();
    let parsed = ZenohWireFormat::parse_inbound_query(&receiver, query, &probe_attachment)
        .expect("should parse");
    assert_eq!(parsed.kind, ServiceQueryKind::Probe);
}

#[test]
fn parse_inbound_query_rejects_missing_attachment_with_structured_error() {
    // Mid-rollout safety: a peer running the old protocol sends queries
    // with no attachment. The producer must surface the structured error
    // rather than silently treating the query as a default UserRequest —
    // dropping the query is what triggers the consumer's ServiceUnreachable.
    let receiver = sample_service_receiver(ServiceKind::Service);
    let query = "server_core/caller_core/server_inst/caller_inst/service/node/robot_arm/v1/_/ping";
    let err = ZenohWireFormat::parse_inbound_query(&receiver, query, &[]).unwrap_err();
    assert!(matches!(
        err,
        ZenohWireParseError::MissingServiceQueryAttachment
    ));
}

#[test]
fn parse_inbound_query_rejects_v1_magic_with_structured_error() {
    // Old-protocol attachment bytes (V1 magic) must produce a structured
    // mismatch error so mid-rollout skew is loud, not silent.
    let receiver = sample_service_receiver(ServiceKind::Service);
    let query = "server_core/caller_core/server_inst/caller_inst/service/node/robot_arm/v1/_/ping";
    let v1_bytes: &[u8] = &[0x01, 0x00];
    let err = ZenohWireFormat::parse_inbound_query(&receiver, query, v1_bytes).unwrap_err();
    assert!(matches!(
        err,
        ZenohWireParseError::ServiceQueryAttachmentMagicMismatch { .. }
    ));
}

#[test]
fn parse_inbound_query_rejects_service_root_mismatch() {
    let receiver = sample_service_receiver(ServiceKind::Service);
    let query =
        "server_core/caller_core/server_inst/caller_inst/service/node/different_node/v1/_/ping";
    let err =
        ZenohWireFormat::parse_inbound_query(&receiver, query, &user_request_attachment_bytes())
            .unwrap_err();
    assert!(matches!(
        err,
        ZenohWireParseError::ServiceRootMismatch { .. }
    ));
}

#[test]
fn parse_inbound_query_rejects_discriminator_mismatch() {
    // Receiver expects `node/robot_arm/v1`; query uses `interface/robot_arm/v1`.
    // The wire format's service_root parse catches the collision.
    let receiver = sample_service_receiver(ServiceKind::Service);
    let query =
        "server_core/caller_core/server_inst/caller_inst/service/interface/robot_arm/v1/_/ping";
    let err =
        ZenohWireFormat::parse_inbound_query(&receiver, query, &user_request_attachment_bytes())
            .unwrap_err();
    assert!(matches!(
        err,
        ZenohWireParseError::ServiceRootMismatch { .. }
    ));
}

#[test]
fn parse_inbound_query_rejects_too_short() {
    let receiver = sample_service_receiver(ServiceKind::Service);
    let query = "only/two/segments";
    let err =
        ZenohWireFormat::parse_inbound_query(&receiver, query, &user_request_attachment_bytes())
            .unwrap_err();
    assert!(matches!(err, ZenohWireParseError::MissingSegment(_)));
}

// ─── Actions — feedback ───────────────────────────────────────────────────

fn sample_action_receiver() -> ActionWireReceiver {
    ActionWireReceiver {
        bound_core_node: seg("server_core"),
        as_instance_id: seg("server_inst"),
        as_identity: node("robot_arm", "v1"),
        as_action_name: seg("pick_place"),
    }
}

fn sample_action_sender() -> ActionWireSender {
    ActionWireSender {
        as_core_node: seg("client_core"),
        as_instance_id: seg("client_inst"),
        target_core_node: Some(seg("server_core")),
        target_instance_id: Some(seg("server_inst")),
        to_target: node("robot_arm", "v1"),
        to_action_name: seg("pick_place"),
    }
}

#[test]
fn action_feedback_publish_node_identity() {
    let recv = sample_action_receiver();
    assert_eq!(
        ZenohWireFormat::action_feedback_publish(&recv, "_", "goal_xyz"),
        "*/server_core/*/server_inst/action/node/robot_arm/v1/_/pick_place/feedback/server_inst/goal_xyz"
    );
}

#[test]
fn action_feedback_publish_normalizes_interface_tag() {
    let mut recv = sample_action_receiver();
    recv.as_identity = iface("manipulator", "v1-rc1");
    assert_eq!(
        ZenohWireFormat::action_feedback_publish(&recv, "_", "goal_xyz"),
        "*/server_core/*/server_inst/action/interface/manipulator/v1_rc1/_/pick_place/feedback/server_inst/goal_xyz"
    );
}

#[test]
fn action_feedback_subscribe_targeted() {
    let sender = sample_action_sender();
    assert_eq!(
        ZenohWireFormat::action_feedback_subscribe(&sender, "goal_xyz"),
        "client_core/server_core/client_inst/server_inst/action/node/robot_arm/v1/*/pick_place/feedback/server_inst/goal_xyz"
    );
}

#[test]
fn action_feedback_subscribe_untargeted() {
    let mut sender = sample_action_sender();
    sender.target_core_node = None;
    sender.target_instance_id = None;
    assert_eq!(
        ZenohWireFormat::action_feedback_subscribe(&sender, "goal_xyz"),
        "client_core/*/client_inst/*/action/node/robot_arm/v1/*/pick_place/feedback/*/goal_xyz"
    );
}

#[test]
fn action_feedback_subscribe_partial_target_uses_wildcard_only_for_missing() {
    let mut sender = sample_action_sender();
    sender.target_core_node = Some(seg("server_core"));
    sender.target_instance_id = None;
    assert_eq!(
        ZenohWireFormat::action_feedback_subscribe(&sender, "goal_xyz"),
        "client_core/server_core/client_inst/*/action/node/robot_arm/v1/*/pick_place/feedback/*/goal_xyz"
    );
}

// ─── Collision safety: node vs interface with overlapping name+tag ────────
//
// Core property the refactor exists to guarantee: a `NodeIdentifier { name, tag }`
// must never share a wire key with an `InterfaceIdentifier { name, tag }` even
// when both carry the same `name` and `tag`. The wire format embeds an
// `interface` | `node` discriminator immediately before the name/tag pair to
// prevent the two identifier namespaces from colliding.

#[test]
fn topic_publish_distinguishes_node_and_interface_with_same_name_tag() {
    let common = TopicWireSender {
        as_core_node: seg("core_a"),
        as_instance_id: seg("inst_1"),
        // Replaced per-case below.
        as_target: node("placeholder", "v1"),
        link_id: Segment::default_link_id(),
        as_topic_name: seg("frames"),
    };
    let mut as_node = common.clone();
    as_node.as_target = node("widget", "v1");
    let mut as_iface = common;
    as_iface.as_target = iface("widget", "v1");

    let node_key = ZenohWireFormat::topic_publish(&as_node);
    let iface_key = ZenohWireFormat::topic_publish(&as_iface);

    assert_ne!(node_key, iface_key);
    assert!(
        node_key.contains("/topic/node/widget/v1/_/"),
        "node-shaped publish should carry the `node` discriminator: {node_key}"
    );
    assert!(
        iface_key.contains("/topic/interface/widget/v1/_/"),
        "interface-shaped publish should carry the `interface` discriminator: {iface_key}"
    );
}

#[test]
fn service_get_selector_distinguishes_node_and_interface_with_same_name_tag() {
    let base = ServiceWireSender {
        bound_core_node: seg("caller_core"),
        as_instance_id: seg("caller_inst"),
        target_core_node: Some(seg("target_core")),
        target_instance_id: Some(seg("target_inst")),
        to_target: node("placeholder", "v1"),
        to_service_name: seg("ping"),
        kind: ServiceKind::Service,
    };
    let mut as_node = base.clone();
    as_node.to_target = node("widget", "v1");
    let mut as_iface = base;
    as_iface.to_target = iface("widget", "v1");

    let node_key = ZenohWireFormat::service_get_selector(&as_node);
    let iface_key = ZenohWireFormat::service_get_selector(&as_iface);

    assert_ne!(node_key, iface_key);
    assert!(node_key.contains("/service/node/widget/v1/*/ping"));
    assert!(iface_key.contains("/service/interface/widget/v1/*/ping"));
}

#[test]
fn action_feedback_distinguishes_node_and_interface_with_same_name_tag() {
    let base = ActionWireReceiver {
        bound_core_node: seg("server_core"),
        as_instance_id: seg("server_inst"),
        as_identity: node("placeholder", "v1"),
        as_action_name: seg("pick_place"),
    };
    let mut as_node = base.clone();
    as_node.as_identity = node("widget", "v1");
    let mut as_iface = base;
    as_iface.as_identity = iface("widget", "v1");

    let node_key = ZenohWireFormat::action_feedback_publish(&as_node, "_", "goal_xyz");
    let iface_key = ZenohWireFormat::action_feedback_publish(&as_iface, "_", "goal_xyz");

    assert_ne!(node_key, iface_key);
    assert!(node_key.contains("/action/node/widget/v1/_/pick_place/"));
    assert!(iface_key.contains("/action/interface/widget/v1/_/pick_place/"));
}

#[test]
fn topic_subscribe_node_only_segment_does_not_match_interface_publisher() {
    // A subscriber that pins on a node target should ONLY match publishers
    // emitting under the node discriminator, not interface publishers with the
    // same name+tag. The discriminator literal in the keyexpr enforces this:
    // `topic/node/widget/v1/...` and `topic/interface/widget/v1/...` differ on
    // a literal segment (no wildcard intersection).
    let receiver = TopicWireReceiver {
        as_core_node: seg("core_a"),
        as_instance_id: seg("inst_1"),
        from_core_node: None,
        from_instance_id: None,
        from_target: Some(node("widget", "v1")),
        from_link_id: None,
        to_topic: seg("frames"),
        defers_secondary_drop: false,
    };
    let publisher_as_node = TopicWireSender {
        as_core_node: seg("core_a"),
        as_instance_id: seg("inst_2"),
        as_target: node("widget", "v1"),
        link_id: Segment::default_link_id(),
        as_topic_name: seg("frames"),
    };
    let publisher_as_iface = TopicWireSender {
        as_core_node: seg("core_a"),
        as_instance_id: seg("inst_2"),
        as_target: iface("widget", "v1"),
        link_id: Segment::default_link_id(),
        as_topic_name: seg("frames"),
    };

    let sub_key = ZenohWireFormat::topic_subscribe(&receiver);
    let node_pub_key = ZenohWireFormat::topic_publish(&publisher_as_node);
    let iface_pub_key = ZenohWireFormat::topic_publish(&publisher_as_iface);

    // The subscriber keyexpr has `topic/node/widget/v1/*/frames` in its tail.
    // The node-shaped publisher key matches segment-by-segment (after the
    // per-`*` caller-side wildcards) while the interface-shaped one differs
    // on the discriminator literal. We verify by checking the discriminator
    // segments appear distinct in the rendered strings.
    assert!(sub_key.contains("/topic/node/widget/v1/*/frames"));
    assert!(node_pub_key.contains("/topic/node/widget/v1/_/frames"));
    assert!(iface_pub_key.contains("/topic/interface/widget/v1/_/frames"));
    // Cross-check: the iface publisher's tail must NOT appear inside the
    // node-pinned subscriber's keyexpr.
    assert!(!sub_key.contains("/topic/interface/"));
}

#[test]
fn topic_subscribe_interface_only_segment_does_not_match_node_publisher() {
    // Mirror of the previous test: a subscriber pinned on an interface target
    // must not match a node-shaped publisher with the same name+tag.
    let receiver = TopicWireReceiver {
        as_core_node: seg("core_a"),
        as_instance_id: seg("inst_1"),
        from_core_node: None,
        from_instance_id: None,
        from_target: Some(iface("widget", "v1")),
        from_link_id: None,
        to_topic: seg("frames"),
        defers_secondary_drop: false,
    };
    let sub_key = ZenohWireFormat::topic_subscribe(&receiver);
    assert!(sub_key.contains("/topic/interface/widget/v1/*/frames"));
    assert!(!sub_key.contains("/topic/node/"));
}

#[test]
fn topic_subscribe_untargeted_wildcards_discriminator_too() {
    // `from_target: None` must wildcard all three target segments
    // (discriminator + name + tag) so a subscriber matches both node-shaped
    // and interface-shaped publishers. Without this, an untargeted subscriber
    // would silently miss one of the two namespaces.
    let receiver = TopicWireReceiver {
        as_core_node: seg("core_a"),
        as_instance_id: seg("inst_1"),
        from_core_node: None,
        from_instance_id: None,
        from_target: None,
        from_link_id: None,
        to_topic: seg("frames"),
        defers_secondary_drop: false,
    };
    let key = ZenohWireFormat::topic_subscribe(&receiver);
    assert!(
        key.contains("/topic/*/*/*/*/frames"),
        "untargeted subscribe should wildcard the discriminator, name, tag, and link_id: {key}"
    );
}

#[test]
fn parse_inbound_query_node_receiver_rejects_interface_shaped_query() {
    // A server bound as a `Node` must not handle a query addressed under the
    // `Interface` discriminator with the same name+tag (and vice-versa). The
    // wire format's service_root parse catches the mismatch.
    let node_receiver = ServiceWireReceiver::new(
        "server_core",
        "server_inst",
        node("widget", "v1"),
        "ping",
        ServiceKind::Service,
    )
    .expect("valid receiver");
    let iface_shaped_query =
        "server_core/caller_core/server_inst/caller_inst/service/interface/widget/v1/_/ping";
    let err = ZenohWireFormat::parse_inbound_query(
        &node_receiver,
        iface_shaped_query,
        &user_request_attachment_bytes(),
    )
    .unwrap_err();
    assert!(matches!(
        err,
        ZenohWireParseError::ServiceRootMismatch { .. }
    ));

    let iface_receiver = ServiceWireReceiver::new(
        "server_core",
        "server_inst",
        iface("widget", "v1"),
        "ping",
        ServiceKind::Service,
    )
    .expect("valid receiver");
    let node_shaped_query =
        "server_core/caller_core/server_inst/caller_inst/service/node/widget/v1/_/ping";
    let err = ZenohWireFormat::parse_inbound_query(
        &iface_receiver,
        node_shaped_query,
        &user_request_attachment_bytes(),
    )
    .unwrap_err();
    assert!(matches!(
        err,
        ZenohWireParseError::ServiceRootMismatch { .. }
    ));
}

// ─── Topic attachment ────────────────────────────────────────────────────

#[test]
fn topic_attachment_primary_roundtrip() {
    let encoded = TopicAttachment { is_primary: true }.encode();
    assert_eq!(encoded.as_ref(), &[0x01u8]);
    assert!(TopicAttachment::decode(&encoded).is_primary);
}

#[test]
fn topic_attachment_secondary_roundtrip() {
    let encoded = TopicAttachment { is_primary: false }.encode();
    assert_eq!(encoded.as_ref(), &[0x00u8]);
    assert!(!TopicAttachment::decode(&encoded).is_primary);
}

#[test]
fn topic_attachment_missing_decodes_as_primary() {
    // Defensive: a publish that drops or omits the attachment should not
    // cause wildcard subscribers to silently drop every message. Missing
    // attachment == primary so the failure mode is "duplicates" (the
    // pre-fix behavior), not "silence".
    assert!(TopicAttachment::decode(&[]).is_primary);
}

#[test]
fn topic_attachment_unknown_byte_decodes_as_primary() {
    // Any byte other than the explicit 0x00 secondary marker decodes as
    // primary — keeps forward compat if the marker schema grows fields
    // (additional bytes are ignored by the current decoder).
    assert!(TopicAttachment::decode(&[0xff]).is_primary);
    assert!(TopicAttachment::decode(&[0x01, 0xaa]).is_primary);
}

// ─── Service query attachment (kind only) ────────────────────────────────

#[test]
fn service_query_attachment_user_request_roundtrips() {
    let attachment = ServiceQueryAttachment {
        kind: ServiceQueryKind::UserRequest,
    };
    let decoded = ServiceQueryAttachment::decode(attachment.encode().as_ref())
        .expect("encoded attachment decodes");
    assert_eq!(decoded.kind, ServiceQueryKind::UserRequest);
}

#[test]
fn service_query_attachment_probe_kind_roundtrips() {
    let attachment = ServiceQueryAttachment {
        kind: ServiceQueryKind::Probe,
    };
    let decoded = ServiceQueryAttachment::decode(attachment.encode().as_ref())
        .expect("encoded attachment decodes");
    assert_eq!(decoded.kind, ServiceQueryKind::Probe);
}

#[test]
fn service_query_attachment_decode_rejects_empty_bytes() {
    assert!(matches!(
        ServiceQueryAttachment::decode(&[]),
        Err(ZenohWireParseError::MissingServiceQueryAttachment)
    ));
}

#[test]
fn service_query_attachment_decode_rejects_older_magic() {
    // Earlier versions also carried a sibling-pinned exclusion set. New
    // producers MUST refuse those so mid-rollout skew surfaces loudly.
    assert!(matches!(
        ServiceQueryAttachment::decode(&[0x02, 0x00, 0x00]),
        Err(ZenohWireParseError::ServiceQueryAttachmentMagicMismatch { .. })
    ));
}

#[test]
fn service_query_attachment_decode_rejects_unknown_kind_byte() {
    let bytes = [ServiceQueryAttachment::MAGIC_V3, 0xff];
    assert!(matches!(
        ServiceQueryAttachment::decode(&bytes),
        Err(ZenohWireParseError::UnknownServiceQueryKind(0xff))
    ));
}

#[test]
fn service_query_attachment_decode_rejects_truncated_payload() {
    // Magic present without the kind byte.
    let bytes = vec![ServiceQueryAttachment::MAGIC_V3];
    assert!(matches!(
        ServiceQueryAttachment::decode(&bytes),
        Err(ZenohWireParseError::TruncatedServiceQueryAttachment)
    ));
}

// ─── Service reply attachment (Ack / Response / HandlerError) ────────────

#[test]
fn service_reply_attachment_ack_roundtrips() {
    let attachment = ServiceReplyAttachment {
        kind: ServiceReplyKind::Ack,
    };
    let decoded = ServiceReplyAttachment::decode(attachment.encode().as_ref())
        .expect("encoded reply attachment decodes");
    assert_eq!(decoded.kind, ServiceReplyKind::Ack);
}

#[test]
fn service_reply_attachment_response_roundtrips() {
    let attachment = ServiceReplyAttachment {
        kind: ServiceReplyKind::Response,
    };
    let decoded = ServiceReplyAttachment::decode(attachment.encode().as_ref())
        .expect("encoded reply attachment decodes");
    assert_eq!(decoded.kind, ServiceReplyKind::Response);
}

#[test]
fn service_reply_attachment_handler_error_roundtrips() {
    let attachment = ServiceReplyAttachment {
        kind: ServiceReplyKind::HandlerError,
    };
    let decoded = ServiceReplyAttachment::decode(attachment.encode().as_ref())
        .expect("encoded reply attachment decodes");
    assert_eq!(decoded.kind, ServiceReplyKind::HandlerError);
}

#[test]
fn service_reply_attachment_decode_rejects_empty_bytes() {
    assert!(matches!(
        ServiceReplyAttachment::decode(&[]),
        Err(ZenohWireParseError::MissingServiceReplyAttachment)
    ));
}

#[test]
fn service_reply_attachment_decode_rejects_unknown_magic() {
    assert!(matches!(
        ServiceReplyAttachment::decode(&[0xff, 0x00]),
        Err(ZenohWireParseError::ServiceReplyAttachmentMagicMismatch { .. })
    ));
}

#[test]
fn service_reply_attachment_decode_rejects_unknown_kind_byte() {
    let bytes = [ServiceReplyAttachment::MAGIC_V1, 0xff];
    assert!(matches!(
        ServiceReplyAttachment::decode(&bytes),
        Err(ZenohWireParseError::UnknownServiceReplyKind(0xff))
    ));
}

// ─── ParsedInboundQuery::claim ───────────────────────────────────────────

#[test]
fn claim_accepts_wildcard_link_id() {
    let parsed = ParsedInboundQuery {
        caller_core: "caller_core".to_string(),
        caller_inst: "caller_inst".to_string(),
        link_id: SINGLE_CHUNK_WILDCARD.to_string(),
        kind: ServiceQueryKind::UserRequest,
    };
    assert_eq!(parsed.claim(), Some("_"));
}

#[test]
fn claim_accepts_default_link_id_literal() {
    let parsed = ParsedInboundQuery {
        caller_core: "caller_core".to_string(),
        caller_inst: "caller_inst".to_string(),
        link_id: "_".to_string(),
        kind: ServiceQueryKind::UserRequest,
    };
    assert_eq!(parsed.claim(), Some("_"));
}

#[test]
fn claim_rejects_other_literals_defensively() {
    // Producers only advertise under `_`; any non-default literal at the
    // link_id slot is a mid-rollout protocol skew and is dropped.
    let parsed = ParsedInboundQuery {
        caller_core: "caller_core".to_string(),
        caller_inst: "caller_inst".to_string(),
        link_id: "wrist_left".to_string(),
        kind: ServiceQueryKind::UserRequest,
    };
    assert!(parsed.claim().is_none());
}

// ─── parse_topic_keyexpr extracts link_id ────────────────────────────────

#[test]
fn parse_topic_keyexpr_surfaces_concrete_link_id_at_segment_eight() {
    // Publish format: `*/{as_core}/*/{as_inst}/topic/{disc}/{name}/{tag}/{link_id}/{topic}`.
    // Index 8 is the link_id slot. The peppylib Subscription wrapper drops
    // messages whose link_id is in the sibling-pinned excluded set;
    // surfacing the literal is the load-bearing change.
    let key = "*/publisher_core/*/publisher_inst/topic/node/sensor_node/v1/wrist_left/temperature";
    let parsed = ZenohWireFormat::parse_topic_keyexpr(key).expect("should parse");
    assert_eq!(parsed.link_id, "wrist_left");
}

#[test]
fn parse_topic_keyexpr_surfaces_default_link_id_literal() {
    // A producer without `--link-id` publishes under the reserved `_` slot.
    // It still surfaces; the filter compares against the consumer's
    // excluded set, and `_` is a valid literal that just doesn't appear
    // there in practice.
    let key = "*/publisher_core/*/publisher_inst/topic/node/sensor_node/v1/_/temperature";
    let parsed = ZenohWireFormat::parse_topic_keyexpr(key).expect("should parse");
    assert_eq!(parsed.link_id, "_");
}
