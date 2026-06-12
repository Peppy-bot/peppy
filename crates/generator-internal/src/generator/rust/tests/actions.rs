use super::*;

use config::node::{ConsumedAction, ExposedAction};
use std::{collections::HashMap, fs};

// --- Exposes examples
const EXPOSED_ACTION_EXAMPLE: &str = r#"
{
  name: "move_arm",
  goal_service: {
    request_message_format: {
      arm_id: "u16",
      desired_position: {
        $type: "array",
        $items: "i32",
        $length: 3
      }
    },
    response_message_format: {
      accepted: "bool"
    }
  },
  feedback_topic: {
    qos_profile: "sensor_data",
    message_format: {
      new_position: {
        $type: "array",
        $items: "i32",
        $length: 3
      }
    }
  },
  result_service: {
    response_message_format: {
      success: "bool",
      error_msg: {
        $type: "string",
        $optional: true
      },
      final_position: {
        $type: "array",
        $items: "i32",
        $length: 3
      }
    }
  }
}
"#;

const EXPOSED_ACTION_EXAMPLE_EMPTY_GOAL_REQUEST: &str = r#"
{
  name: "move_arm",
  goal_service: {
    request_message_format: {},
    response_message_format: {
      accepted: "bool"
    }
  },
  feedback_topic: {
    qos_profile: "sensor_data",
    message_format: {
      new_position: {
        $type: "array",
        $items: "i32",
        $length: 3
      }
    }
  },
  result_service: {
    response_message_format: {
      success: "bool",
      error_msg: {
        $type: "string",
        $optional: true
      },
      final_position: {
        $type: "array",
        $items: "i32",
        $length: 3
      }
    }
  }
}
"#;

const EXPOSED_ACTION_EXAMPLE2: &str = r#"
{
  name: "rotate_servo_clockwise",
  goal_service: {
    response_message_format: {
      accepted: "bool"
    }
  },
  feedback_topic: {
    qos_profile: "sensor_data",
    message_format: {
      new_position: "i32", // Angular position in degree
      speed: "i32"
    }
  },
  result_service: {
    response_message_format: {
      success: "bool",
      error_msg: {
        $type: "string",
        $optional: true
      },
    }
  }
}
"#;

const EXPOSED_ACTION_RESERVED_FEEDBACK_FIELD_EXAMPLE: &str = r#"
{
  name: "status_ping",
  feedback_topic: {
    qos_profile: "standard",
    message_format: {
      instance_id: "string",
      progress: "u8"
    }
  }
}
"#;

// --- Subscribes examples
pub(super) const SUBSCRIBED_ACTION_EXAMPLE1: &str = r#"
{
  link_id: "brain",
  name: "move_arm",
}
"#;

pub(super) const SUBSCRIBED_ACTION_GOAL_FORMAT1: &str = r#"
{
  arm_id: "u16",
  desired_position: {
    $type: "array",
    $items: "i32",
    $length: 3
  }
}
"#;

pub(super) const SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT1: &str = r#"
{
  accepted: "bool"
}
"#;

pub(super) const SUBSCRIBED_ACTION_FEEDBACK_FORMAT1: &str = r#"
{
  new_position: {
    $type: "array",
    $items: "i32",
    $length: 3
  }
}
"#;

pub(super) const SUBSCRIBED_ACTION_RESULT_RESPONSE_FORMAT1: &str = r#"
{
  success: "bool",
  error_msg: {
    $type: "string",
    $optional: true
  },
  final_position: {
    $type: "array",
    $items: "i32",
    $length: 3
  }
}
"#;

const SUBSCRIBED_ACTION_EXAMPLE2: &str = r#"
{
  link_id: "controller",
  name: "rotate_servo_clockwise",
}
"#;

const SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT2: &str = r#"
{
  accepted: "bool"
}
"#;
const SUBSCRIBED_ACTION_FEEDBACK_FORMAT2: &str = r#"
{
  new_position: "i32",
  speed: "i32"
}
"#;
const SUBSCRIBED_ACTION_RESULT_RESPONSE_FORMAT2: &str = r#"
{
  success: "bool",
  error_msg: {
    $type: "string",
    $optional: true
  },
}
"#;

