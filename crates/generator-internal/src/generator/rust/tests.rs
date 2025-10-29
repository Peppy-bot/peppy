use super::*;
use config::node::{
    ExposedAction, ExposedService, ExposedTopic, SubscribedAction, SubscribedService,
    SubscribedTopic,
};
use std::{fs, path::Path, process::Command};
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

macro_rules! assert_rendered {
        ($cond:expr, $rendered:expr, $($arg:tt)+) => {
            if !$cond {
                eprintln!("rendered output:\n{}", $rendered);
                panic!($($arg)+);
            }
        };
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
fn exposed_topic_gen_calling_code() {
    let topic: ExposedTopic = serde_json5::from_str(EXPOSED_TOPIC_EXAMPLE).unwrap();

    let mut generator = RustGenerator::new();
    generator.add_exposed_topic(&topic).unwrap();
    let artifacts: Vec<String> = generator
        .into_artifacts()
        .into_iter()
        .map(|artifact| artifact.code_output)
        .collect();
    let rendered = single_artifact(artifacts);

    assert_rendered!(
        rendered.contains("pub fn push_frame("),
        &rendered,
        "expected sync function"
    );
    assert_rendered!(
        rendered.contains("-> capnp::Result<Vec<u8>>"),
        &rendered,
        "expected capnp result type for sync function"
    );
    assert_rendered!(
        rendered.contains("pub async fn push_frame_async("),
        &rendered,
        "expected async function"
    );
    assert_rendered!(
        rendered.contains("let mut message = capnp::message::Builder::new_default();"),
        &rendered,
        "expected capnp message builder"
    );
    assert_rendered!(
        rendered.contains("crate::capnp::push_frame_message_capnp::push_frame_message::Builder"),
        &rendered,
        "expected capnp root initialization"
    );
    assert_rendered!(
        rendered.contains("root.set_encoding(encoding.as_str());"),
        &rendered,
        "expected encoding setter"
    );
    assert_rendered!(
        rendered.contains("root.set_image(image.as_ref());"),
        &rendered,
        "expected fixed-size array setter"
    );
    assert_rendered!(
        rendered.contains("root.reborrow().init_header();"),
        &rendered,
        "expected nested header initialization"
    );
    assert_rendered!(
        rendered.contains("config::convert_time"),
        &rendered,
        "expected timestamp conversion helper"
    );
    assert_rendered!(
        rendered.contains("capnp::serialize::write_message"),
        &rendered,
        "expected serialization call"
    );
    assert_rendered!(
        rendered.contains("header: PushFrameHeader"),
        &rendered,
        "expected structured header argument"
    );
    assert_rendered!(
        rendered.contains("image: [u8; 3]"),
        &rendered,
        "expected fixed-size array argument"
    );
    assert_rendered!(
        rendered.contains("pub struct PushFrameHeader"),
        &rendered,
        "expected generated struct for nested object"
    );
}

#[test]
fn exposed_service_gen_calling_code() {
    let service: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE).unwrap();

    let mut generator = RustGenerator::new();
    generator.add_exposed_service(&service).unwrap();
    let artifacts: Vec<String> = generator
        .into_artifacts()
        .into_iter()
        .map(|artifact| artifact.code_output)
        .collect();
    let rendered = single_artifact(artifacts);

    println!("generated service code:\n{rendered}");

    assert_rendered!(
        rendered.contains("pub fn enable_camera("),
        &rendered,
        "expected sync service function"
    );
    assert_rendered!(
        rendered.contains("-> ::capnp::Result<Vec<u8>>"),
        &rendered,
        "expected capnp result type for service function"
    );
    assert_rendered!(
        rendered.contains("pub async fn enable_camera_async("),
        &rendered,
        "expected async service function"
    );
    assert_rendered!(
        rendered.contains("::capnp::message::Builder::new_default"),
        &rendered,
        "expected capnp builder usage"
    );
    assert_rendered!(
        rendered.contains("enable_camera_message_capnp::enable_camera_message::Builder"),
        &rendered,
        "expected service-specific schema builder"
    );
    assert_rendered!(
        rendered.contains("::capnp::serialize::write_message"),
        &rendered,
        "expected serialization call"
    );
    assert_rendered!(
        rendered.contains("enable: bool"),
        &rendered,
        "expected bool argument"
    );
}

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
fn subscribed_topic_returns_arguments() {
    let topic = r#"
        {
            node: "uvc_camera",
            name: "stream",
            tag: "0.1.0",
            callback: "on_video_frame_received"
        }
        "#;
    let topic: SubscribedTopic = serde_json5::from_str(topic).unwrap();
    let format = r#"
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
    let format: MessageFormat = serde_json5::from_str(format).unwrap();

    let mut generator = RustGenerator::new();
    generator
        .add_subscribed_topic(&topic, Some(&format))
        .unwrap();
    let artifacts: Vec<String> = generator
        .into_artifacts()
        .into_iter()
        .map(|artifact| artifact.code_output)
        .collect();
    let rendered = single_artifact(artifacts);

    println!("generated subscribed topic code:\n{rendered}");

    assert_rendered!(
        rendered
            .contains("pub async fn on_video_frame_received() -> OnVideoFrameReceivedArguments"),
        &rendered,
        "expected async subscriber function with return type"
    );
    assert_rendered!(
        rendered.contains("pub struct OnVideoFrameReceivedArguments"),
        &rendered,
        "expected return struct definition"
    );
    assert_rendered!(
        rendered.contains("pub struct OnVideoFrameReceivedHeader"),
        &rendered,
        "expected nested struct definition"
    );
    assert_rendered!(
        rendered.contains("image: [u8; 3]"),
        &rendered,
        "expected array element type"
    );
}

