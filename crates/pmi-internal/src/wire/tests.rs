use super::*;

// ─── Iface ────────────────────────────────────────────────────────────────

#[test]
fn iface_native_uses_underscore_sentinels() {
    let iface = Iface::native();
    assert_eq!(iface.name(), NATIVE_IFACE_SEGMENT);
    assert_eq!(iface.tag(), NATIVE_IFACE_SEGMENT);
    assert!(iface.is_native());
}

#[test]
fn iface_new_preserves_alphanumeric_tag() {
    let iface = Iface::new("camera_driver", "v1");
    assert_eq!(iface.name(), "camera_driver");
    assert_eq!(iface.tag(), "v1");
    assert!(!iface.is_native());
}

#[test]
fn iface_new_normalizes_hyphenated_tag() {
    let iface = Iface::new("camera_driver", "v1-beta-2");
    assert_eq!(iface.tag(), "v1_beta_2");
}

#[test]
fn iface_new_does_not_touch_underscored_tag() {
    let iface = Iface::new("nav", "v2_stable");
    assert_eq!(iface.tag(), "v2_stable");
}

#[test]
fn iface_from_options_both_none_is_native() {
    let iface = Iface::from_options(None, None).expect("should construct native iface");
    assert!(iface.is_native());
}

#[test]
fn iface_from_options_both_some_uses_values() {
    let iface = Iface::from_options(Some("nav"), Some("v2")).expect("should construct iface");
    assert_eq!(iface.name(), "nav");
    assert_eq!(iface.tag(), "v2");
}

#[test]
fn iface_from_options_one_side_only_is_err() {
    assert_eq!(Iface::from_options(Some("nav"), None), Err(IfaceError));
    assert_eq!(Iface::from_options(None, Some("v2")), Err(IfaceError));
}

#[test]
fn iface_from_options_normalizes_tag() {
    let iface =
        Iface::from_options(Some("nav"), Some("v2-stable")).expect("should construct iface");
    assert_eq!(iface.tag(), "v2_stable");
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
        as_core_node: "caller_core".into(),
        as_instance_id: "caller_inst".into(),
        to_core_node: Some("target_core".into()),
        to_instance_id: Some("target_inst".into()),
        to_node_name: "robot_arm".into(),
        iface: Iface::native(),
        to_action_name: "pick_place".into(),
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
    assert_eq!(goal.to_core_node.as_deref(), Some("target_core"));
    assert_eq!(goal.to_instance_id.as_deref(), Some("target_inst"));
    assert_eq!(goal.to_node_name, "robot_arm");
}

#[test]
fn action_sender_cancel_and_result_only_differ_by_kind() {
    let action = sample_action_sender();
    let cancel = action.cancel_service();
    let result = action.result_service();
    assert_eq!(cancel.kind, ServiceKind::ActionCancel);
    assert_eq!(result.kind, ServiceKind::ActionResult);
    // Everything else identical to the goal-service derivation.
    let goal = action.goal_service();
    assert_eq!(cancel.to_service_name, goal.to_service_name);
    assert_eq!(cancel.to_node_name, goal.to_node_name);
    assert_eq!(result.to_service_name, goal.to_service_name);
    assert_eq!(result.to_node_name, goal.to_node_name);
}

// ─── ActionWireReceiver derived services ──────────────────────────────────

fn sample_action_receiver() -> ActionWireReceiver {
    ActionWireReceiver {
        bound_core_node: "server_core".into(),
        as_instance_id: "server_inst".into(),
        as_node_name: "robot_arm".into(),
        iface: Iface::new("manipulator", "v1"),
        as_action_name: "pick_place".into(),
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
    assert_eq!(goal.as_node_name, "robot_arm");
    assert_eq!(goal.iface.name(), "manipulator");
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
        assert_eq!(derived.as_node_name, goal.as_node_name);
        assert_eq!(derived.as_service_name, goal.as_service_name);
        assert_eq!(derived.iface, goal.iface);
    }
    assert_eq!(cancel.kind, ServiceKind::ActionCancel);
    assert_eq!(result.kind, ServiceKind::ActionResult);
}
