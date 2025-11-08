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
      success: "bool"
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

    todo!("Finish")
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
