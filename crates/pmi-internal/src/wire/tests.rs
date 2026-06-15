use super::*;

/// Test-local shorthand: wrap a `&str` in a validated [`Segment`]. Panics on
/// invalid input — tests use known-good values only.
fn seg(value: &str) -> Segment {
    Segment::try_from(value).expect("test segment value should be valid")
}

/// Test-local shorthand: build a node-shaped target with the standard test tag.
fn test_node_target(name: &str) -> SenderTarget {
    SenderTarget::node(name, "v1").expect("test node target")
}

// ─── InterfaceIdentifier ──────────────────────────────────────────────────

#[test]
fn interface_new_preserves_alphanumeric_tag() {
    let interface = InterfaceIdentifier::new("camera_driver", "v1").expect("valid interface");
    assert_eq!(interface.name(), "camera_driver");
    assert_eq!(interface.tag(), "v1");
}

#[test]
fn interface_new_normalizes_hyphenated_tag() {
    let interface =
        InterfaceIdentifier::new("camera_driver", "v1-beta-2").expect("valid interface");
    assert_eq!(interface.tag(), "v1_beta_2");
}

#[test]
fn interface_new_does_not_touch_underscored_tag() {
    let interface = InterfaceIdentifier::new("nav", "v2_stable").expect("valid interface");
    assert_eq!(interface.tag(), "v2_stable");
}

#[test]
fn interface_new_rejects_segment_with_slash() {
    let err = InterfaceIdentifier::new("nav/sub", "v1").unwrap_err();
    assert!(matches!(err, SenderTargetError::InvalidSegment(_)));
}

#[test]
fn interface_new_rejects_reserved_sentinel() {
    let err = InterfaceIdentifier::new("_", "v1").unwrap_err();
    assert!(matches!(err, SenderTargetError::InvalidSegment(_)));
}

// ─── Segment validators ───────────────────────────────────────────────────

#[test]
fn segment_try_from_rejects_at_sign() {
    let err = Segment::try_from("foo@bar").unwrap_err();
    assert!(matches!(err, SegmentError::ContainsAt(s) if s == "foo@bar"));
}

#[test]
fn segment_try_link_id_rejects_at_sign() {
    let err = Segment::try_link_id("cam@a").unwrap_err();
    assert!(matches!(err, SegmentError::ContainsAt(s) if s == "cam@a"));
}

#[test]
fn segment_try_link_id_still_rejects_slash_and_wildcards() {
    assert!(matches!(
        Segment::try_link_id("a/b"),
        Err(SegmentError::ContainsSlash(_))
    ));
    assert!(matches!(
        Segment::try_link_id("*"),
        Err(SegmentError::ReservedSentinel(_))
    ));
}

// ─── NodeIdentifier ───────────────────────────────────────────────────────

#[test]
fn node_new_preserves_alphanumeric_tag() {
    let node = NodeIdentifier::new("uvc_camera", "v1").expect("valid node");
    assert_eq!(node.name(), "uvc_camera");
    assert_eq!(node.tag(), "v1");
}

#[test]
fn node_new_normalizes_hyphenated_tag() {
    let node = NodeIdentifier::new("uvc_camera", "v1-beta-2").expect("valid node");
    assert_eq!(node.tag(), "v1_beta_2");
}

#[test]
fn node_new_does_not_touch_underscored_tag() {
    let node = NodeIdentifier::new("uvc_camera", "v2_stable").expect("valid node");
    assert_eq!(node.tag(), "v2_stable");
}

#[test]
fn node_new_rejects_segment_with_slash() {
    let err = NodeIdentifier::new("nav/sub", "v1").unwrap_err();
    assert!(matches!(err, SenderTargetError::InvalidSegment(_)));
}

// ─── SenderTarget ─────────────────────────────────────────────────────────

#[test]
fn sender_target_interface_discriminator_is_interface() {
    let target = SenderTarget::interface("manipulator", "v1").expect("valid interface target");
    assert_eq!(target.discriminator(), "interface");
    assert_eq!(target.name(), "manipulator");
    assert_eq!(target.tag(), "v1");
    assert!(target.is_interface());
    assert!(!target.is_node());
}

#[test]
fn sender_target_node_discriminator_is_node() {
    let target = test_node_target("uvc_camera");
    assert_eq!(target.discriminator(), "node");
    assert_eq!(target.name(), "uvc_camera");
    assert_eq!(target.tag(), "v1");
    assert!(target.is_node());
    assert!(!target.is_interface());
}

#[test]
fn sender_target_interface_and_node_with_same_name_tag_are_distinct() {
    let interface = SenderTarget::interface("widget", "v1").expect("valid interface target");
    let node = test_node_target("widget");
    assert_ne!(interface, node);
    assert_ne!(interface.discriminator(), node.discriminator());
}

// ─── ServiceKind ──────────────────────────────────────────────────────────

#[test]
fn service_kind_service_has_no_suffix() {
    assert_eq!(ServiceKind::Service.root_segment(), "service");
    assert_eq!(ServiceKind::Service.suffix(), None);
}

#[test]
fn service_kind_action_variants_share_root_with_distinct_suffixes() {
    assert_eq!(ServiceKind::ActionGoal.root_segment(), "action");
    assert_eq!(ServiceKind::ActionCancel.root_segment(), "action");
    assert_eq!(ServiceKind::ActionResult.root_segment(), "action");

    assert_eq!(ServiceKind::ActionGoal.suffix(), Some("goal"));
    assert_eq!(ServiceKind::ActionCancel.suffix(), Some("cancel"));
    assert_eq!(ServiceKind::ActionResult.suffix(), Some("result"));
}