#[test]
fn exposed_action() {
    let action: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE).unwrap();

    let mut generator = RustGenerator::new();
    generator.add_exposed_action(&action, None).unwrap();
    let artifacts = render_artifacts(generator.into_artifacts());
    assert_eq!(
        artifacts.len(),
        1,
        "expected a single generated artifact, got {}",
        artifacts.len()
    );
    let rendered = artifacts.into_iter().next().expect("artifact is present");

    // ACTION_NAME constant and GoalRequestData struct
    assert_contains_all(
        &rendered,
        &[
            "const ACTION_NAME: &str = \"move_arm\";",
            "pub struct GoalRequestData",
            "pub arm_id: u16",
            "desired_position: [i32; 3]",
        ],
    );

    // GoalRequest struct
    assert_contains_all(
        &rendered,
        &[
            "pub struct GoalRequest",
            "pub instance_id: String",
            "pub core_node: String",
            "pub data: GoalRequestData",
        ],
    );

    // GoalResponse struct: framework-owned goal acknowledgement
    // ({accepted, error_message}) with its `new` constructor and the
    // accept/reject helpers the decider returns.
    assert_contains_all(
        &rendered,
        &[
            "pub struct GoalResponse",
            "impl GoalResponse",
            "pub fn new(accepted: bool, error_message: Option<String>) -> Self",
            "pub fn accept() -> Self",
            "pub fn reject(reason: impl Into<String>) -> Self",
        ],
    );

    // ActionHandle wraps the concurrent-action engine.
    assert_contains_all(
        &rendered,
        &[
            "pub struct ActionHandle",
            "inner: peppylib::messaging::ConcurrentAction",
        ],
    );

    // expose builds the engine (has_feedback = true for this action).
    assert_contains_all(
        &rendered,
        &[
            "pub async fn expose(node_runner: &crate::NodeRunner) -> crate::Result<Self>",
            "peppylib::messaging::ConcurrentAction::expose",
        ],
    );

    // handle_goal_next_request returns a per-goal GoalContext (or None when
    // rejected / the stream closed).
    assert_contains_all(
        &rendered,
        &[
            "pub async fn handle_goal_next_request",
            "F: Fn(&GoalRequest) -> crate::Result<GoalResponse>",
            "crate::Result<Option<GoalContext>>",
            "recv_next_goal",
        ],
    );

    // GoalContext: request accessor, cancel signal, feedback, completion.
    assert_contains_all(
        &rendered,
        &[
            "pub struct GoalContext",
            "pub fn request(&self) -> &GoalRequest",
            "pub fn goal_id(&self) -> &str",
            "pub async fn cancel_signal(&self)",
            "pub fn is_cancelled(&self) -> bool",
            "pub async fn publish_feedback",
            "pub async fn complete",
            "pub async fn complete_cancelled",
            "success: bool",
            "error_msg: Option<String>",
            "final_position: [i32; 3]",
        ],
    );

    // Goal request deserializer helper remains.
    assert_contains_all(&rendered, &["fn deserialize_goal_request"]);
}

#[test]
fn expose_action_without_request_body() {
    let action: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE2).unwrap();

    let mut generator = RustGenerator::new();
    generator.add_exposed_action(&action, None).unwrap();
    let rendered = render_artifacts(generator.into_artifacts())
        .into_iter()
        .next()
        .expect("artifact is present");

    // GoalRequest still exists (with instance_id and core_node) even without
    // data. The framework GoalResponse (+ accept/reject) is always present.
    assert_contains_all(
        &rendered,
        &[
            "pub struct GoalRequest",
            "pub struct GoalResponse",
            "pub fn accept() -> Self",
            "pub fn reject(reason: impl Into<String>) -> Self",
            "pub async fn handle_goal_next_request",
            "F: Fn(&GoalRequest) -> crate::Result<GoalResponse>",
        ],
    );
    // No request data → GoalRequest has no `data` field and no deserializer.
    assert_rendered!(
        !rendered.contains("pub data:"),
        rendered,
        "GoalRequest carries no data field when there is no request body"
    );
    assert_rendered!(
        !rendered.contains("deserialize_goal_request"),
        rendered,
        "no goal request deserializer without request data"
    );

    // Result → GoalContext completion methods.
    assert_contains_all(
        &rendered,
        &[
            "pub async fn complete",
            "pub async fn complete_cancelled",
            "success: bool",
            "error_msg: Option<String>",
        ],
    );

    // Feedback → GoalContext::publish_feedback.
    assert_contains_all(&rendered, &["pub async fn publish_feedback"]);
}

#[test]
fn exposed_action_rejects_reserved_message_field_name() {
    use crate::error::Error;

    let action: ExposedAction =
        serde_json5::from_str(EXPOSED_ACTION_RESERVED_FEEDBACK_FIELD_EXAMPLE).unwrap();

    let mut generator = RustGenerator::new();
    let err = generator.add_exposed_action(&action, None).unwrap_err();

    match err {
        Error::UnauthorizedMessageFieldName {
            field,
            path,
            context,
        } => {
            assert_eq!(field, "instance_id");
            assert_eq!(path, "instance_id");
            assert_eq!(context, "message_format");
        }
        other => panic!("expected UnauthorizedMessageFieldName, got: {other:?}"),
    }
}

