use super::*;

use config::node::{ExposedAction, SubscribedAction};
use std::{collections::HashMap, fs};

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
const SUBSCRIBED_ACTION_EXAMPLE1: &str = r#"
{
  node: "brain",
  name: "move_arm",
  tag: "0.1.0"
}
"#;

const SUBSCRIBED_ACTION_GOAL_FORMAT1: &str = r#"
{
  arm_id: "u16",
  desired_position: {
    type: "array",
    items: "i32",
    length: 3
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
    type: "array",
    items: "i32",
    length: 3
  }
}
"#;

const SUBSCRIBED_ACTION_RESULT_RESPONSE_FORMAT1: &str = r#"
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

const SUBSCRIBED_ACTION_EXAMPLE2: &str = r#"
{
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
  error_msg: "string"
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
        rendered.contains("pub struct GoalRequest"),
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
        rendered.contains("pub struct GoalResponse"),
        &rendered,
        "expected goal response struct"
    );
    assert_rendered!(
        rendered.contains("impl GoalResponse"),
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
        rendered.contains("pub struct ResultResponse"),
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
        rendered.contains("pub async fn handle_goal_next_request"),
        &rendered,
        "expected goal handler method"
    );
    assert_rendered!(
        rendered.contains("F: Fn(GoalRequest) -> crate::Result<GoalResponse>"),
        &rendered,
        "expected goal handler signature"
    );
    assert_rendered!(
        rendered.contains("let service_name = \"move_arm/goal\";"),
        &rendered,
        "expected goal service name literal"
    );
    assert_rendered!(
        rendered.contains("pub async fn handle_cancel_next_request"),
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
        rendered.contains("pub async fn emit_feedback"),
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
        rendered.contains("pub async fn handle_result_next_request"),
        &rendered,
        "expected result handler method"
    );
    assert_rendered!(
        rendered.contains("F: Fn() -> crate::Result<ResultResponse>"),
        &rendered,
        "expected result handler signature"
    );
    assert_rendered!(
        rendered.contains("fn handle_goal_payload"),
        &rendered,
        "expected goal payload helper"
    );
    assert_rendered!(
        rendered.contains("fn handle_cancel_request_payload"),
        &rendered,
        "expected cancel payload helper"
    );
    assert_rendered!(
        rendered.contains("fn handle_result_payload"),
        &rendered,
        "expected result payload helper"
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
        rendered.contains("pub struct GoalResponse"),
        &rendered,
        "expected goal response struct without request body"
    );
    assert_rendered!(
        rendered.contains("pub async fn handle_goal_next_request"),
        &rendered,
        "expected goal handler even when goal request body is missing"
    );
    assert_rendered!(
        rendered.contains("F: Fn() -> crate::Result<GoalResponse>"),
        &rendered,
        "expected goal handler signature with zero parameters"
    );
    assert_rendered!(
        rendered.contains("fn handle_goal_payload"),
        &rendered,
        "expected helper function for goal handler without request body"
    );
    assert_rendered!(
        rendered.contains("pub struct ResultResponse"),
        &rendered,
        "expected result response struct without request body"
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
        rendered.contains("pub async fn handle_result_next_request"),
        &rendered,
        "expected result handler even when result request body is missing"
    );
    assert_rendered!(
        rendered.contains("F: Fn() -> crate::Result<ResultResponse>"),
        &rendered,
        "expected result handler signature with zero parameters"
    );
    assert_rendered!(
        rendered.contains("fn handle_result_payload"),
        &rendered,
        "expected helper function for result handler without request body"
    );

    assert_rendered!(
        rendered.contains("pub async fn emit_feedback"),
        &rendered,
        "expected feedback emitter for action without request payloads"
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
        move_arm.contains("pub async fn handle_goal_next_request"),
        &move_arm,
        "expected goal handler for `move_arm`"
    );
    assert_rendered!(
        move_arm.contains("pub struct GoalRequest"),
        &move_arm,
        "expected goal request struct for `move_arm`"
    );
    assert_rendered!(
        move_arm.contains("pub struct ResultResponse"),
        &move_arm,
        "expected result response struct for `move_arm`"
    );
    assert_rendered!(
        move_arm.contains("pub async fn emit_feedback"),
        &move_arm,
        "expected feedback emitter for `move_arm`"
    );
    assert_rendered!(
        move_arm.contains("let service_name = \"move_arm/goal\";"),
        &move_arm,
        "expected move_arm goal service literal"
    );
    assert_rendered!(
        rotate_servo.contains("pub async fn handle_goal_next_request"),
        &rotate_servo,
        "expected goal handler for `rotate_servo_clockwise`"
    );
    assert_rendered!(
        rotate_servo.contains("F: Fn() -> crate::Result<GoalResponse>"),
        &rotate_servo,
        "expected zero-argument goal handler signature for `rotate_servo_clockwise`"
    );
    assert_rendered!(
        rotate_servo.contains("pub struct GoalResponse"),
        &rotate_servo,
        "expected goal response struct for `rotate_servo_clockwise`"
    );
    assert_rendered!(
        rotate_servo.contains("pub struct ResultResponse"),
        &rotate_servo,
        "expected result response struct for `rotate_servo_clockwise`"
    );
    assert_rendered!(
        rotate_servo.contains("pub async fn emit_feedback"),
        &rotate_servo,
        "expected feedback emitter for `rotate_servo_clockwise`"
    );
    assert_rendered!(
        rotate_servo.contains("let service_name = \"rotate_servo_clockwise/goal\";"),
        &rotate_servo,
        "expected rotate_servo_clockwise goal service literal"
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

    assert_rendered!(
        rendered.contains("pub struct GoalResponse"),
        &rendered,
        "expected goal response struct for subscribed action"
    );
    assert_rendered!(
        rendered.contains("pub accepted: bool"),
        &rendered,
        "expected accepted field in goal response"
    );
    assert_rendered!(
        rendered.contains("pub struct ResultResponse"),
        &rendered,
        "expected result response struct for subscribed action"
    );
    assert_rendered!(
        rendered.contains("final_position: [i32; 3]"),
        &rendered,
        "expected final_position array in result response"
    );
    assert_rendered!(
        rendered.contains("pub struct FeedbackMessage"),
        &rendered,
        "expected feedback message struct for subscribed action"
    );
    assert_rendered!(
        rendered.contains("new_position: [i32; 3]"),
        &rendered,
        "expected feedback message field in struct"
    );
    assert_rendered!(
        rendered.contains("pub async fn fire_goal("),
        &rendered,
        "expected fire_goal method definition"
    );
    assert_rendered!(
        rendered.contains("arm_id: u16"),
        &rendered,
        "expected goal parameter arm_id"
    );
    assert_rendered!(
        rendered.contains("desired_position: [i32; 3]"),
        &rendered,
        "expected goal parameter desired_position"
    );
    assert_rendered!(
        rendered.contains("-> crate::Result<GoalResponse>"),
        &rendered,
        "expected goal method to return goal response struct"
    );
    assert_rendered!(
        rendered.contains("let service_name = \"move_arm/goal\";"),
        &rendered,
        "expected goal service endpoint literal"
    );
    assert_rendered!(
        rendered.contains("pub async fn cancel_goal"),
        &rendered,
        "expected cancel_goal method definition"
    );
    assert_rendered!(
        rendered.contains("-> crate::Result<()>"),
        &rendered,
        "expected cancel goal to return unit result"
    );
    assert_rendered!(
        rendered.contains("pub async fn on_next_feedback_message"),
        &rendered,
        "expected feedback listener method definition"
    );
    assert_rendered!(
        rendered.contains("peppylib::TopicMessenger::subscribe"),
        &rendered,
        "expected topic subscription for feedback listener"
    );
    assert_rendered!(
        rendered.contains("fn deserialize_feedback_payload"),
        &rendered,
        "expected feedback payload helper function"
    );
    assert_rendered!(
        rendered.contains("pub async fn get_action_result"),
        &rendered,
        "expected result method definition"
    );
    assert_rendered!(
        rendered.contains("let service_name = \"move_arm/result\";"),
        &rendered,
        "expected result service endpoint literal"
    );
    assert_rendered!(
        rendered.contains("-> crate::Result<ResultResponse>"),
        &rendered,
        "expected result method to return result response struct"
    );
    assert_rendered!(
        rendered.contains("String::from(\"brain move_arm GoalResponse\")"),
        &rendered,
        "expected goal response context label to include node and action"
    );
    assert_rendered!(
        rendered.contains("String::from(\"brain move_arm ResultResponse\")"),
        &rendered,
        "expected result response context label to include node and action"
    );
    assert_rendered!(
        rendered.contains("String::from(\"brain move_arm FeedbackMessage\")"),
        &rendered,
        "expected feedback context label to include node and action"
    );
    assert_rendered!(
        rendered.contains("peppylib::ServiceMessenger::poll"),
        &rendered,
        "expected service poll usage in subscribed action methods"
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

    let mut generator = RustGenerator::new();
    generator
        .add_subscribed_action(&move_arm_action, Some(&move_arm_messages))
        .unwrap();
    generator
        .add_subscribed_action(&rotate_action, Some(&rotate_messages))
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

    let move_arm = artifact_map
        .get("move_arm")
        .expect("move_arm artifact is present");
    let rotate_servo = artifact_map
        .get("rotate_servo_clockwise")
        .expect("rotate_servo_clockwise artifact is present");

    // move_arm
    assert_rendered!(
        move_arm.contains("pub struct GoalResponse"),
        move_arm,
        "expected move_arm goal response struct"
    );
    assert_rendered!(
        move_arm.contains("pub struct ResultResponse"),
        move_arm,
        "expected move_arm result response struct"
    );
    assert_rendered!(
        move_arm.contains("pub struct FeedbackMessage"),
        move_arm,
        "expected move_arm feedback message struct"
    );
    assert_rendered!(
        move_arm.contains("arm_id: u16"),
        move_arm,
        "expected arm_id goal parameter"
    );
    assert_rendered!(
        move_arm.contains("desired_position: [i32; 3]"),
        move_arm,
        "expected desired_position goal parameter"
    );
    assert_rendered!(
        move_arm.contains("let service_name = \"move_arm/goal\";"),
        move_arm,
        "expected move_arm goal service literal"
    );
    assert_rendered!(
        move_arm.contains("let service_name = \"move_arm/result\";"),
        move_arm,
        "expected move_arm result service literal"
    );
    assert_rendered!(
        move_arm.contains("fn deserialize_feedback_payload"),
        move_arm,
        "expected move_arm feedback helper"
    );
    assert_rendered!(
        move_arm.contains("String::from(\"brain move_arm GoalResponse\")"),
        move_arm,
        "expected move_arm goal response context label"
    );
    assert_rendered!(
        move_arm.contains("String::from(\"brain move_arm ResultResponse\")"),
        move_arm,
        "expected move_arm result response context label"
    );
    assert_rendered!(
        move_arm.contains("String::from(\"brain move_arm FeedbackMessage\")"),
        move_arm,
        "expected move_arm feedback context label"
    );

    // rotate_servo_clockwise
    assert_rendered!(
        rotate_servo.contains("pub struct GoalResponse"),
        rotate_servo,
        "expected rotate_servo_clockwise goal response struct"
    );
    assert_rendered!(
        rotate_servo.contains("pub struct ResultResponse"),
        rotate_servo,
        "expected rotate_servo_clockwise result response struct"
    );
    assert_rendered!(
        rotate_servo.contains("pub struct FeedbackMessage"),
        rotate_servo,
        "expected rotate_servo_clockwise feedback message struct"
    );
    assert_rendered!(
        rotate_servo.contains("let service_name = \"rotate_servo_clockwise/goal\";"),
        rotate_servo,
        "expected rotate_servo_clockwise goal service literal"
    );
    assert_rendered!(
        rotate_servo.contains("let service_name = \"rotate_servo_clockwise/result\";"),
        rotate_servo,
        "expected rotate_servo_clockwise result service literal"
    );
    assert_rendered!(
        rotate_servo.contains("fn deserialize_feedback_payload"),
        rotate_servo,
        "expected rotate_servo_clockwise feedback helper"
    );
    assert_rendered!(
        rotate_servo.contains("-> crate::Result<GoalResponse>"),
        rotate_servo,
        "expected rotate_servo_clockwise goal helper return type"
    );
    assert_rendered!(
        rotate_servo.contains("peppylib::ServiceMessenger::poll"),
        rotate_servo,
        "expected rotate_servo_clockwise to poll service endpoints"
    );
    assert_rendered!(
        rotate_servo.contains("peppylib::TopicMessenger::subscribe"),
        rotate_servo,
        "expected rotate_servo_clockwise to subscribe for feedback"
    );
    assert_rendered!(
        rotate_servo.contains("String::from(\"brain rotate_servo_clockwise GoalResponse\")"),
        rotate_servo,
        "expected rotate_servo goal response context label"
    );
    assert_rendered!(
        rotate_servo.contains("String::from(\"brain rotate_servo_clockwise ResultResponse\")"),
        rotate_servo,
        "expected rotate_servo result response context label"
    );
    assert_rendered!(
        rotate_servo.contains("String::from(\"brain rotate_servo_clockwise FeedbackMessage\")"),
        rotate_servo,
        "expected rotate_servo feedback context label"
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

    let mut generator = RustGenerator::new();
    generator
        .add_subscribed_action(&action, Some(&format))
        .expect("generator should allow subscribed actions with empty response payloads");
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
        rendered.contains("pub async fn fire_goal"),
        &rendered,
        "expected fire_goal helper to be generated"
    );
    assert_rendered!(
        rendered.contains("pub async fn get_action_result"),
        &rendered,
        "expected get_action_result helper to be generated"
    );
    let ok_unit_count = rendered.matches("Ok(())").count();
    assert_rendered!(
        ok_unit_count >= 2,
        &rendered,
        "expected helpers with empty responses to return Ok(())"
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

    let mut generator = RustGenerator::new();
    generator
        .add_subscribed_action(&action, Some(&format))
        .expect("generator should allow subscribed actions without feedback payloads");
    let artifacts: Vec<_> = generator.into_artifacts();
    assert_eq!(artifacts.len(), 1, "expected single generated artifact");

    let rendered = artifacts[0].code_output.clone();
    assert_rendered!(
        rendered.contains("pub async fn fire_goal"),
        &rendered,
        "expected subscribed actions to still emit goal helpers"
    );
    assert_rendered!(
        rendered.contains("pub async fn get_action_result"),
        &rendered,
        "expected subscribed actions to still emit result helpers"
    );
    assert_rendered!(
        !rendered.contains("pub struct FeedbackMessage"),
        &rendered,
        "expected absence of feedback struct when feedback payload is missing"
    );
    assert_rendered!(
        !rendered.contains("pub async fn on_next_feedback_message"),
        &rendered,
        "expected generator to skip feedback listener when no feedback payload is provided"
    );
    assert_rendered!(
        !rendered.contains("fn deserialize_feedback_payload"),
        &rendered,
        "expected feedback payload helper to be omitted without feedback format"
    );
    assert_rendered!(
        !rendered.contains("peppylib::TopicMessenger::subscribe"),
        &rendered,
        "expected generator to avoid topic subscriptions without feedback payloads"
    );
}

/// This is a long running test
#[test]
fn compile_lib_with_exposed_and_subscribed_actions() {
    let temp_dir = TempDir::new().unwrap();
    let action1: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE).unwrap();
    let action2: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE2).unwrap();

    let subscribed_action1: SubscribedAction =
        serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE1).unwrap();
    let subscribed_action1_goal_request: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_FORMAT1).unwrap();
    let subscribed_action1_goal_response: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT1).unwrap();
    let subscribed_action1_feedback: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_FEEDBACK_FORMAT1).unwrap();
    let subscribed_action1_result_response: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_RESULT_RESPONSE_FORMAT1).unwrap();
    let subscribed_action1_messages = SubscribedActionMessage {
        goal_request: Some(subscribed_action1_goal_request),
        goal_response: Some(subscribed_action1_goal_response),
        feedback: Some(subscribed_action1_feedback),
        result_request: None,
        result_response: Some(subscribed_action1_result_response),
    };

    let subscribed_action2: SubscribedAction =
        serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE2).unwrap();
    let subscribed_action2_goal_response: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_RESPONSE_FORMAT2).unwrap();
    let subscribed_action2_feedback: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_FEEDBACK_FORMAT2).unwrap();
    let subscribed_action2_result_response: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_ACTION_RESULT_RESPONSE_FORMAT2).unwrap();
    let subscribed_action2_messages = SubscribedActionMessage {
        goal_request: None,
        goal_response: Some(subscribed_action2_goal_response),
        feedback: Some(subscribed_action2_feedback),
        result_request: None,
        result_response: Some(subscribed_action2_result_response),
    };

    let (mut generator, output_dir, user_node) = init_test_env(&temp_dir);
    generator.add_exposed_action(&action1).unwrap();
    generator.add_exposed_action(&action2).unwrap();
    generator
        .add_subscribed_action(&subscribed_action1, Some(&subscribed_action1_messages))
        .unwrap();
    generator
        .add_subscribed_action(&subscribed_action2, Some(&subscribed_action2_messages))
        .unwrap();
    let output_config = copy_config_to_output(&user_node, &output_dir);
    generator.build(&output_dir).unwrap();
    fs::remove_file(output_config).unwrap();

    let cargo_output = Command::new("cargo")
        .arg("build")
        .env("CARGO_NET_OFFLINE", "true")
        .current_dir(&output_dir)
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
    assert!(
        lib_contents.contains("pub mod exposed_actions;"),
        "Generated lib.rs should re-export the exposed_actions module, got:\n{}",
        lib_contents
    );
    assert!(
        lib_contents.contains("pub mod subscribed_actions;"),
        "Generated lib.rs should re-export the subscribed_actions module, got:\n{}",
        lib_contents
    );

    let exposed_actions_mod = output_dir.join("src/exposed_actions.rs");
    assert!(
        exposed_actions_mod.exists(),
        "Expected exposed_actions module file so `peppygen::exposed_actions::<action>` resolves"
    );
    let exposed_actions_contents =
        fs::read_to_string(&exposed_actions_mod).expect("failed to read exposed_actions module");
    assert!(
        exposed_actions_contents.contains("pub mod move_arm;"),
        "Expected exposed_actions module to declare the move_arm action, got:\n{}",
        exposed_actions_contents
    );
    assert!(
        exposed_actions_contents.contains("pub mod rotate_servo_clockwise;"),
        "Expected exposed_actions module to declare the rotate_servo_clockwise action, got:\n{}",
        exposed_actions_contents
    );

    let subscribed_actions_mod = output_dir.join("src/subscribed_actions.rs");
    assert!(
        subscribed_actions_mod.exists(),
        "Expected subscribed_actions module file so `peppygen::subscribed_actions::<action>` resolves"
    );
    let subscribed_actions_contents = fs::read_to_string(&subscribed_actions_mod)
        .expect("failed to read subscribed_actions module");
    assert!(
        subscribed_actions_contents.contains("pub mod brain_move_arm;"),
        "Expected subscribed_actions module to declare the brain_move_arm client, got:\n{}",
        subscribed_actions_contents
    );
    assert!(
        subscribed_actions_contents.contains("pub mod controller_rotate_servo_clockwise;"),
        "Expected subscribed_actions module to declare the controller_rotate_servo_clockwise client, got:\n{}",
        subscribed_actions_contents
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
    let subscribed_brain_path = output_dir.join("src/subscribed_actions/brain_move_arm.rs");
    assert!(
        subscribed_brain_path.exists(),
        "Expected brain_move_arm subscribed action module at {:?}",
        subscribed_brain_path
    );
    let subscribed_controller_path =
        output_dir.join("src/subscribed_actions/controller_rotate_servo_clockwise.rs");
    assert!(
        subscribed_controller_path.exists(),
        "Expected controller_rotate_servo_clockwise subscribed action module at {:?}",
        subscribed_controller_path
    );
}