// ─── ActionWireSender derived services ────────────────────────────────────

fn sample_action_sender() -> ActionWireSender {
    ActionWireSender {
        as_core_node: seg("caller_core"),
        as_instance_id: seg("caller_inst"),
        target_core_node: Some(seg("target_core")),
        target_instance_id: Some(seg("target_inst")),
        to_target: test_node_target("robot_arm"),
        to_action_name: seg("pick_place"),
    }
}

#[test]
fn action_sender_goal_service_threads_kind_and_name() {
    let action = sample_action_sender();
    let goal = action.goal_service();
    assert_eq!(goal.kind, ServiceKind::ActionGoal);
    assert_eq!(goal.to_service_name, "pick_place");
    assert_eq!(goal.bound_core_node, "caller_core");
    assert_eq!(goal.as_instance_id, "caller_inst");
    assert_eq!(goal.target_core_node.as_deref(), Some("target_core"));
    assert_eq!(goal.target_instance_id.as_deref(), Some("target_inst"));
    assert_eq!(goal.to_target.name(), "robot_arm");
    assert_eq!(goal.to_target.tag(), "v1");
    assert!(goal.to_target.is_node());
}

#[test]
fn action_sender_cancel_and_result_only_differ_by_kind() {
    let action = sample_action_sender();
    let cancel = action.cancel_service();
    let result = action.result_service();
    assert_eq!(cancel.kind, ServiceKind::ActionCancel);
    assert_eq!(result.kind, ServiceKind::ActionResult);
    let goal = action.goal_service();
    assert_eq!(cancel.to_service_name, goal.to_service_name);
    assert_eq!(cancel.to_target, goal.to_target);
    assert_eq!(result.to_service_name, goal.to_service_name);
    assert_eq!(result.to_target, goal.to_target);
}

#[test]
fn action_sender_pinned_to_overwrites_identity_and_preserves_rest() {
    // Mimic the wildcard-goal case: start with no target identity, then
    // latch to a concrete responder after `goal_response` arrives.
    let mut wildcard = sample_action_sender();
    wildcard.target_core_node = None;
    wildcard.target_instance_id = None;

    let pinned = wildcard
        .pinned_to("responder_core", "responder_inst")
        .expect("pinned_to should validate the segments");

    assert_eq!(pinned.target_core_node.as_deref(), Some("responder_core"));
    assert_eq!(pinned.target_instance_id.as_deref(), Some("responder_inst"));
    // Everything else carries over unchanged.
    assert_eq!(pinned.as_core_node, wildcard.as_core_node);
    assert_eq!(pinned.as_instance_id, wildcard.as_instance_id);
    assert_eq!(pinned.to_target, wildcard.to_target);
    assert_eq!(pinned.to_action_name, wildcard.to_action_name);
}

#[test]
fn action_sender_pinned_to_rejects_invalid_identity_segment() {
    let wildcard = sample_action_sender();
    let err = wildcard.pinned_to("ok_core", "bad/inst").unwrap_err();
    assert!(format!("{err}").contains("/"));
}

// ─── ActionWireReceiver derived services ──────────────────────────────────

fn sample_action_receiver() -> ActionWireReceiver {
    ActionWireReceiver {
        bound_core_node: seg("server_core"),
        as_instance_id: seg("server_inst"),
        as_identity: SenderTarget::interface("manipulator", "v1").expect("valid interface target"),
        as_action_name: seg("pick_place"),
    }
}

#[test]
fn action_receiver_goal_service_threads_kind_and_name() {
    let action = sample_action_receiver();
    let goal = action.goal_service();
    assert_eq!(goal.kind, ServiceKind::ActionGoal);
    assert_eq!(goal.as_service_name, "pick_place");
    assert_eq!(goal.bound_core_node, "server_core");
    assert_eq!(goal.as_instance_id, "server_inst");
    assert_eq!(goal.as_identity.name(), "manipulator");
    assert!(goal.as_identity.is_interface());
}

#[test]
fn action_receiver_all_three_variants_have_consistent_addressing() {
    let action = sample_action_receiver();
    let goal = action.goal_service();
    let cancel = action.cancel_service();
    let result = action.result_service();
    for derived in [&cancel, &result] {
        assert_eq!(derived.bound_core_node, goal.bound_core_node);
        assert_eq!(derived.as_instance_id, goal.as_instance_id);
        assert_eq!(derived.as_identity, goal.as_identity);
        assert_eq!(derived.as_service_name, goal.as_service_name);
    }
    assert_eq!(cancel.kind, ServiceKind::ActionCancel);
    assert_eq!(result.kind, ServiceKind::ActionResult);
}

// ─── from_validated panic contract ──────────────────────────────────────────

#[test]
fn node_from_validated_builds_a_node_target_for_safe_segments() {
    let target = SenderTarget::node_from_validated("uvc_camera", "v1");
    assert!(target.is_node());
    assert_eq!(target.name(), "uvc_camera");
    assert_eq!(target.tag(), "v1");
}

#[test]
fn node_from_validated_normalizes_hyphenated_tag_like_new() {
    // from_validated funnels through new(), so the hyphen-to-underscore tag
    // normalization still applies to the validated path.
    let target = SenderTarget::node_from_validated("arm", "v1-beta");
    assert_eq!(target.tag(), "v1_beta");
}

#[test]
#[should_panic(expected = "validated name and tag should be wire-segment safe")]
fn node_from_validated_panics_on_reserved_sentinel() {
    // The documented panic contract: a degenerate name that collides with a
    // reserved wire sentinel must blow up rather than produce a bad target.
    let _ = NodeIdentifier::from_validated("_", "v1");
}