#[test]
fn expose_feedback_only_action() {
    // A feedback-only action is still goal-driven: the client fires a goal, the
    // server accepts it and publishes feedback through the GoalContext. The goal
    // has no request/response payload and there is no completion (no result).
    let action: ExposedAction = serde_json5::from_str(
        r#"
        {
          name: "blink_led",
          feedback_topic: {
            qos_profile: "standard",
            message_format: {
              progress: "u8"
            }
          }
        }
        "#,
    )
    .unwrap();

    let mut generator = RustGenerator::new();
    generator.add_exposed_action(&action, None).unwrap();
    let rendered = render_artifacts(generator.into_artifacts())
        .into_iter()
        .next()
        .expect("artifact is present");

    assert_contains_all(
        &rendered,
        &[
            "pub struct ActionHandle",
            "inner: peppylib::messaging::ConcurrentAction",
            "pub async fn expose(node_runner: &crate::NodeRunner) -> crate::Result<Self>",
            "peppylib::messaging::ConcurrentAction::expose",
            "pub async fn handle_goal_next_request",
            "pub struct GoalResponse",
            "pub struct GoalContext",
            "pub async fn publish_feedback",
        ],
    );

    // The goal acknowledgement is framework-owned, so even a feedback-only
    // action gets the GoalResponse accept/reject helpers.
    assert_contains_all(
        &rendered,
        &[
            "pub fn accept() -> Self",
            "pub fn reject(reason: impl Into<String>) -> Self",
        ],
    );
    // No result service → no completion methods.
    assert_rendered!(
        !rendered.contains("pub async fn complete"),
        rendered,
        "no completion methods without a result service"
    );
}

#[test]
fn expose_two_actions() {
    let action1: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE).unwrap();
    let action2: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE2).unwrap();

    let mut generator = RustGenerator::new();
    generator.add_exposed_action(&action1, None).unwrap();
    generator.add_exposed_action(&action2, None).unwrap();

    let artifacts = generator.into_artifacts();
    assert_eq!(
        artifacts.len(),
        2,
        "expected two generated artifacts, got {}",
        artifacts.len()
    );

    let artifact_map: HashMap<_, _> = artifacts
        .into_iter()
        .map(|artifact| (artifact.leaf_name().to_string(), artifact.code_output))
        .collect();

    let move_arm = artifact_map
        .get("move_arm")
        .expect("move_arm artifact is present");
    let rotate_servo = artifact_map
        .get("rotate_servo_clockwise")
        .expect("rotate_servo_clockwise artifact is present");

    // move_arm checks
    assert_contains_all(
        move_arm,
        &[
            "pub async fn handle_goal_next_request",
            "pub struct GoalRequest",
            "pub struct GoalContext",
            "pub async fn complete",
            "pub async fn publish_feedback",
            "const ACTION_NAME: &str = \"move_arm\";",
        ],
    );

    // rotate_servo_clockwise checks
    assert_contains_all(
        rotate_servo,
        &[
            "pub async fn handle_goal_next_request",
            "F: Fn(&GoalRequest) -> crate::Result<GoalResponse>",
            "pub struct GoalResponse",
            "pub async fn complete",
            "pub async fn publish_feedback",
            "const ACTION_NAME: &str = \"rotate_servo_clockwise\";",
        ],
    );
}

