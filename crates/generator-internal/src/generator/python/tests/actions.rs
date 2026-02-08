use super::*;
use config::node::{ExposedAction, MessageFormat, SubscribedAction};
use std::collections::HashMap;

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
      new_position: "i32",
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

    let mut generator = PythonGenerator::new();
    generator.add_exposed_action(&action).unwrap();
    let artifacts = render_artifacts(generator);
    assert_eq!(
        artifacts.len(),
        1,
        "expected a single generated artifact, got {}",
        artifacts.len()
    );
    let rendered = artifacts.into_iter().next().expect("artifact is present");

    // ACTION_NAME constant and GoalRequestData dataclass
    assert_contains_all(
        &rendered,
        &[
            "\"move_arm\"",
            "@dataclass",
            "class GoalRequestData:",
            "arm_id: int",
            "desired_position: list[int]",
        ],
    );

    // GoalRequest dataclass
    assert_contains_all(
        &rendered,
        &[
            "class GoalRequest:",
            "instance_id: str",
            "master_node: str",
            "data: GoalRequestData",
        ],
    );

    // GoalResponse dataclass
    assert_contains_all(&rendered, &["class GoalResponse:"]);

    // Cancel request/response dataclasses
    assert_contains_all(
        &rendered,
        &[
            "class CancelRequest:",
            "class CancelResponse:",
            "error_message: Optional[str]",
        ],
    );

    // Result request/response dataclasses
    assert_contains_all(
        &rendered,
        &[
            "class ResultRequest:",
            "class ResultResponse:",
            "final_position: list[int]",
            "success: bool",
            "error_msg: Optional[str]",
        ],
    );

    // ActionHandle class
    assert_contains_all(&rendered, &["class ActionHandle:"]);

    // expose method
    assert_contains_all(
        &rendered,
        &["async def expose(", "peppylib.ActionMessenger.expose("],
    );

    // Handler methods
    assert_contains_all(
        &rendered,
        &[
            "async def handle_goal_next_request(",
            "async def handle_cancel_next_request(",
            "async def handle_result_next_request(",
        ],
    );

    // Feedback emit method
    assert_contains_all(&rendered, &["async def emit_feedback("]);
}

#[test]
fn expose_action_without_request_body() {
    let action: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE2).unwrap();

    let mut generator = PythonGenerator::new();
    generator.add_exposed_action(&action).unwrap();
    let rendered = render_artifacts(generator)
        .into_iter()
        .next()
        .expect("artifact is present");

    // GoalRequest still exists (with metadata) even without request data
    assert_contains_all(
        &rendered,
        &[
            "class GoalRequest:",
            "class GoalResponse:",
            "async def handle_goal_next_request(",
        ],
    );

    // Result handling
    assert_contains_all(
        &rendered,
        &[
            "class ResultResponse:",
            "success: bool",
            "error_msg: Optional[str]",
            "async def handle_result_next_request(",
        ],
    );

    // Cancel handling
    assert_contains_all(
        &rendered,
        &[
            "class CancelResponse:",
            "async def handle_cancel_next_request(",
        ],
    );

    // Feedback emitter
    assert_contains_all(&rendered, &["async def emit_feedback("]);
}

#[test]
fn expose_two_actions() {
    let action1: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE).unwrap();
    let action2: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE2).unwrap();

    let mut generator = PythonGenerator::new();
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
            "async def handle_goal_next_request(",
            "class GoalRequest:",
            "class ResultResponse:",
            "async def emit_feedback(",
            "\"move_arm\"",
        ],
    );

    // rotate_servo_clockwise checks
    assert_contains_all(
        rotate_servo,
        &[
            "async def handle_goal_next_request(",
            "class GoalResponse:",
            "class ResultResponse:",
            "async def emit_feedback(",
            "\"rotate_servo_clockwise\"",
        ],
    );
}