#[test]
fn subscribed_service_returns_arguments() {
    let service = r#"
        {
            node: "uvc_camera",
            name: "get_camera_info",
            tag: "0.1.0",
            callback: "on_get_camera_info"
        }
        "#;
    let service: SubscribedService = serde_json5::from_str(service).unwrap();
    let format = r#"
        {
            card_type: "string",
            size: "string",
            interval: "string"
        }
        "#;
    let format: MessageFormat = serde_json5::from_str(format).unwrap();

    let mut generator = RustGenerator::new();
    generator
        .add_subscribed_service(&service, Some(&format))
        .unwrap();
    let artifacts: Vec<String> = generator
        .into_artifacts()
        .into_iter()
        .map(|artifact| artifact.code_output)
        .collect();
    let rendered = single_artifact(artifacts);

    println!("generated subscribed service code:\n{rendered}");

    assert_rendered!(
        rendered.contains("pub async fn on_get_camera_info() -> OnGetCameraInfoArguments"),
        &rendered,
        "expected async service subscriber"
    );
    assert_rendered!(
        rendered.contains("pub struct OnGetCameraInfoArguments"),
        &rendered,
        "expected return struct"
    );
    assert_rendered!(
        rendered.contains("card_type: String"),
        &rendered,
        "expected field mapping"
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
fn create_lib_with_exposed_topic_artifact() {
    let temp_dir = TempDir::new().unwrap();
    let topic: ExposedTopic = serde_json5::from_str(EXPOSED_TOPIC_EXAMPLE).unwrap();

    let (mut generator, output_dir, user_node) = init_test_env(&temp_dir);
    generator.add_exposed_topic(&topic).unwrap();
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
        "Expected lib.rs to exist so `peppygen::topics` is reachable"
    );
    let lib_contents = std::fs::read_to_string(&lib_rs).expect("failed to read generated lib.rs");
    assert!(
        lib_contents.contains("pub mod topics;"),
        "Expected generated lib.rs to re-export the `topics` module, got:\n{}",
        lib_contents
    );

    let topics_mod = output_dir.join("src/topics.rs");
    assert!(
        topics_mod.exists(),
        "Expected topics module file to exist so `peppygen::topics::<module>` resolves"
    );
    let topics_contents =
        std::fs::read_to_string(&topics_mod).expect("failed to read topics module");
    assert!(
        topics_contents.contains("pub mod push_frame;"),
        "Expected topics module to expose generated `push_frame` module, got:\n{}",
        topics_contents
    );

    let push_frame_mod = output_dir.join("src/topics/push_frame.rs");
    assert!(
        push_frame_mod.exists(),
        "Expected generated topic module at {:?}",
        push_frame_mod
    );
    let push_frame_contents =
        std::fs::read_to_string(&push_frame_mod).expect("failed to read push_frame module");
    assert!(
        push_frame_contents.contains("pub fn push_frame("),
        "Expected generated module to expose sync topic function, got:\n{}",
        push_frame_contents
    );
    assert!(
        push_frame_contents.contains("pub async fn push_frame_async("),
        "Expected generated module to expose async topic function, got:\n{}",
        push_frame_contents
    );
}

#[test]
fn create_lib_with_exposed_double_topic_artifact() {
    let temp_dir = TempDir::new().unwrap();
    let topic2 = r#"
        {
            name: "push_lidar_object", // The name of the topic inside the `lidar_sensor` node
            qos_profile: "sensor_data",
            message_format: {
                header: {
                  type: "object",
                  stamp: "time",
                  frame_id: "u32",
                },
                x: "f32",
                y: "f32",
                z: "f32",
                intensity: "f32",
                return_type: "u8", // e.g. first return, last return
                classification: "u8", // type of object detected
            },
        }
        "#;
    let topic: ExposedTopic = serde_json5::from_str(EXPOSED_TOPIC_EXAMPLE).unwrap();
    let topic2: ExposedTopic = serde_json5::from_str(topic2).unwrap();

    let (mut generator, output_dir, user_node) = init_test_env(&temp_dir);
    generator.add_exposed_topic(&topic).unwrap();
    generator.add_exposed_topic(&topic2).unwrap();
    let output_config = copy_config_to_output(&user_node, &output_dir);
    generator.build(&output_dir).unwrap();
    fs::remove_file(output_config).unwrap();

    assert!(
        output_dir.join("Cargo.toml").exists(),
        "Expected Cargo.toml to be generated in the temporary crate directory"
    );

    let lib_rs = output_dir.join("src/lib.rs");
    assert!(
        lib_rs.exists(),
        "Expected lib.rs to exist so `peppygen::topics` is reachable"
    );
    let lib_contents = std::fs::read_to_string(&lib_rs).expect("failed to read generated lib.rs");
    assert!(
        lib_contents.contains("pub mod topics;"),
        "Expected generated lib.rs to re-export the `topics` module, got:\n{}",
        lib_contents
    );

    let topics_mod = output_dir.join("src/topics.rs");
    assert!(
        topics_mod.exists(),
        "Expected topics module file to exist so `peppygen::topics::<module>` resolves"
    );
    let topics_contents =
        std::fs::read_to_string(&topics_mod).expect("failed to read topics module");
    assert!(
        topics_contents.contains("pub mod push_frame;"),
        "Expected topics module to expose generated `push_frame` module, got:\n{}",
        topics_contents
    );
    assert!(
        topics_contents.contains("pub mod push_lidar_object;"),
        "Expected topics module to expose generated `push_frame` module, got:\n{}",
        topics_contents
    );

    let push_frame_mod = output_dir.join("src/topics/push_frame.rs");
    assert!(
        push_frame_mod.exists(),
        "Expected generated topic module at {:?}",
        push_frame_mod
    );
    let push_frame_contents =
        std::fs::read_to_string(&push_frame_mod).expect("failed to read push_frame module");
    assert!(
        push_frame_contents.contains("pub fn push_frame("),
        "Expected generated module to expose sync topic function, got:\n{}",
        push_frame_contents
    );
    assert!(
        push_frame_contents.contains("pub async fn push_frame_async("),
        "Expected generated module to expose async topic function, got:\n{}",
        push_frame_contents
    );

    let push_lidar_mod = output_dir.join("src/topics/push_lidar_object.rs");
    assert!(
        push_lidar_mod.exists(),
        "Expected generated topic module at {:?}",
        push_lidar_mod
    );
    let push_lidar_contents =
        std::fs::read_to_string(&push_lidar_mod).expect("failed to read push_lidar_object module");
    assert!(
        push_lidar_contents.contains("pub fn push_lidar_object("),
        "Expected generated module to expose sync topic function, got:\n{}",
        push_lidar_contents
    );
    assert!(
        push_lidar_contents.contains("pub async fn push_lidar_object_async("),
        "Expected generated module to expose async topic function, got:\n{}",
        push_lidar_contents
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
        stream_contents
            .contains("pub async fn on_video_frame_received() -> OnVideoFrameReceivedArguments"),
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