#[test]
fn consumed_action() {
    let action: ConsumedAction = serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE1).unwrap();
    let goal_request_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_FORMAT1).unwrap();
    let goal_response_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT1).unwrap();
    let feedback_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_FEEDBACK_FORMAT1).unwrap();
    let result_response_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_RESULT_RESPONSE_FORMAT1).unwrap();
    let format = ConsumedActionMessage {
        goal_request: Some(goal_request_format),
        goal_response: Some(goal_response_format),
        feedback: Some(feedback_format),
        result_request: None,
        result_response: Some(result_response_format),
    };

    let mut generator = RustGenerator::new();
    generator
        .add_consumed_action(
            &action,
            &format,
            &crate::DependencyContext::native("brain", "v1"),
        )
        .unwrap();
    let artifacts = render_artifacts(generator.into_artifacts());
    assert_eq!(
        artifacts.len(),
        1,
        "expected a single generated artifact, got {}",
        artifacts.len()
    );
    let rendered = artifacts.into_iter().next().expect("artifact is present");

    // Constants
    assert_contains_all(
        &rendered,
        &[
            "const TARGET_NODE_NAME: &str = \"brain\";",
            "const TARGET_ACTION_NAME: &str = \"move_arm\";",
        ],
    );

    // GoalRequest struct
    assert_contains_all(
        &rendered,
        &[
            "pub struct GoalRequest",
            "pub arm_id: u16",
            "pub desired_position: [i32; 3]",
        ],
    );

    // GoalResponseData struct
    assert_contains_all(
        &rendered,
        &["pub struct GoalResponseData", "pub accepted: bool"],
    );

    // ActionHandle struct
    assert_contains_all(
        &rendered,
        &[
            "pub struct ActionHandle",
            "messenger: peppylib::MessengerHandle",
            "inner: peppylib::messaging::ActionGoalHandle",
            "pub data: GoalResponseData",
            "impl ActionHandle",
        ],
    );

    // CancelResponse struct carries a generated typed CancelState enum.
    assert_contains_all(
        &rendered,
        &[
            "pub enum CancelState",
            "Signalled",
            "AlreadyTerminal",
            "Unknown",
            "pub struct CancelResponse",
            "pub state: CancelState",
        ],
    );

    // ResultResponseData, the ResultOutcome enum, and the ResultResponse struct.
    assert_contains_all(
        &rendered,
        &[
            "pub struct ResultResponseData",
            "pub final_position: [i32; 3]",
            "pub enum ResultOutcome",
            "Completed(ResultResponseData)",
            "Cancelled(ResultResponseData)",
            "Abandoned",
            "Expired",
            "pub struct ResultResponse",
            "pub core_node: String",
            "pub instance_id: String",
            "pub outcome: ResultOutcome",
        ],
    );

    // FeedbackMessage struct
    assert_contains_all(
        &rendered,
        &["pub struct FeedbackMessage", "pub new_position: [i32; 3]"],
    );

    // fire_goal method (constructor). The fixture's `DependencyContext::native`
    // defaults to `WireLinkId::wildcard()` (no manifest link_id), so the
    // send_goal call splices a typed `None` at its single target slot.
    assert_contains_all(
        &rendered,
        &[
            "pub async fn fire_goal(",
            "request: GoalRequest",
            "feedback_qos: peppylib::config::QoSProfile",
            "-> crate::Result<Self>",
            "peppylib::ActionMessenger::send_goal",
            "Option::<&peppylib::messaging::ProducerRef>::None,",
            "node_runner.messenger().clone()",
        ],
    );

    // cancel_goal method
    assert_contains_all(
        &rendered,
        &[
            "pub async fn cancel_goal(",
            "&self,",
            "-> crate::Result<CancelResponse>",
            "peppylib::ActionMessenger::cancel_goal",
            "&self.messenger",
            "&self.inner",
        ],
    );

    // on_next_feedback_message method
    assert_contains_all(
        &rendered,
        &[
            "pub async fn on_next_feedback_message(&mut self)",
            "self.inner.on_next_feedback()",
            "fn deserialize_feedback_payload",
        ],
    );

    // get_result method
    assert_contains_all(
        &rendered,
        &[
            "pub async fn get_result(",
            "&self,",
            "-> crate::Result<ResultResponse>",
            "peppylib::ActionMessenger::request_result",
            "&self.messenger",
            "&self.inner",
        ],
    );

    // Context labels use format! macro
    assert_contains_all(
        &rendered,
        &[
            "format!(\"{} {} GoalResponse\", TARGET_NODE_NAME, TARGET_ACTION_NAME)",
            r#"TARGET_NODE_NAME, TARGET_ACTION_NAME, "ResultResponse""#,
            "format!(\"{} {} FeedbackMessage\", TARGET_NODE_NAME, TARGET_ACTION_NAME)",
        ],
    );
}

