use super::*;

use config::node::{ExposedAction, ConsumedAction};
use std::process::{Command, Stdio};
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
const SUBSCRIBED_ACTION_EXAMPLE1: &str = r#"
{
  id: "brain_move_arm",
  node: "brain",
  name: "move_arm",
  tag: "0.1.0"
}
"#;

const SUBSCRIBED_ACTION_GOAL_FORMAT1: &str = r#"
{
  arm_id: "u16",
  desired_position: {
    $type: "array",
    $items: "i32",
    $length: 3
  }
}
"#;

const SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT1: &str = r#"
{
  accepted: "bool"
}
"#;

const SUBSCRIBED_ACTION_FEEDBACK_FORMAT1: &str = r#"
{
  new_position: {
    $type: "array",
    $items: "i32",
    $length: 3
  }
}
"#;

const SUBSCRIBED_ACTION_RESULT_RESPONSE_FORMAT1: &str = r#"
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
  id: "controller_rotate_servo",
  node: "controller",
  name: "rotate_servo_clockwise",
  tag: "0.1.0"
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
    generator.add_exposed_action(&action).unwrap();
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

    // GoalResponse struct
    assert_contains_all(
        &rendered,
        &[
            "pub struct GoalResponse",
            "impl GoalResponse",
            "pub fn new(accepted: bool) -> Self",
        ],
    );

    // Cancel request/response structs
    assert_contains_all(
        &rendered,
        &[
            "pub struct CancelRequest",
            "pub struct CancelResponse",
            "pub error_message: Option<String>",
            "impl CancelResponse",
            "pub fn new(accepted: bool, error_message: Option<String>) -> Self",
        ],
    );

    // Result request/response structs
    assert_contains_all(
        &rendered,
        &[
            "pub struct ResultRequest",
            "pub struct ResultResponse",
            "final_position: [i32; 3]",
            "success: bool",
            "error_msg: Option<String>",
        ],
    );

    // ActionHandle struct
    assert_contains_all(
        &rendered,
        &[
            "pub struct ActionHandle",
            "goal_service: peppylib::messaging::ServiceEndpoint",
            "cancel_service: peppylib::messaging::ServiceEndpoint",
            "result_service: peppylib::messaging::ServiceEndpoint",
            "feedback_publisher: peppylib::messaging::TopicPublisher",
        ],
    );

    // expose method
    assert_contains_all(
        &rendered,
        &[
            "pub async fn expose(node_runner: &crate::NodeRunner) -> crate::Result<Self>",
            "peppylib::ActionMessenger::expose",
        ],
    );

    // Handler methods
    assert_contains_all(
        &rendered,
        &[
            "pub async fn handle_goal_next_request",
            "F: Fn(GoalRequest) -> crate::Result<GoalResponse>",
            "pub async fn handle_cancel_next_request",
            "F: Fn(CancelRequest) -> crate::Result<CancelResponse>",
            "pub async fn handle_result_next_request",
            "F: Fn(ResultRequest) -> crate::Result<ResultResponse>",
        ],
    );

    // Feedback emit method
    assert_contains_all(
        &rendered,
        &[
            "pub async fn emit_feedback",
            "#[allow(clippy::too_many_arguments)]",
            "self.feedback_publisher.publish(payload).await",
        ],
    );

    // Helper functions
    assert_contains_all(
        &rendered,
        &[
            "fn deserialize_goal_request",
            "fn handle_goal_payload",
            "fn handle_cancel_payload",
            "fn handle_result_payload",
        ],
    );
}

#[test]
fn expose_action_without_request_body() {
    let action: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE2).unwrap();

    let mut generator = RustGenerator::new();
    generator.add_exposed_action(&action).unwrap();
    let rendered = render_artifacts(generator.into_artifacts())
        .into_iter()
        .next()
        .expect("artifact is present");

    // GoalRequest still exists (with instance_id and core_node) even without data
    assert_contains_all(
        &rendered,
        &[
            "pub struct GoalRequest",
            "pub struct GoalResponse",
            "pub async fn handle_goal_next_request",
            "F: Fn(GoalRequest) -> crate::Result<GoalResponse>",
            "fn handle_goal_payload",
        ],
    );

    // Result handling
    assert_contains_all(
        &rendered,
        &[
            "pub struct ResultResponse",
            "success: bool",
            "error_msg: Option<String>",
            "pub async fn handle_result_next_request",
            "F: Fn(ResultRequest) -> crate::Result<ResultResponse>",
            "fn handle_result_payload",
        ],
    );

    // Cancel handling
    assert_contains_all(
        &rendered,
        &[
            "pub struct CancelResponse",
            "pub async fn handle_cancel_next_request",
            "F: Fn(CancelRequest) -> crate::Result<CancelResponse>",
        ],
    );

    // Feedback emitter
    assert_contains_all(&rendered, &["pub async fn emit_feedback"]);
}

