use super::*;
use config::node::{ExposedAction, ExposedService, ExposedTopic};
use std::{fs, process::Command};
use tempfile::TempDir;

const EXPOSED_TOPIC_EXAMPLE: &str = r#"
{
  name: "push_frame",
  qos_profile: "sensor_data",
  message_format: {
    header: {
    type: "object",
    stamp: "time",
    frame_id: "u32"
  },
  encoding: "string",
    width: "u32",
    height: "u32",
    image: {
      type: "array",
      items: "u8",
      length: 3
    }
  }
}
"#;

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

const SUBSCRIBED_TOPIC_EXAMPLE: &str = r#"
{
  node: "uvc_camera",
  name: "stream",
  tag: "0.1.0"
}
"#;

const SUBSCRIBED_TOPIC_FORMAT_EXAMPLE: &str = r#"
{
  header: {
    type: "object",
    stamp: "time",
    frame_id: "u32"
  },
  encoding: "string",
  width: "u32",
  height: "u32",
  image: {
    type: "array",
    items: "u8",
    length: 3
  }
}
"#;

const SUBSCRIBED_SERVICE_EXAMPLE: &str = r#"
{
  node: "uvc_camera",
  name: "get_camera_info",
  tag: "0.1.0"
}
"#;

const SUBSCRIBED_ACTION_EXAMPLE: &str = r#"
{
  node: "brain",
  name: "move_arm",
  tag: "0.1.0",
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
  final_position: {
    type: "array",
    items: "i32",
    length: 3
  }
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

const SUBSCRIBED_SERVICE_FORMAT_EXAMPLE: &str = r#"
{
  card_type: "string",
  size: "string",
  interval: "string"
}
"#;

const EXPOSED_SERVICE_EXAMPLE: &str = r#"
{
  name: "enable_camera",
  qos_profile: "critical",
  accept_message_format: {
    enable: "bool"
  },
  return_message_format: {
    enabled: "bool",
    error_msg: "string"
  }
}
"#;

#[test]
fn create_lib_basic_structure() {
    let temp_dir = TempDir::new().unwrap();
    let (generator, output_dir, user_node) = init_test_env(&temp_dir);
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
    assert!(
        user_node.join(PEPPY_NODE_CONFIG_FILE).exists(),
        "Expected original user project to retain the node configuration file"
    );
    assert!(
        !output_dir.join(PEPPY_NODE_CONFIG_FILE).exists(),
        "Generated crate should not keep a copy of the node configuration file"
    );
    let temp_dir_path = temp_dir.path();
    let mut entries = fs::read_dir(&temp_dir_path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    entries.sort();
    assert_eq!(entries, vec![".peppy", "user_node"]);

    let mut hidden_entries = fs::read_dir(temp_dir_path.join(".peppy"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    hidden_entries.sort();
    assert_eq!(hidden_entries, vec!["libs"]);

    let mut libs_entries = fs::read_dir(temp_dir_path.join(".peppy/libs"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    libs_entries.sort();
    assert_eq!(libs_entries, vec!["peppygen"]);

    let output = Command::new("cargo")
        .arg("build")
        .env("CARGO_NET_OFFLINE", "true")
        .current_dir(&output_dir)
        .output()
        .expect("failed to invoke cargo build on generated crate");
    let status = output.status;

    assert!(
        status.success(),
        "cargo build failed for generated crate with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Generates the peppygen lib and runs the tests inside of it, including clippy
#[test]
fn generate_lib_and_run_tests() {
    let temp_dir = TempDir::new().unwrap();
    let service: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE).unwrap();
    let topic: ExposedTopic = serde_json5::from_str(EXPOSED_TOPIC_EXAMPLE).unwrap();
    let action: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE).unwrap();

    let (mut generator, output_dir, user_node) = init_test_env(&temp_dir);
    generator.add_exposed_service(&service).unwrap();
    generator.add_exposed_topic(&topic).unwrap();
    generator.add_exposed_action(&action).unwrap();
    let output_config = copy_config_to_output(&user_node, &output_dir);
    generator.build(&output_dir).unwrap();
    fs::remove_file(output_config).unwrap();

    let clippy_output = Command::new("cargo")
        .arg("clippy")
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

    let test_output = Command::new("cargo")
        .arg("test")
        .arg("--color")
        .arg("always")
        .env("CARGO_NET_OFFLINE", "true")
        .current_dir(&output_dir)
        .output()
        .expect("failed to run cargo test on generated crate");
    assert!(
        test_output.status.success(),
        "cargo test failed for generated crate with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        test_output.status.code(),
        String::from_utf8_lossy(&test_output.stdout),
        String::from_utf8_lossy(&test_output.stderr)
    );
}

#[test]
fn create_lib_with_exposed_and_subscribed_topic_service_and_action_artifacts() {
    let temp_dir = TempDir::new().unwrap();
    let topic: ExposedTopic = serde_json5::from_str(EXPOSED_TOPIC_EXAMPLE).unwrap();
    let service: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE).unwrap();
    let action: ExposedAction = serde_json5::from_str(EXPOSED_ACTION_EXAMPLE).unwrap();
    let subscribed_topic: SubscribedTopic =
        serde_json5::from_str(SUBSCRIBED_TOPIC_EXAMPLE).unwrap();
    let subscribed_topic_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE).unwrap();
    let subscribed_service: SubscribedService =
        serde_json5::from_str(SUBSCRIBED_SERVICE_EXAMPLE).unwrap();
    let subscribed_service_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_SERVICE_FORMAT_EXAMPLE).unwrap();
    let subscribed_action: SubscribedAction =
        serde_json5::from_str(SUBSCRIBED_ACTION_EXAMPLE).unwrap();
    let subscribed_action_messages = SubscribedActionMessage {
        goal: serde_json5::from_str(SUBSCRIBED_ACTION_GOAL_FORMAT).unwrap(),
        feedback: serde_json5::from_str(SUBSCRIBED_ACTION_FEEDBACK_FORMAT).unwrap(),
        result: serde_json5::from_str(SUBSCRIBED_ACTION_RESULT_FORMAT).unwrap(),
    };

    let (mut generator, output_dir, user_node) = init_test_env(&temp_dir);
    generator.add_exposed_topic(&topic).unwrap();
    generator.add_exposed_service(&service).unwrap();
    generator.add_exposed_action(&action).unwrap();
    generator
        .add_subscribed_topic(&subscribed_topic, Some(&subscribed_topic_format))
        .unwrap();
    generator
        .add_subscribed_service(&subscribed_service, Some(&subscribed_service_format))
        .unwrap();
    generator
        .add_subscribed_action(&subscribed_action, Some(&subscribed_action_messages))
        .unwrap();
    let output_config = copy_config_to_output(&user_node, &output_dir);
    generator.build(&output_dir).unwrap();
    fs::remove_file(output_config).unwrap();

    let lib_rs = output_dir.join("src/lib.rs");
    assert!(
        lib_rs.exists(),
        "Expected lib.rs to exist so the generated crate exposes its modules"
    );
    let lib_contents = std::fs::read_to_string(&lib_rs).expect("failed to read generated lib.rs");
    assert!(
        output_dir.join("Cargo.toml").exists(),
        "Expected Cargo.toml to be generated in the temporary crate directory"
    );
    assert!(
        user_node.join(PEPPY_NODE_CONFIG_FILE).exists(),
        "Expected original user project to retain the node configuration file"
    );
    assert!(
        !output_dir.join(PEPPY_NODE_CONFIG_FILE).exists(),
        "Generated crate should not keep a copy of the node configuration file"
    );

    assert!(
        lib_contents.contains("pub mod topics;"),
        "Expected lib.rs to re-export `topics` module, got:\n{}",
        lib_contents
    );
    assert!(
        lib_contents.contains("pub mod services;"),
        "Expected lib.rs to re-export `services` module, got:\n{}",
        lib_contents
    );
    assert!(
        lib_contents.contains("pub mod actions;"),
        "Expected lib.rs to re-export `actions` module, got:\n{}",
        lib_contents
    );

    let topics_mod = output_dir.join("src/topics.rs");
    let services_mod = output_dir.join("src/services.rs");
    let actions_mod = output_dir.join("src/actions.rs");
    assert!(topics_mod.exists(), "Expected topics module file to exist");
    assert!(
        services_mod.exists(),
        "Expected services module file to exist"
    );
    assert!(
        actions_mod.exists(),
        "Expected actions module file to exist"
    );

    let topics_contents =
        std::fs::read_to_string(&topics_mod).expect("failed to read topics module");
    assert!(
        topics_contents.contains("pub mod push_frame;"),
        "Expected topics module to expose generated `push_frame` module, got:\n{}",
        topics_contents
    );
    assert!(
        topics_contents.contains("pub mod stream;"),
        "Expected topics module to expose subscribed `stream` module, got:\n{}",
        topics_contents
    );

    let services_contents =
        std::fs::read_to_string(&services_mod).expect("failed to read services module");
    assert!(
        services_contents.contains("pub mod enable_camera;"),
        "Expected services module to expose generated `enable_camera` module, got:\n{}",
        services_contents
    );
    assert!(
        services_contents.contains("pub mod get_camera_info;"),
        "Expected services module to expose subscribed `get_camera_info` module, got:\n{}",
        services_contents
    );

    let actions_contents =
        std::fs::read_to_string(&actions_mod).expect("failed to read actions module");
    assert!(
        actions_contents.contains("pub mod move_arm;"),
        "Expected actions module to expose generated `move_arm` module, got:\n{}",
        actions_contents
    );

    let push_frame_module = output_dir.join("src/topics/push_frame.rs");
    let stream_module = output_dir.join("src/topics/stream.rs");
    let enable_module = output_dir.join("src/services/enable_camera.rs");
    let get_camera_info_module = output_dir.join("src/services/get_camera_info.rs");
    let move_arm_module = output_dir.join("src/actions/move_arm.rs");
    assert!(
        push_frame_module.exists(),
        "Expected generated topic module to exist"
    );
    assert!(
        enable_module.exists(),
        "Expected generated service module to exist"
    );
    assert!(
        stream_module.exists(),
        "Expected generated subscribed topic module to exist"
    );
    assert!(
        get_camera_info_module.exists(),
        "Expected generated subscribed service module to exist"
    );
    assert!(
        move_arm_module.exists(),
        "Expected generated action module to exist"
    );

    let push_frame_contents =
        std::fs::read_to_string(&push_frame_module).expect("failed to read push_frame module");
    assert!(
        push_frame_contents.contains("pub async fn push_frame_async("),
        "Expected combined generation to produce async topic function, got:\n{}",
        push_frame_contents
    );
    let stream_contents =
        std::fs::read_to_string(&stream_module).expect("failed to read stream module");
    assert!(
        stream_contents.contains(
            "pub async fn on_next_stream_message(&mut self) -> peppylib::PeppyResult<UvcCameraStreamMessage>"
        ),
        "Expected subscribed topic module to expose callback arguments, got:\n{}",
        stream_contents
    );

    let enable_contents =
        std::fs::read_to_string(&enable_module).expect("failed to read enable_camera module");
    assert!(
        enable_contents.contains("pub fn enable_camera("),
        "Expected combined generation to produce service function, got:\n{}",
        enable_contents
    );
    let get_camera_info_contents = std::fs::read_to_string(&get_camera_info_module)
        .expect("failed to read get_camera_info module");
    assert!(
        get_camera_info_contents
            .contains("pub async fn on_get_camera_info() -> OnGetCameraInfoArguments"),
        "Expected subscribed service module to expose callback, got:\n{}",
        get_camera_info_contents
    );

    let move_arm_contents =
        std::fs::read_to_string(&move_arm_module).expect("failed to read move_arm module");
    assert!(
        move_arm_contents.contains("pub async fn move_arm_result_async("),
        "Expected combined generation to produce action result async function, got:\n{}",
        move_arm_contents
    );
    assert!(
        move_arm_contents
            .contains("pub async fn on_move_arm_feedback() -> OnMoveArmFeedbackArguments"),
        "Expected subscribed action module to expose feedback callback, got:\n{}",
        move_arm_contents
    );
    assert!(
        move_arm_contents.contains("pub async fn on_move_arm_result() -> OnMoveArmResultArguments"),
        "Expected subscribed action module to expose result callback, got:\n{}",
        move_arm_contents
    );

    assert!(
        !output_dir.join(PEPPY_NODE_CONFIG_FILE).exists(),
        "Generated crate should not keep a copy of the node configuration file"
    );
}