#[test]
fn consumed_two_actions_same_node() {
    let move_arm_action: ConsumedAction =
        serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE1).unwrap();
    let move_arm_goal_request: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_FORMAT1).unwrap();
    let move_arm_goal_response: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT1).unwrap();
    let move_arm_feedback: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_FEEDBACK_FORMAT1).unwrap();
    let move_arm_result_response: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_RESULT_RESPONSE_FORMAT1).unwrap();
    let move_arm_messages = ConsumedActionMessage {
        goal_request: Some(move_arm_goal_request),
        goal_response: Some(move_arm_goal_response),
        feedback: Some(move_arm_feedback),
        result_request: None,
        result_response: Some(move_arm_result_response),
    };

    // Both subscriptions target the same source node ("brain"), so link_id must match.
    let rotate_action: ConsumedAction =
        serde_json5::from_str(r#"{ link_id: "brain", name: "rotate_servo_clockwise" }"#).unwrap();
    let rotate_goal_response: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT2).unwrap();
    let rotate_feedback: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_FEEDBACK_FORMAT2).unwrap();
    let rotate_result_response: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_RESULT_RESPONSE_FORMAT2).unwrap();
    let rotate_messages = ConsumedActionMessage {
        goal_request: None,
        goal_response: Some(rotate_goal_response),
        feedback: Some(rotate_feedback),
        result_request: None,
        result_response: Some(rotate_result_response),
    };

    let mut generator = RustGenerator::new();
    generator
        .add_consumed_action(
            &move_arm_action,
            &move_arm_messages,
            &crate::DependencyContext::native("brain", "v1"),
        )
        .unwrap();
    generator
        .add_consumed_action(
            &rotate_action,
            &rotate_messages,
            &crate::DependencyContext::native("brain", "v1"),
        )
        .unwrap();

    let artifacts: Vec<_> = generator.into_artifacts();
    assert_eq!(
        artifacts.len(),
        2,
        "expected two generated artifacts, got {}",
        artifacts.len()
    );

    let artifact_map: HashMap<_, _> = artifacts
        .into_iter()
        .map(|artifact| (artifact.leaf_name().to_string(), artifact.code_output))
        .collect();

    let move_arm_module =
        sanitize_node_display_name(&raw_module_label("brain", &move_arm_action.name));
    let rotate_module = sanitize_node_display_name(&raw_module_label("brain", &rotate_action.name));

    let move_arm = artifact_map
        .get(&move_arm_module)
        .unwrap_or_else(|| panic!("move_arm artifact `{}` is present", move_arm_module));
    let rotate_servo = artifact_map.get(&rotate_module).unwrap_or_else(|| {
        panic!(
            "rotate_servo_clockwise artifact `{}` is present",
            rotate_module
        )
    });

    // move_arm - constants and struct hierarchy
    assert_contains_all(
        move_arm,
        &[
            "const TARGET_NODE_NAME: &str = \"brain\";",
            "const TARGET_ACTION_NAME: &str = \"move_arm\";",
            "pub struct GoalRequest",
            "pub struct GoalResponseData",
            "pub struct ActionHandle",
            "inner: peppylib::messaging::ActionGoalHandle",
            "messenger: peppylib::MessengerHandle",
            "impl ActionHandle",
            "pub struct ResultResponse",
            "pub struct FeedbackMessage",
            "arm_id: u16",
            "desired_position: [i32; 3]",
        ],
    );

    // move_arm - API calls and helpers
    assert_contains_all(
        move_arm,
        &[
            "peppylib::ActionMessenger::send_goal",
            "peppylib::ActionMessenger::cancel_goal",
            "peppylib::ActionMessenger::request_result",
            "fn deserialize_feedback_payload",
        ],
    );

    // move_arm - format! macro context labels
    assert_contains_all(
        move_arm,
        &[
            "format!(\"{} {} GoalResponse\", TARGET_NODE_NAME, TARGET_ACTION_NAME)",
            r#"TARGET_NODE_NAME, TARGET_ACTION_NAME, "ResultResponse""#,
            "format!(\"{} {} FeedbackMessage\", TARGET_NODE_NAME, TARGET_ACTION_NAME)",
        ],
    );

    // rotate_servo_clockwise - constants and struct hierarchy
    assert_contains_all(
        rotate_servo,
        &[
            "const TARGET_NODE_NAME: &str = \"brain\";",
            "const TARGET_ACTION_NAME: &str = \"rotate_servo_clockwise\";",
            "pub struct GoalResponseData",
            "pub struct ActionHandle",
            "inner: peppylib::messaging::ActionGoalHandle",
            "messenger: peppylib::MessengerHandle",
            "impl ActionHandle",
            "pub struct ResultResponse",
            "pub struct FeedbackMessage",
            "fn deserialize_feedback_payload",
            "-> crate::Result<Self>",
        ],
    );

    // rotate_servo_clockwise - API calls
    assert_contains_all(
        rotate_servo,
        &[
            "peppylib::ActionMessenger::send_goal",
            "peppylib::ActionMessenger::cancel_goal",
            "peppylib::ActionMessenger::request_result",
            "self.inner.on_next_feedback",
        ],
    );

    // rotate_servo_clockwise - format! macro context labels
    assert_contains_all(
        rotate_servo,
        &[
            "format!(\"{} {} GoalResponse\", TARGET_NODE_NAME, TARGET_ACTION_NAME)",
            r#"TARGET_NODE_NAME, TARGET_ACTION_NAME, "ResultResponse""#,
            "format!(\"{} {} FeedbackMessage\", TARGET_NODE_NAME, TARGET_ACTION_NAME)",
        ],
    );
}

