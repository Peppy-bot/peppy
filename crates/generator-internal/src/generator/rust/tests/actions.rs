use super::*;

use config::node::{ExposedAction, SubscribedAction};

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
    request_message_format: {
      final_position: {
        type: "array",
        items: "i32",
        length: 3
      }
    },
    response_message_format: {
      success: "bool",
      error_msg: "string"
    }
  }
}
"#;

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
        rendered.contains("pub struct MoveArmResultRequest"),
        &rendered,
        "expected result request struct"
    );
    assert_rendered!(
        rendered.contains("final_position: [i32; 3]"),
        &rendered,
        "expected final_position field in result request"
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
        rendered.contains("F: Fn(MoveArmResultRequest) -> crate::Result<MoveArmResultResponse>"),
        &rendered,
        "expected result handler signature"
    );
}

#[test]
fn expose_action_without_request_body() {
    todo!("Finish")
}

#[test]
fn expose_two_actions() {
    todo!("Finish")
}

#[test]
fn subscribed_to_action() {
    let action: SubscribedAction = serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE).unwrap();
    let goal_format: MessageFormat = serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_FORMAT).unwrap();
    let feedback_format: MessageFormat = serde_json5::from_str(r#"{ payload: "bytes" }"#).unwrap();
    let result_format: MessageFormat = serde_json5::from_str(
        r#"{
            final_position: {
                type: "array",
                items: "i32",
                length: 3
            }
        }"#,
    )
    .unwrap();
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