#[test]
fn exposed_action_rejects_reserved_message_field_name() {
    use crate::error::Error;

    let action: ExposedAction =
        serde_json5::from_str(EXPOSED_ACTION_RESERVED_FEEDBACK_FIELD_EXAMPLE).unwrap();

    let mut generator = RustGenerator::new();
    let err = generator.add_exposed_action(&action).unwrap_err();

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
fn expose_action_with_feedback_only_initializes_existing_fields() {
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
    generator.add_exposed_action(&action).unwrap();
    let rendered = render_artifacts(generator.into_artifacts())
        .into_iter()
        .next()
        .expect("artifact is present");

    assert_contains_all(
        &rendered,
        &[
            "pub struct ActionHandle",
            "feedback_publisher: peppylib::messaging::TopicPublisher",
            "pub async fn expose(node_runner: &crate::NodeRunner) -> crate::Result<Self>",
            "feedback_publisher: action.feedback_publisher",
        ],
    );

    assert_rendered!(
        !rendered.contains("goal_service: action.goal_service"),
        rendered,
        "expose method should not initialize goal_service when goal service is missing"
    );
    assert_rendered!(
        !rendered.contains("cancel_service: action.cancel_service"),
        rendered,
        "expose method should not initialize cancel_service when goal service is missing"
    );
    assert_rendered!(
        !rendered.contains("result_service: action.result_service"),
        rendered,
        "expose method should not initialize result_service when result service is missing"
    );
}

#[test]
fn expose_two_actions() {
    let action1: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE).unwrap();
    let action2: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE2).unwrap();

    let mut generator = RustGenerator::new();
    generator.add_exposed_action(&action1).unwrap();
    generator.add_exposed_action(&action2).unwrap();

    let artifacts = generator.into_artifacts();
    assert_eq!(
        artifacts.len(),
        2,
        "expected two generated artifacts, got {}",
        artifacts.len()
    );

    let artifact_map: HashMap<_, _> = artifacts
        .into_iter()
        .map(|artifact| (artifact.node_name, artifact.code_output))
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
            "pub struct ResultResponse",
            "pub async fn emit_feedback",
            "const ACTION_NAME: &str = \"move_arm\";",
        ],
    );

    // rotate_servo_clockwise checks
    assert_contains_all(
        rotate_servo,
        &[
            "pub async fn handle_goal_next_request",
            "F: Fn(GoalRequest) -> crate::Result<GoalResponse>",
            "pub struct GoalResponse",
            "pub struct ResultResponse",
            "pub async fn emit_feedback",
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
    generator.add_consumed_action(&action, &format).unwrap();
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

    // CancelResponseData and CancelResponse structs
    assert_contains_all(
        &rendered,
        &[
            "pub struct CancelResponseData",
            "pub error_message: Option<String>",
            "pub struct CancelResponse",
        ],
    );

    // ResultResponseData and ResultResponse structs
    assert_contains_all(
        &rendered,
        &[
            "pub struct ResultResponseData",
            "pub final_position: [i32; 3]",
            "pub struct ResultResponse",
            "pub core_node: String",
            "pub instance_id: String",
        ],
    );

    // FeedbackMessage struct
    assert_contains_all(
        &rendered,
        &["pub struct FeedbackMessage", "pub new_position: [i32; 3]"],
    );

    // fire_goal method (constructor)
    assert_contains_all(
        &rendered,
        &[
            "pub async fn fire_goal(",
            "request: GoalRequest",
            "feedback_qos: peppylib::config::QoSProfile",
            "-> crate::Result<Self>",
            "peppylib::ActionMessenger::send_goal",
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

    let mut rotate_action: ConsumedAction =
        serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE2).unwrap();
    // Reuse the same upstream node so both subscriptions target the same source.
    rotate_action.node = move_arm_action.node.clone();
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
        .add_consumed_action(&move_arm_action, &move_arm_messages)
        .unwrap();
    generator
        .add_consumed_action(&rotate_action, &rotate_messages)
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
        .map(|artifact| (artifact.node_name, artifact.code_output))
        .collect();

    let move_arm_module = module_name_from_components(&move_arm_action.node, &move_arm_action.name);
    let rotate_module = module_name_from_components(&rotate_action.node, &rotate_action.name);

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
        .add_consumed_action(&action, &format)
        .expect("generator should allow consumed actions with empty response payloads");
    let artifacts = render_artifacts(generator.into_artifacts());
    assert_eq!(
        artifacts.len(),
        1,
        "expected a single generated artifact, got {}",
        artifacts.len()
    );
    let rendered = artifacts.into_iter().next().expect("artifact is present");

    // ActionHandle struct without GoalResponseData (no goal response format)
    assert_contains_all(
        &rendered,
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
    assert_rendered!(
        !rendered.contains("GoalResponseData"),
        rendered,
        "expected no GoalResponseData when goal response format is absent"
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
        .add_consumed_action(&action, &format)
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
    generator.add_exposed_action(&action).unwrap();
    generator
        .add_consumed_action(&consumed_action1, &consumed_action1_messages)
        .unwrap();
    generator
        .add_consumed_action(&consumed_action2, &consumed_action2_messages)
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

    let clippy_output = Command::new("cargo")
        .arg("clippy")
        .arg("--all-targets")
        .arg("--color")
        .arg("always")
        .arg("--")
        .arg("-D")
        .arg("warnings")
        .env("CARGO_NET_OFFLINE", "true")
        .current_dir(&output_dir)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run cargo clippy on generated crate");
    assert!(
        clippy_output.status.success(),
        "cargo clippy failed for generated crate with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        clippy_output.status.code(),
        String::from_utf8_lossy(&clippy_output.stdout),
        String::from_utf8_lossy(&clippy_output.stderr)
    );

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
    let consumed_actions_contents = fs::read_to_string(&consumed_actions_mod)
        .expect("failed to read consumed_actions module");
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
    generator.add_exposed_action(&action1).unwrap();
    generator.add_exposed_action(&action2).unwrap();
    generator
        .add_consumed_action(&consumed_action1, &consumed_action1_messages)
        .unwrap();
    generator
        .add_consumed_action(&consumed_action2, &consumed_action2_messages)
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

    let cargo_output = Command::new("cargo")
        .arg("build")
        .env("CARGO_NET_OFFLINE", "true")
        .current_dir(&output_dir)
        .stdin(Stdio::null())
        .output()
        .expect("failed to invoke cargo build on generated crate");
    assert!(
        cargo_output.status.success(),
        "cargo build failed for generated crate with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        cargo_output.status.code(),
        String::from_utf8_lossy(&cargo_output.stdout),
        String::from_utf8_lossy(&cargo_output.stderr)
    );

    let clippy_output = Command::new("cargo")
        .arg("clippy")
        .arg("--all-targets")
        .arg("--color")
        .arg("always")
        .arg("--")
        .arg("-D")
        .arg("warnings")
        .env("CARGO_NET_OFFLINE", "true")
        .current_dir(&output_dir)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run cargo clippy on generated crate");
    assert!(
        clippy_output.status.success(),
        "cargo clippy failed for generated crate with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        clippy_output.status.code(),
        String::from_utf8_lossy(&clippy_output.stdout),
        String::from_utf8_lossy(&clippy_output.stderr)
    );

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
    let consumed_actions_contents = fs::read_to_string(&consumed_actions_mod)
        .expect("failed to read consumed_actions module");
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
          id: "robot_calibrate",
          node: "robot",
          name: "calibrate",
          tag: "0.1.0"
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
        .add_consumed_action(&consumed_action, &action_messages)
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

    let clippy_output = Command::new("cargo")
        .arg("clippy")
        .arg("--all-targets")
        .arg("--color")
        .arg("always")
        .arg("--")
        .arg("-D")
        .arg("warnings")
        .env("CARGO_NET_OFFLINE", "true")
        .current_dir(&output_dir)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run cargo clippy on generated crate");
    assert!(
        clippy_output.status.success(),
        "cargo clippy failed for generated crate with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        clippy_output.status.code(),
        String::from_utf8_lossy(&clippy_output.stdout),
        String::from_utf8_lossy(&clippy_output.stderr)
    );
}