/// A real manifest dep (link_id present) splices the runtime binding
/// lookup as the single `target` argument of the generated `send_goal`:
/// `consumer_filter(<link_id>).pinned_target()` resolves at runtime to
/// the bound producer's full `(core_node, instance_id)`, so a pinned
/// slot addresses exactly one producer with no discovery probe.
#[test]
fn consumed_action_with_link_id_splices_runtime_binding_target() {
    let mut action: ConsumedAction = serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE1).unwrap();
    // Production derives the wire link_id and the manifest link_id from the
    // same `depends_on` entry (see core-node `sync`); keep them identical
    // here so the fixture models a reachable input.
    action.link_id = "left_arm".to_owned();
    let goal_request_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_FORMAT1).unwrap();
    let goal_response_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT1).unwrap();
    let format = ConsumedActionMessage {
        goal_request: Some(goal_request_format),
        goal_response: Some(goal_response_format),
        feedback: None,
        result_request: None,
        result_response: None,
    };

    let mut generator = RustGenerator::new();
    generator
        .add_consumed_action(
            &action,
            &format,
            &crate::DependencyContext::native("brain", "v1")
                .with_link_id(crate::WireLinkId::from_link_id("left_arm", false)),
        )
        .unwrap();
    let artifacts = render_artifacts(generator.into_artifacts());
    let rendered = artifacts.into_iter().next().expect("artifact is present");

    assert_contains_all(
        &rendered,
        &[".consumer_filter(\"left_arm\")", ".pinned_target()"],
    );
    assert_rendered!(
        !rendered.contains("Option::<&peppylib::messaging::ProducerRef>::None"),
        rendered,
        "a linked dep must resolve its target from the bindings map, not emit a wildcard",
    );
}

#[test]
fn consumed_action_without_response_payload() {
    let action: ConsumedAction = serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE1).unwrap();
    let goal_request_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_FORMAT1).unwrap();
    let feedback_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_FEEDBACK_FORMAT1).unwrap();

    let format = ConsumedActionMessage {
        goal_request: Some(goal_request_format),
        goal_response: None,
        feedback: Some(feedback_format),
        result_request: None,
        result_response: None,
    };

    let mut generator = RustGenerator::new();
    generator
        .add_consumed_action(
            &action,
            &format,
            &crate::DependencyContext::native("brain", "v1"),
        )
        .expect("generator should allow consumed actions with empty response payloads");
    let artifacts = render_artifacts(generator.into_artifacts());
    assert_eq!(
        artifacts.len(),
        1,
        "expected a single generated artifact, got {}",
        artifacts.len()
    );
    let rendered = artifacts.into_iter().next().expect("artifact is present");

    // The goal acknowledgement is framework-owned, so GoalResponseData and the
    // `pub data` field are always present even when the action declares no goal
    // response format.
    assert_contains_all(
        &rendered,
        &[
            "pub struct ActionHandle",
            "messenger: peppylib::MessengerHandle",
            "inner: peppylib::messaging::ActionGoalHandle",
            "pub data: GoalResponseData",
            "impl ActionHandle",
            "pub async fn fire_goal",
            "pub async fn get_result",
            "peppylib::ActionMessenger::send_goal",
            "peppylib::ActionMessenger::request_result",
            "pub struct GoalResponseData",
            "pub accepted: bool",
            "fn deserialize_goal_response",
        ],
    );
}

#[test]
fn consumed_action_without_feedback() {
    let action: ConsumedAction = serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE1).unwrap();
    let goal_request_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_FORMAT1).unwrap();

    let format = ConsumedActionMessage {
        goal_request: Some(goal_request_format),
        goal_response: None,
        feedback: None,
        result_request: None,
        result_response: None,
    };

    let mut generator = RustGenerator::new();
    generator
        .add_consumed_action(
            &action,
            &format,
            &crate::DependencyContext::native("brain", "v1"),
        )
        .expect("generator should allow consumed actions without feedback payloads");
    let artifacts = render_artifacts(generator.into_artifacts());
    assert_eq!(artifacts.len(), 1, "expected single generated artifact");

    let rendered = &artifacts[0];

    // ActionHandle struct and methods
    assert_contains_all(
        rendered,
        &[
            "pub struct ActionHandle",
            "messenger: peppylib::MessengerHandle",
            "inner: peppylib::messaging::ActionGoalHandle",
            "impl ActionHandle",
            "pub async fn fire_goal",
            "pub async fn get_result",
            "peppylib::ActionMessenger::send_goal",
            "peppylib::ActionMessenger::request_result",
        ],
    );

    // Make sure the related functions for feedback are not present
    assert_rendered!(
        !rendered.contains("pub async fn on_next_feedback_message"),
        rendered,
        "expected generator to skip feedback listener when no feedback payload is provided"
    );
    assert_rendered!(
        !rendered.contains("fn deserialize_feedback_payload"),
        rendered,
        "expected feedback payload helper to be omitted without feedback format"
    );
}

