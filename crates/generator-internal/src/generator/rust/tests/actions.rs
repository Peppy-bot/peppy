use super::*;

use config::node::{ExposedAction, SubscribedAction};
use std::collections::HashMap;

// --- Exposes examples
const EXPOSED_ACTION_EXAMPLE: &str = r#"
{
  name: "move_arm",
  goal_service: {
    request_message_format: {
      arm_id: "u16",
      desired_position: {
        type: "array",
        items: "i32",
        length: 3
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
        type: "array",
        items: "i32",
        length: 3
      }
    }
  },
  result_service: {
    response_message_format: {
      success: "bool",
      error_msg: "string",
      final_position: {
        type: "array",
        items: "i32",
        length: 3
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
      error_msg: "string"
    }
  }
}
"#;

// --- Subscribes examples
const SUBSCRIBED_ACTION_EXAMPLE: &str = r#"
{
  node: "brain",
  name: "move_arm",
  tag: "0.1.0"
}
"#;

const SUBSCRIBED_ACTION_GOAL_FORMAT: &str = r#"
{
  arm_id: "u16",
  desired_position: {
    type: "array",
    items: "i32",
    length: 3
  }
}
"#;

const SUBSCRIBED_ACTION_FEEDBACK_FORMAT: &str = r#"
{
  new_position: {
    type: "array",
    items: "i32",
    length: 3
  }
}
"#;

const SUBSCRIBED_ACTION_RESULT_FORMAT: &str = r#"
{
  success: "bool",
  error_msg: "string",
  final_position: {
    type: "array",
    items: "i32",
    length: 3
  }
}
"#;

#[test]
fn exposed_action() {
    let action: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE).unwrap();

    let mut generator = RustGenerator::new();
    generator.add_exposed_action(&action).unwrap();
    let artifacts: Vec<String> = generator
        .into_artifacts()
        .into_iter()
        .map(|artifact| artifact.code_output)
        .collect();
    assert_eq!(
        artifacts.len(),
        1,
        "expected a single generated artifact, got {}",
        artifacts.len()
    );
    let rendered = artifacts.into_iter().next().expect("artifact is present");

    assert_rendered!(
        rendered.contains("pub struct MoveArmGoalRequest"),
        &rendered,
        "expected goal request struct"
    );
    assert_rendered!(
        rendered.contains("pub arm_id: u16"),
        &rendered,
        "expected arm_id field in goal request"
    );
    assert_rendered!(
        rendered.contains("desired_position: [i32; 3]"),
        &rendered,
        "expected desired_position array in goal request"
    );
    assert_rendered!(
        rendered.contains("pub struct MoveArmGoalResponse"),
        &rendered,
        "expected goal response struct"
    );
    assert_rendered!(
        rendered.contains("impl MoveArmGoalResponse"),
        &rendered,
        "expected goal response constructor block"
    );
    assert_rendered!(
        rendered.contains("pub fn new(accepted: bool) -> Self"),
        &rendered,
        "expected goal response constructor signature"
    );
    assert_rendered!(
        rendered.contains("final_position: [i32; 3]"),
        &rendered,
        "expected final_position field in result response"
    );
    assert_rendered!(
        rendered.contains("pub struct MoveArmResultResponse"),
        &rendered,
        "expected result response struct"
    );
    assert_rendered!(
        rendered.contains("success: bool"),
        &rendered,
        "expected success field in result response"
    );
    assert_rendered!(
        rendered.contains("error_msg: String"),
        &rendered,
        "expected error_msg field in result response"
    );
    assert_rendered!(
        rendered.contains("pub struct MoveArmAction;"),
        &rendered,
        "expected action marker struct"
    );
    assert_rendered!(
        rendered.contains("pub async fn handle_move_arm_goal_next_request"),
        &rendered,
        "expected goal handler method"
    );
    assert_rendered!(
        rendered.contains("F: Fn(MoveArmGoalRequest) -> crate::Result<MoveArmGoalResponse>"),
        &rendered,
        "expected goal handler signature"
    );
    assert_rendered!(
        rendered.contains("let service_name = \"move_arm/goal\";"),
        &rendered,
        "expected goal service name literal"
    );
    assert_rendered!(
        rendered.contains("pub async fn handle_move_arm_goal_cancel_request"),
        &rendered,
        "expected cancel handler method"
    );
    assert_rendered!(
        rendered.contains("F: Fn() -> crate::Result<()>"),
        &rendered,
        "expected cancel handler signature"
    );
    assert_rendered!(
        rendered.contains("let service_name = \"move_arm/cancel\";"),
        &rendered,
        "expected cancel service name"
    );
    assert_rendered!(
        rendered.contains("pub async fn emit_move_arm_feedback"),
        &rendered,
        "expected feedback emit method"
    );
    assert_rendered!(
        rendered.contains("let topic_name = \"move_arm/feedback\";"),
        &rendered,
        "expected feedback topic literal"
    );
    assert_rendered!(
        rendered.contains("#[allow(clippy::too_many_arguments)]"),
        &rendered,
        "expected clippy allowance on feedback emitter"
    );
    assert_rendered!(
        rendered.contains("peppylib::TopicMessenger::emit"),
        &rendered,
        "expected topic messenger emit call"
    );
    assert_rendered!(
        rendered.contains("peppylib::ServiceMessenger::listen"),
        &rendered,
        "expected service listen call for action endpoints"
    );
    assert_rendered!(
        rendered.contains("pub async fn handle_move_arm_result_next_request"),
        &rendered,
        "expected result handler method"
    );
    assert_rendered!(
        rendered.contains("F: Fn() -> crate::Result<MoveArmResultResponse>"),
        &rendered,
        "expected result handler signature"
    );
}

