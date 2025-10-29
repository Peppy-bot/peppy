macro_rules! assert_rendered {
    ($cond:expr, $rendered:expr, $($arg:tt)+) => {
        if !$cond {
            eprintln!("rendered output:\n{}", $rendered);
            panic!($($arg)+);
        }
    };
}

mod actions;
mod libgen;
mod services;
mod topics;

use super::*;
use std::{fs, path::Path};
use tempfile::TempDir;

const STUB_NODE_CONFIG: &str = r#"{
  schema_version: 1,
  manifest: {
    name: "generated_node",
    tag: "0.1.0",
    launch_cmd: ["cargo", "run", "--release"]
  },
  logging: {
    min_level: "info",
    format: "text"
  }
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
  tag: "0.1.0",
  callback: "on_video_frame_received"
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
  tag: "0.1.0",
  callback: "on_get_camera_info"
}
"#;

const SUBSCRIBED_SERVICE_FORMAT_EXAMPLE: &str = r#"
{
  card_type: "string",
  size: "string",
  interval: "string"
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

fn prepare_directories(temp_dir: &TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
    let output_dir = temp_dir.path().join(".peppy/libs/peppygen");
    let user_node = temp_dir.path().join("user_node");
    fs::create_dir_all(&output_dir).unwrap();
    fs::create_dir_all(&user_node).unwrap();
    fs::write(user_node.join(PEPPY_NODE_CONFIG_FILE), STUB_NODE_CONFIG).unwrap();
    (output_dir, user_node)
}

fn init_test_env(temp_dir: &TempDir) -> (RustGenerator, std::path::PathBuf, std::path::PathBuf) {
    let (output_dir, user_node) = prepare_directories(temp_dir);
    (RustGenerator::new(), output_dir, user_node)
}

fn copy_config_to_output(user_node: &Path, output_dir: &Path) -> std::path::PathBuf {
    let source = user_node.join(PEPPY_NODE_CONFIG_FILE);
    let destination = output_dir.join(PEPPY_NODE_CONFIG_FILE);
    fs::copy(&source, &destination).unwrap();
    destination
}

fn single_artifact(artifacts: Vec<String>) -> String {
    assert_eq!(
        artifacts.len(),
        1,
        "expected a single generated artifact, got {}",
        artifacts.len()
    );
    artifacts.into_iter().next().expect("artifact is present")
}
