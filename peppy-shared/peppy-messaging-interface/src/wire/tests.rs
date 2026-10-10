use super::test_support::{node, seg};
use super::*;

// ─── Identifier constructors ─────────────────────────────────────────────

type TargetConstructor = fn(&str, &str) -> Result<SenderTarget, SenderTargetError>;

/// The contract, node and pairing constructors share one name and tag
/// validation, so each constructor test runs against all three.
const TARGET_CONSTRUCTORS: [(&str, TargetConstructor); 3] = [
    ("contract", SenderTarget::contract),
    ("node", SenderTarget::node),
    ("pairing", SenderTarget::pairing),
];

#[test]
fn target_constructors_preserve_an_alphanumeric_tag() {
    for (kind, new_target) in TARGET_CONSTRUCTORS {
        let target =
            new_target("camera_driver", "v1").unwrap_or_else(|err| panic!("{kind}: {err}"));
        assert_eq!(target.name(), "camera_driver", "{kind}");
        assert_eq!(target.tag(), "v1", "{kind}");
    }
}

#[test]
fn target_constructors_normalize_a_hyphenated_tag() {
    for (kind, new_target) in TARGET_CONSTRUCTORS {
        let target =
            new_target("camera_driver", "v1-beta-2").unwrap_or_else(|err| panic!("{kind}: {err}"));
        assert_eq!(target.tag(), "v1_beta_2", "{kind}");
    }
}

#[test]
fn target_constructors_keep_an_underscored_tag() {
    for (kind, new_target) in TARGET_CONSTRUCTORS {
        let target = new_target("nav", "v2_stable").unwrap_or_else(|err| panic!("{kind}: {err}"));
        assert_eq!(target.tag(), "v2_stable", "{kind}");
    }
}

#[test]
fn target_constructors_reject_a_segment_with_a_slash() {
    for (kind, new_target) in TARGET_CONSTRUCTORS {
        let err = new_target("nav/sub", "v1").unwrap_err();
        assert!(
            matches!(err, SenderTargetError::InvalidSegment(_)),
            "{kind}"
        );
    }
}

#[test]
fn contract_new_rejects_reserved_sentinel() {
    let err = ContractIdentifier::new("_", "v1").unwrap_err();
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

// ─── SenderTarget ─────────────────────────────────────────────────────────

#[test]
fn sender_target_contract_discriminator_is_contract() {
    let target = SenderTarget::contract("manipulator", "v1").expect("valid contract target");
    assert_eq!(target.discriminator(), "contract");
    assert_eq!(target.name(), "manipulator");
    assert_eq!(target.tag(), "v1");
    assert!(target.is_contract());
    assert!(!target.is_node());
}

#[test]
fn sender_target_node_discriminator_is_node() {
    let target = node("uvc_camera", "v1");
    assert_eq!(target.discriminator(), "node");
    assert_eq!(target.name(), "uvc_camera");
    assert_eq!(target.tag(), "v1");
    assert!(target.is_node());
    assert!(!target.is_contract());
}

#[test]
fn sender_target_contract_and_node_with_same_name_tag_are_distinct() {
    let contract = SenderTarget::contract("widget", "v1").expect("valid contract target");
    let node_target = node("widget", "v1");
    assert_ne!(contract, node_target);
    assert_ne!(contract.discriminator(), node_target.discriminator());
}

#[test]
fn sender_target_pairing_discriminator_is_pairing() {
    let target = SenderTarget::pairing("arm_link", "v1").expect("valid pairing target");
    assert_eq!(target.discriminator(), "pairing");
    assert_eq!(target.name(), "arm_link");
    assert_eq!(target.tag(), "v1");
    assert!(target.is_pairing());
    assert!(!target.is_contract());
    assert!(!target.is_node());
}

#[test]
fn sender_target_pairing_and_contract_with_same_name_tag_are_distinct() {
    // The load-bearing lock-in property: pairing traffic can never match a
    // contract subscription because the wire discriminators differ.
    let pairing = SenderTarget::pairing("widget", "v1").expect("valid pairing target");
    let contract = SenderTarget::contract("widget", "v1").expect("valid contract target");
    assert_ne!(pairing, contract);
    assert_ne!(pairing.discriminator(), contract.discriminator());
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

// ─── Caller-side sender constructors ──────────────────────────────────────

/// The outcome of building a service sender and an action sender from the
/// same arguments, so that one case checks both constructors.
fn build_caller_senders(
    own_core_node: &str,
    target: Option<&ProducerRef>,
    name: &str,
) -> [(&'static str, crate::error::Result<()>); 2] {
    [
        (
            "ServiceWireSender",
            ServiceWireSender::new(
                own_core_node,
                "caller_inst",
                target,
                node("robot_arm", "v1"),
                name,
                ServiceKind::Service,
            )
            .map(drop),
        ),
        (
            "ActionWireSender",
            ActionWireSender::new(
                own_core_node,
                "caller_inst",
                target,
                node("robot_arm", "v1"),
                name,
            )
            .map(drop),
        ),
    ]
}

#[test]
fn caller_senders_check_their_own_core_node_then_the_target_then_the_name() {
    let invalid_core_node = ProducerRef::new("bad/core", "target_inst");
    let invalid_instance_id = ProducerRef::new("target_core", "bad@inst");
    let cases = [
        (
            "caller@core",
            Some(&invalid_core_node),
            "ping",
            SegmentError::ContainsAt("caller@core".to_string()),
        ),
        (
            "caller_core",
            Some(&invalid_core_node),
            "bad/name",
            SegmentError::ContainsSlash("bad/core".to_string()),
        ),
        (
            "caller_core",
            Some(&invalid_instance_id),
            "bad/name",
            SegmentError::ContainsAt("bad@inst".to_string()),
        ),
        (
            "caller_core",
            None,
            "bad/name",
            SegmentError::ContainsSlash("bad/name".to_string()),
        ),
    ];

    for (own_core_node, target, name, expected) in cases {
        for (constructor, result) in build_caller_senders(own_core_node, target, name) {
            match result {
                Err(crate::error::Error::InvalidSegment(error)) => {
                    assert_eq!(error, expected, "{constructor}");
                }
                other => panic!("{constructor}: expected {expected:?}, got {other:?}"),
            }
        }
    }
}

#[test]
fn caller_senders_pin_both_producer_slots_or_leave_both_open() {
    let target = ProducerRef::new("target_core", "target_inst");
    let pinned = ServiceWireSender::new(
        "caller_core",
        "caller_inst",
        Some(&target),
        node("robot_arm", "v1"),
        "ping",
        ServiceKind::Service,
    )
    .expect("valid service sender");
    assert_eq!(pinned.target_core_node, Some(seg("target_core")));
    assert_eq!(pinned.target_instance_id, Some(seg("target_inst")));

    let open = ActionWireSender::new(
        "caller_core",
        "caller_inst",
        None,
        node("robot_arm", "v1"),
        "pick_place",
    )
    .expect("valid action sender");
    assert_eq!(open.target_core_node, None);
    assert_eq!(open.target_instance_id, None);
}

// ─── ActionWireSender derived services ────────────────────────────────────

fn sample_action_sender() -> ActionWireSender {
    ActionWireSender {
        as_core_node: seg("caller_core"),
        as_instance_id: seg("caller_inst"),
        target_core_node: Some(seg("target_core")),
        target_instance_id: Some(seg("target_inst")),
        to_target: node("robot_arm", "v1"),
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
        as_identity: SenderTarget::contract("manipulator", "v1").expect("valid contract target"),
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
    assert!(goal.as_identity.is_contract());
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
