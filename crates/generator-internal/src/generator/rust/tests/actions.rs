use super::*;

use config::node::{ExposedAction, SubscribedAction};

const EXPOSED_ACTION_EXAMPLE: &str = r#"
{
  name: "move_arm",
  goal_service: {
    accept_message_format: {
      arm_id: "u16",
      desired_position: {
        type: "array",
        items: "i32",
        length: 3
      }
    },
    return_message_format: {
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
    accept_message_format: {
      final_position: {
        type: "array",
        items: "i32",
        length: 3
      }
    },
    return_message_format: {
      success: "bool"
    }
  }
}
"#;

const SUBSCRIBED_ACTION_EXAMPLE: &str = r#"
{
  node: "brain",
  name: "move_arm",
  tag: "0.1.0",
  feedback_callback: "on_move_arm_feedback",
  results_callback: "on_move_arm_result"
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
fn exposed_action_gen_calling_code() {
    let action: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE).unwrap();

    let mut generator = RustGenerator::new();
    generator.add_exposed_action(&action).unwrap();
    let artifacts: Vec<String> = generator
        .into_artifacts()
        .into_iter()
        .map(|artifact| artifact.code_output)
        .collect();
    let rendered = single_artifact(artifacts);

    println!("generated action code:\n{rendered}");

    for expected in [
        "pub fn move_arm_goal(",
        "pub async fn move_arm_goal_async(",
        "pub fn move_arm_feedback(",
        "pub async fn move_arm_feedback_async(",
        "pub fn move_arm_result(",
        "pub async fn move_arm_result_async(",
    ] {
        assert_rendered!(
            rendered.contains(expected),
            &rendered,
            "expected `{expected}` in rendered"
        );
    }

    for expected in [
        "-> ::capnp::Result<Vec<u8>>",
        "::capnp::message::Builder::new_default",
        "::capnp::serialize::write_message",
    ] {
        assert_rendered!(
            rendered.contains(expected),
            &rendered,
            "expected `{expected}` in capnp-based action code"
        );
    }

    assert_rendered!(
        rendered.contains("move_arm_goal_message_capnp::move_arm_goal_message::Builder"),
        &rendered,
        "expected goal schema builder"
    );
    assert_rendered!(
        rendered.contains("init_desired_position"),
        &rendered,
        "expected list initialization for desired position"
    );
    assert_rendered!(
        rendered.contains("init_new_position"),
        &rendered,
        "expected list initialization for feedback"
    );
    assert_rendered!(
        rendered.contains("init_final_position"),
        &rendered,
        "expected list initialization for result"
    );

    assert_rendered!(
        rendered.contains("arm_id: u16"),
        &rendered,
        "expected goal argument"
    );
    assert_rendered!(
        rendered.contains("desired_position: [i32; 3]"),
        &rendered,
        "expected goal array argument"
    );
    assert_rendered!(
        rendered.contains("new_position: [i32; 3]"),
        &rendered,
        "expected feedback array argument"
    );
    assert_rendered!(
        rendered.contains("final_position: [i32; 3]"),
        &rendered,
        "expected result array argument"
    );
}

#[test]
fn subscribed_action_returns_arguments() {
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
    let rendered = single_artifact(artifacts);

    println!("generated subscribed action code:\n{rendered}");

    assert!(
        !rendered.contains("pub async fn on_move_arm_goal()"),
        "unexpected goal callback generated:\n{rendered}"
    );

    for expected in [
        "pub async fn on_move_arm_feedback() -> OnMoveArmFeedbackArguments",
        "pub async fn on_move_arm_result() -> OnMoveArmResultArguments",
    ] {
        assert_rendered!(
            rendered.contains(expected),
            &rendered,
            "expected `{expected}` in rendered"
        );
    }

    assert_rendered!(
        rendered.contains("payload: Vec<u8>"),
        &rendered,
        "expected feedback payload"
    );
    assert_rendered!(
        rendered.contains("final_position: [i32; 3]"),
        &rendered,
        "expected result array"
    );
    assert!(
        !rendered.contains("arm_id: u16"),
        "goal fields should not appear in feedback or result structs:\n{rendered}"
    );
}

#[test]
fn create_lib_with_exposed_action_artifact() {
    let temp_dir = TempDir::new().unwrap();
    let action: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE).unwrap();

    let (mut generator, output_dir, user_node) = init_test_env(&temp_dir);
    generator.add_exposed_action(&action).unwrap();
    let output_config = copy_config_to_output(&user_node, &output_dir);
    generator.build(&output_dir).unwrap();
    fs::remove_file(output_config).unwrap();

    assert!(
        output_dir.join("Cargo.toml").exists(),
        "Expected Cargo.toml to be generated in the temporary crate directory"
    );
    assert!(
        !output_dir.join(PEPPY_NODE_CONFIG_FILE).exists(),
        "Generated crate should not keep a copy of the node configuration file"
    );

    let lib_rs = output_dir.join("src/lib.rs");
    assert!(
        lib_rs.exists(),
        "Expected lib.rs to exist so `peppygen::actions` is reachable"
    );
    let lib_contents = std::fs::read_to_string(&lib_rs).expect("failed to read generated lib.rs");
    assert!(
        lib_contents.contains("pub mod actions;"),
        "Expected generated lib.rs to re-export the `actions` module, got:\n{}",
        lib_contents
    );

    let actions_mod = output_dir.join("src/actions.rs");
    assert!(
        actions_mod.exists(),
        "Expected actions module file to exist so `peppygen::actions::<module>` resolves"
    );
    let actions_contents =
        std::fs::read_to_string(&actions_mod).expect("failed to read actions module");
    assert!(
        actions_contents.contains("pub mod move_arm;"),
        "Expected actions module to expose generated `move_arm` module, got:\n{}",
        actions_contents
    );

    let move_arm_module = output_dir.join("src/actions/move_arm.rs");
    assert!(
        move_arm_module.exists(),
        "Expected generated action module at {:?}",
        move_arm_module
    );
    let move_arm_contents =
        std::fs::read_to_string(&move_arm_module).expect("failed to read move_arm module");
    for expected in [
        "pub fn move_arm_goal(",
        "pub async fn move_arm_goal_async(",
        "pub fn move_arm_feedback(",
        "pub async fn move_arm_feedback_async(",
        "pub fn move_arm_result(",
        "pub async fn move_arm_result_async(",
    ] {
        assert!(
            move_arm_contents.contains(expected),
            "Expected generated action module to expose `{expected}`, got:\n{}",
            move_arm_contents
        );
    }
}