/// This is a long running test
/// Checks for clippy warnings when there is only one exposed action with an empty goal request.
#[test]
fn clippy_single_exposed_action_empty_goal_request() {
    let temp_dir = TempDir::new().unwrap();
    let action: ExposedAction =
        serde_json5::from_str(EXPOSED_ACTION_EXAMPLE_EMPTY_GOAL_REQUEST).unwrap();

    let consumed_action1: ConsumedAction =
        serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE1).unwrap();
    let consumed_action1_goal_request: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_FORMAT1).unwrap();
    let consumed_action1_goal_response: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT1).unwrap();
    let consumed_action1_feedback: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_FEEDBACK_FORMAT1).unwrap();
    let consumed_action1_result_response: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_RESULT_RESPONSE_FORMAT1).unwrap();
    let consumed_action1_messages = ConsumedActionMessage {
        goal_request: Some(consumed_action1_goal_request),
        goal_response: Some(consumed_action1_goal_response),
        feedback: Some(consumed_action1_feedback),
        result_request: None,
        result_response: Some(consumed_action1_result_response),
    };

    let consumed_action2: ConsumedAction =
        serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE2).unwrap();
    let consumed_action2_goal_response: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT2).unwrap();
    let consumed_action2_feedback: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_FEEDBACK_FORMAT2).unwrap();
    let consumed_action2_result_response: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_RESULT_RESPONSE_FORMAT2).unwrap();
    let consumed_action2_messages = ConsumedActionMessage {
        goal_request: None,
        goal_response: Some(consumed_action2_goal_response),
        feedback: Some(consumed_action2_feedback),
        result_request: None,
        result_response: Some(consumed_action2_result_response),
    };

    let (mut generator, output_dir, user_node, _) = init_test_env::<RustGenerator>(&temp_dir);
    generator.add_exposed_action(&action, None).unwrap();
    generator
        .add_consumed_action(
            &consumed_action1,
            &consumed_action1_messages,
            &crate::DependencyContext::native("brain", "v1"),
        )
        .unwrap();
    generator
        .add_consumed_action(
            &consumed_action2,
            &consumed_action2_messages,
            &crate::DependencyContext::native("controller", "v1"),
        )
        .unwrap();
    let output_config = copy_config_to_output(&user_node, &output_dir);
    generator
        .build(
            &output_dir,
            &config::consts::PeppyDirs::default(),
            Default::default(),
        )
        .unwrap();
    fs::remove_file(output_config).unwrap();

    run_clippy(&output_dir);

    let exposed_actions_mod = output_dir.join("src/exposed_actions.rs");
    assert!(
        exposed_actions_mod.exists(),
        "Expected exposed_actions module file so `peppygen::exposed_actions::<action>` resolves"
    );
    let exposed_actions_contents =
        fs::read_to_string(&exposed_actions_mod).expect("failed to read exposed_actions module");
    assert_contains_all(&exposed_actions_contents, &["pub mod move_arm;"]);

    let consumed_actions_mod = output_dir.join("src/consumed_actions.rs");
    assert!(
        consumed_actions_mod.exists(),
        "Expected consumed_actions module file so `peppygen::consumed_actions::<action>` resolves"
    );
    let consumed_actions_contents =
        fs::read_to_string(&consumed_actions_mod).expect("failed to read consumed_actions module");
    assert_contains_all(
        &consumed_actions_contents,
        &[
            "pub mod brain_move_arm;",
            "pub mod controller_rotate_servo_clockwise;",
        ],
    );
}