#[test]
fn expose_action_without_request_body() {
    let action: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE2).unwrap();

    let mut generator = RustGenerator::new();
    generator.add_exposed_action(&action).unwrap();
    let artifacts: Vec<String> = generator
        .into_artifacts()
        .into_iter()
        .map(|artifact| artifact.code_output)
        .collect();
    assert_eq!(
        artifacts.len(),
        1,
        "expected a single generated artifact, got {}",
        artifacts.len()
    );
    let rendered = artifacts.into_iter().next().expect("artifact is present");

    assert_rendered!(
        rendered.contains("pub struct RotateServoClockwiseGoalResponse"),
        &rendered,
        "expected goal response struct without request body"
    );
    assert_rendered!(
        !rendered.contains("RotateServoClockwiseGoalRequest"),
        &rendered,
        "expected no goal request struct when goal request body is missing"
    );
    assert_rendered!(
        rendered.contains("pub async fn handle_rotate_servo_clockwise_goal_next_request"),
        &rendered,
        "expected goal handler even when goal request body is missing"
    );
    assert_rendered!(
        rendered.contains("F: Fn() -> crate::Result<RotateServoClockwiseGoalResponse>"),
        &rendered,
        "expected goal handler signature with zero parameters"
    );
    assert_rendered!(
        rendered.contains("fn rotate_servo_clockwise_goal_handle_request_payload"),
        &rendered,
        "expected helper function for goal handler without request body"
    );
    assert_rendered!(
        !rendered.contains("fn rotate_servo_clockwise_goal_deserialize_request"),
        &rendered,
        "expected no goal request deserializer when there is no request schema"
    );

    assert_rendered!(
        rendered.contains("pub struct RotateServoClockwiseResultResponse"),
        &rendered,
        "expected result response struct without request body"
    );
    assert_rendered!(
        !rendered.contains("RotateServoClockwiseResultRequest"),
        &rendered,
        "expected no result request struct when result request body is missing"
    );
    assert_rendered!(
        rendered.contains("success: bool"),
        &rendered,
        "expected result response to expose success field"
    );
    assert_rendered!(
        rendered.contains("error_msg: String"),
        &rendered,
        "expected result response to expose error message field"
    );
    assert_rendered!(
        rendered.contains("pub async fn handle_rotate_servo_clockwise_result_next_request"),
        &rendered,
        "expected result handler even when result request body is missing"
    );
    assert_rendered!(
        rendered.contains("F: Fn() -> crate::Result<RotateServoClockwiseResultResponse>"),
        &rendered,
        "expected result handler signature with zero parameters"
    );
    assert_rendered!(
        rendered.contains("fn rotate_servo_clockwise_result_handle_request_payload"),
        &rendered,
        "expected helper function for result handler without request body"
    );
    assert_rendered!(
        !rendered.contains("fn rotate_servo_clockwise_result_deserialize_request"),
        &rendered,
        "expected no result request deserializer when there is no request schema"
    );

    assert_rendered!(
        rendered.contains("pub async fn emit_rotate_servo_clockwise_feedback"),
        &rendered,
        "expected feedback emitter for action without request payloads"
    );
    assert_rendered!(
        rendered.contains("pub struct RotateServoClockwiseAction;"),
        &rendered,
        "expected action marker struct even when request payloads are absent"
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

    assert_rendered!(
        move_arm.contains("pub struct MoveArmAction;"),
        &move_arm,
        "expected action marker struct for `move_arm`"
    );
    assert_rendered!(
        move_arm.contains("pub async fn handle_move_arm_goal_next_request"),
        &move_arm,
        "expected goal handler for `move_arm`"
    );
    assert_rendered!(
        move_arm.contains("pub struct MoveArmGoalRequest"),
        &move_arm,
        "expected goal request struct for `move_arm`"
    );
    assert_rendered!(
        move_arm.contains("pub struct MoveArmResultResponse"),
        &move_arm,
        "expected result response struct for `move_arm`"
    );
    assert_rendered!(
        move_arm.contains("pub async fn emit_move_arm_feedback"),
        &move_arm,
        "expected feedback emitter for `move_arm`"
    );

    assert_rendered!(
        rotate_servo.contains("pub struct RotateServoClockwiseAction;"),
        &rotate_servo,
        "expected action marker struct for `rotate_servo_clockwise`"
    );
    assert_rendered!(
        rotate_servo.contains("pub async fn handle_rotate_servo_clockwise_goal_next_request"),
        &rotate_servo,
        "expected goal handler for `rotate_servo_clockwise`"
    );
    assert_rendered!(
        rotate_servo.contains("F: Fn() -> crate::Result<RotateServoClockwiseGoalResponse>"),
        &rotate_servo,
        "expected zero-argument goal handler signature for `rotate_servo_clockwise`"
    );
    assert_rendered!(
        !rotate_servo.contains("RotateServoClockwiseGoalRequest"),
        &rotate_servo,
        "expected no goal request struct when goal payload is absent for `rotate_servo_clockwise`"
    );
    assert_rendered!(
        rotate_servo.contains("pub struct RotateServoClockwiseGoalResponse"),
        &rotate_servo,
        "expected goal response struct for `rotate_servo_clockwise`"
    );
    assert_rendered!(
        rotate_servo.contains("pub struct RotateServoClockwiseResultResponse"),
        &rotate_servo,
        "expected result response struct for `rotate_servo_clockwise`"
    );
    assert_rendered!(
        rotate_servo.contains("pub async fn emit_rotate_servo_clockwise_feedback"),
        &rotate_servo,
        "expected feedback emitter for `rotate_servo_clockwise`"
    );
}