#[test]
fn subscribed_to_action() {
    let action: SubscribedAction = serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE1).unwrap();
    let goal_request_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_FORMAT1).unwrap();
    let goal_response_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT1).unwrap();
    let feedback_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_FEEDBACK_FORMAT1).unwrap();
    let result_response_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_RESULT_RESPONSE_FORMAT1).unwrap();
    let format = SubscribedActionMessage {
        goal_request: Some(goal_request_format),
        goal_response: Some(goal_response_format),
        feedback: Some(feedback_format),
        result_request: None,
        result_response: Some(result_response_format),
    };

    let mut generator = PythonGenerator::new();
    generator.add_subscribed_action(&action, &format).unwrap();
    let artifacts = render_artifacts(generator);
    assert_eq!(
        artifacts.len(),
        1,
        "expected a single generated artifact, got {}",
        artifacts.len()
    );
    let rendered = artifacts.into_iter().next().expect("artifact is present");

    // Constants
    assert_contains_all(&rendered, &["\"brain\"", "\"move_arm\""]);

    // GoalRequest dataclass
    assert_contains_all(
        &rendered,
        &[
            "class GoalRequest:",
            "arm_id: int",
            "desired_position: list[int]",
        ],
    );

    // GoalResponseData and GoalResponse dataclasses
    assert_contains_all(
        &rendered,
        &[
            "class GoalResponseData:",
            "accepted: bool",
            "class GoalResponse:",
        ],
    );

    // CancelResponseData and CancelResponse dataclasses
    assert_contains_all(
        &rendered,
        &[
            "class CancelResponseData:",
            "error_message: Optional[str]",
            "class CancelResponse:",
        ],
    );

    // ResultResponseData and ResultResponse dataclasses
    assert_contains_all(
        &rendered,
        &[
            "class ResultResponseData:",
            "final_position: list[int]",
            "class ResultResponse:",
            "master_node: str",
            "instance_id: str",
        ],
    );

    // FeedbackMessage dataclass
    assert_contains_all(
        &rendered,
        &["class FeedbackMessage:", "new_position: list[int]"],
    );

    // fire_goal method
    assert_contains_all(
        &rendered,
        &[
            "async def fire_goal(",
            "peppylib.ActionMessenger.send_goal(",
        ],
    );

    // cancel_goal method
    assert_contains_all(
        &rendered,
        &[
            "async def cancel_goal(",
            "peppylib.ActionMessenger.cancel_goal(",
        ],
    );

    // on_next_feedback_message method
    assert_contains_all(&rendered, &["async def on_next_feedback_message("]);

    // get_result method
    assert_contains_all(
        &rendered,
        &[
            "async def get_result(",
            "peppylib.ActionMessenger.request_result(",
        ],
    );
}