/// This is a long running test
#[test]
fn compile_lib_with_exposed_and_consumed_actions() {
    let temp_dir = TempDir::new().unwrap();
    let action1: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE).unwrap();
    let action2: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE2).unwrap();

    let consumed_action1: ConsumedAction =
        serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE1).unwrap();
    let consumed_action1_goal_request: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_FORMAT1).unwrap();
    let consumed_action1_goal_response: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT1).unwrap();
    let consumed_action1_feedback: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_FEEDBACK_FORMAT1).unwrap();
    let consumed_action1_result_response: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_RESULT_RESPONSE_FORMAT1).unwrap();
    let consumed_action1_messages = ConsumedActionMessage {
        goal_request: Some(consumed_action1_goal_request),
        goal_response: Some(consumed_action1_goal_response),
        feedback: Some(consumed_action1_feedback),
        result_request: None,
        result_response: Some(consumed_action1_result_response),
    };

    let consumed_action2: ConsumedAction =
        serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE2).unwrap();
    let consumed_action2_goal_response: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT2).unwrap();
    let consumed_action2_feedback: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_FEEDBACK_FORMAT2).unwrap();
    let consumed_action2_result_response: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_RESULT_RESPONSE_FORMAT2).unwrap();
    let consumed_action2_messages = ConsumedActionMessage {
        goal_request: None,
        goal_response: Some(consumed_action2_goal_response),
        feedback: Some(consumed_action2_feedback),
        result_request: None,
        result_response: Some(consumed_action2_result_response),
    };

    let (mut generator, output_dir, user_node, _) = init_test_env::<RustGenerator>(&temp_dir);
    generator.add_exposed_action(&action1, None).unwrap();
    generator.add_exposed_action(&action2, None).unwrap();
    generator
        .add_consumed_action(
            &consumed_action1,
            &consumed_action1_messages,
            &crate::DependencyContext::native("brain", "v1"),
        )
        .unwrap();
    generator
        .add_consumed_action(
            &consumed_action2,
            &consumed_action2_messages,
            &crate::DependencyContext::native("controller", "v1"),
        )
        .unwrap();
    let output_config = copy_config_to_output(&user_node, &output_dir);
    generator
        .build(
            &output_dir,
            &config::consts::PeppyDirs::default(),
            Default::default(),
        )
        .unwrap();
    fs::remove_file(output_config).unwrap();

    run_cargo_build(&output_dir);
    run_clippy(&output_dir);

    let lib_rs = output_dir.join("src/lib.rs");
    assert!(
        lib_rs.exists(),
        "Expected lib.rs to exist so generated action modules are reachable"
    );
    let lib_contents = fs::read_to_string(&lib_rs).expect("failed to read generated lib.rs");
    assert_contains_all(
        &lib_contents,
        &["pub mod exposed_actions;", "pub mod consumed_actions;"],
    );

    let exposed_actions_mod = output_dir.join("src/exposed_actions.rs");
    assert!(
        exposed_actions_mod.exists(),
        "Expected exposed_actions module file so `peppygen::exposed_actions::<action>` resolves"
    );
    let exposed_actions_contents =
        fs::read_to_string(&exposed_actions_mod).expect("failed to read exposed_actions module");
    assert_contains_all(
        &exposed_actions_contents,
        &["pub mod move_arm;", "pub mod rotate_servo_clockwise;"],
    );

    let consumed_actions_mod = output_dir.join("src/consumed_actions.rs");
    assert!(
        consumed_actions_mod.exists(),
        "Expected consumed_actions module file so `peppygen::consumed_actions::<action>` resolves"
    );
    let consumed_actions_contents =
        fs::read_to_string(&consumed_actions_mod).expect("failed to read consumed_actions module");
    assert_contains_all(
        &consumed_actions_contents,
        &[
            "pub mod brain_move_arm;",
            "pub mod controller_rotate_servo_clockwise;",
        ],
    );

    let expose_move_arm_path = output_dir.join("src/exposed_actions/move_arm.rs");
    assert!(
        expose_move_arm_path.exists(),
        "Expected generated move_arm action module at {:?}",
        expose_move_arm_path
    );
    let expose_rotate_path = output_dir.join("src/exposed_actions/rotate_servo_clockwise.rs");
    assert!(
        expose_rotate_path.exists(),
        "Expected generated rotate_servo_clockwise action module at {:?}",
        expose_rotate_path
    );
    let subscribed_brain_path = output_dir.join("src/consumed_actions/brain_move_arm.rs");
    assert!(
        subscribed_brain_path.exists(),
        "Expected brain_move_arm consumed action module at {:?}",
        subscribed_brain_path
    );
    let subscribed_controller_path =
        output_dir.join("src/consumed_actions/controller_rotate_servo_clockwise.rs");
    assert!(
        subscribed_controller_path.exists(),
        "Expected controller_rotate_servo_clockwise consumed action module at {:?}",
        subscribed_controller_path
    );
}

/// Checks for clippy warnings when there is a consumed action with an empty goal request.
#[test]
fn clippy_consumed_action_empty_goal_request() {
    let temp_dir = TempDir::new().unwrap();

    let consumed_action: ConsumedAction = serde_json5::from_str(
        r#"
        {
          link_id: "robot",
          name: "calibrate",
        }
        "#,
    )
    .unwrap();
    let goal_response_format: MessageFormat =
        serde_json5::from_str(r#"{ accepted: "bool" }"#).unwrap();
    let action_messages = ConsumedActionMessage {
        goal_request: None,
        goal_response: Some(goal_response_format),
        feedback: None,
        result_request: None,
        result_response: None,
    };

    let (mut generator, output_dir, user_node, _) = init_test_env::<RustGenerator>(&temp_dir);
    generator
        .add_consumed_action(
            &consumed_action,
            &action_messages,
            &crate::DependencyContext::native("robot", "v1"),
        )
        .unwrap();
    let output_config = copy_config_to_output(&user_node, &output_dir);
    generator
        .build(
            &output_dir,
            &config::consts::PeppyDirs::default(),
            Default::default(),
        )
        .unwrap();
    fs::remove_file(output_config).unwrap();

    run_clippy(&output_dir);
}