#[test]
fn subscribed_to_action() {
    let action: SubscribedAction = serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE).unwrap();
    let goal_format: MessageFormat = serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_FORMAT).unwrap();
    let feedback_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_FEEDBACK_FORMAT).unwrap();
    let result_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_RESULT_FORMAT).unwrap();
    let format = SubscribedActionMessage {
        goal: goal_format,
        feedback: feedback_format,
        result: result_format,
    };

    let mut generator = RustGenerator::new();
    generator
        .add_subscribed_action(&action, Some(&format))
        .unwrap();
    let artifacts: Vec<String> = generator
        .into_artifacts()
        .into_iter()
        .map(|artifact| artifact.code_output)
        .collect();
    assert_eq!(
        artifacts.len(),
        1,
        "expected a single generated artifact, got {}",
        artifacts.len()
    );
    let rendered = artifacts.into_iter().next().expect("artifact is present");

    todo!("Finish")
}

#[test]
fn subscribed_to_two_actions_same_node() {
    todo!("Finish")
}

#[test]
fn subscribed_action_without_response_payload() {
    todo!("Finish")
}

#[test]
fn compile_lib_with_exposed_action_artifact() {
    let temp_dir = TempDir::new().unwrap();
    let action: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE).unwrap();

    let (mut generator, output_dir, user_node) = init_test_env(&temp_dir);
    generator.add_exposed_action(&action).unwrap();
    let output_config = copy_config_to_output(&user_node, &output_dir);
    generator.build(&output_dir).unwrap();
    fs::remove_file(output_config).unwrap();

    todo!("Finish")
}