#[test]
fn subscribed_to_two_actions_same_node() {
    let move_arm_action: SubscribedAction =
        serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE1).unwrap();
    let move_arm_goal_request: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_FORMAT1).unwrap();
    let move_arm_goal_response: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT1).unwrap();
    let move_arm_feedback: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_FEEDBACK_FORMAT1).unwrap();
    let move_arm_result_response: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_RESULT_RESPONSE_FORMAT1).unwrap();
    let move_arm_messages = SubscribedActionMessage {
        goal_request: Some(move_arm_goal_request),
        goal_response: Some(move_arm_goal_response),
        feedback: Some(move_arm_feedback),
        result_request: None,
        result_response: Some(move_arm_result_response),
    };

    let mut rotate_action: SubscribedAction =
        serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE2).unwrap();
    // Reuse the same upstream node so both subscriptions target the same source.
    rotate_action.node = move_arm_action.node.clone();
    let rotate_goal_response: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT2).unwrap();
    let rotate_feedback: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_FEEDBACK_FORMAT2).unwrap();
    let rotate_result_response: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_RESULT_RESPONSE_FORMAT2).unwrap();
    let rotate_messages = SubscribedActionMessage {
        goal_request: None,
        goal_response: Some(rotate_goal_response),
        feedback: Some(rotate_feedback),
        result_request: None,
        result_response: Some(rotate_result_response),
    };

    let mut generator = PythonGenerator::new();
    generator
        .add_subscribed_action(&move_arm_action, &move_arm_messages)
        .unwrap();
    generator
        .add_subscribed_action(&rotate_action, &rotate_messages)
        .unwrap();

    let artifacts: Vec<_> = generator.into_artifacts();
    assert_eq!(
        artifacts.len(),
        2,
        "expected two generated artifacts, got {}",
        artifacts.len()
    );

    // Find each artifact by checking which one contains the action name
    let (move_arm, rotate_servo) = if artifacts[0].code_output.contains("\"move_arm\"") {
        (&artifacts[0].code_output, &artifacts[1].code_output)
    } else {
        (&artifacts[1].code_output, &artifacts[0].code_output)
    };

    // move_arm - constants and dataclass hierarchy
    assert_contains_all(
        move_arm,
        &[
            "\"brain\"",
            "\"move_arm\"",
            "class GoalRequest:",
            "class GoalResponseData:",
            "class GoalResponse:",
            "class ResultResponse:",
            "class FeedbackMessage:",
            "arm_id: int",
            "desired_position: list[int]",
        ],
    );

    // move_arm - API calls
    assert_contains_all(
        move_arm,
        &[
            "peppylib.ActionMessenger.send_goal(",
            "peppylib.ActionMessenger.cancel_goal(",
            "peppylib.ActionMessenger.request_result(",
        ],
    );

    // rotate_servo_clockwise - constants and dataclass hierarchy
    assert_contains_all(
        rotate_servo,
        &[
            "\"brain\"",
            "\"rotate_servo_clockwise\"",
            "class GoalResponseData:",
            "class GoalResponse:",
            "class ResultResponse:",
            "class FeedbackMessage:",
        ],
    );

    // rotate_servo_clockwise - API calls
    assert_contains_all(
        rotate_servo,
        &[
            "peppylib.ActionMessenger.send_goal(",
            "peppylib.ActionMessenger.cancel_goal(",
            "peppylib.ActionMessenger.request_result(",
        ],
    );
}

#[test]
fn subscribed_action_without_response_payload() {
    let action: SubscribedAction = serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE1).unwrap();
    let goal_request_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_FORMAT1).unwrap();
    let feedback_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_FEEDBACK_FORMAT1).unwrap();

    let format = SubscribedActionMessage {
        goal_request: Some(goal_request_format),
        goal_response: None,
        feedback: Some(feedback_format),
        result_request: None,
        result_response: None,
    };

    let mut generator = PythonGenerator::new();
    generator
        .add_subscribed_action(&action, &format)
        .expect("generator should allow subscribed actions with empty response payloads");
    let artifacts = render_artifacts(generator);
    assert_eq!(
        artifacts.len(),
        1,
        "expected a single generated artifact, got {}",
        artifacts.len()
    );
    let rendered = artifacts.into_iter().next().expect("artifact is present");

    assert_contains_all(
        &rendered,
        &[
            "async def fire_goal(",
            "async def get_result(",
            "peppylib.ActionMessenger.send_goal(",
            "peppylib.ActionMessenger.request_result(",
        ],
    );
}

#[test]
fn subscribed_action_without_feedback() {
    let action: SubscribedAction = serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE1).unwrap();
    let goal_request_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_FORMAT1).unwrap();

    let format = SubscribedActionMessage {
        goal_request: Some(goal_request_format),
        goal_response: None,
        feedback: None,
        result_request: None,
        result_response: None,
    };

    let mut generator = PythonGenerator::new();
    generator
        .add_subscribed_action(&action, &format)
        .expect("generator should allow subscribed actions without feedback payloads");
    let artifacts = render_artifacts(generator);
    assert_eq!(artifacts.len(), 1, "expected single generated artifact");

    let rendered = &artifacts[0];

    // Should still emit goal and result methods
    assert_contains_all(
        rendered,
        &[
            "async def fire_goal(",
            "async def get_result(",
            "peppylib.ActionMessenger.send_goal(",
            "peppylib.ActionMessenger.request_result(",
        ],
    );

    // Make sure the related functions for feedback are not present
    assert_rendered!(
        !rendered.contains("async def on_next_feedback_message"),
        rendered,
        "expected generator to skip feedback listener when no feedback payload is provided"
    );
}
