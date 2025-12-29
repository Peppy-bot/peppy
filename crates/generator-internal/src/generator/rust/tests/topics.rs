use super::*;
use config::node::{ExposedTopic, MessageFormat, SubscribedTopic};
use std::process::Command;

const EXPOSED_TOPIC_EXAMPLE: &str = r#"
{
  name: "push_frame",
  qos_profile: "sensor_data",
  message_format: {
    header: {
      $type: "object",
      stamp: "time",
      frame_id: "u32"
    },
    encoding: "string",
    width: "u32",
    height: "u32",
    image: {
      $type: "array",
      $items: "u8",
      $length: 3
    }
  }
}
"#;

const EXPOSED_TOPIC_EXAMPLE2: &str = r#"
{
  name: "push_lidar_object", // The name of the topic inside the `lidar_sensor` node
  qos_profile: "sensor_data",
  message_format: {
    header: {
      $type: "object",
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

const SUBSCRIBED_TOPIC_EXAMPLE1: &str = r#"
{
    id: "stream",
    node: "uvc_camera",
    name: "stream",
    tag: "0.1.0"
}
"#;

const SUBSCRIBED_TOPIC_FORMAT_EXAMPLE1: &str = r#"
{
    header: {
        $type: "object",
        stamp: "time",
        frame_id: "u32"
    },
    encoding: "string",
    width: "u32",
    height: "u32",
    image: {
        $type: "array",
        $items: "u8",
        $length: 3
    }
}
"#;

const SUBSCRIBED_TOPIC_EXAMPLE2: &str = r#"
{
    id: "sound",
    node: "uvc_camera",
    name: "sound",
    tag: "0.1.0"
}
"#;

const SUBSCRIBED_TOPIC_FORMAT_EXAMPLE2: &str = r#"
{
  header: {
    $type: "object",
    stamp: "time"
  },
  encoding: "string",         // e.g., "pcm_s16le", "f32", "mp3", "opus"
  sample_rate: "u32",         // Hz
  channels: "u32",            // e.g., 1=mono, 2=stereo
  layout: "string",           // "interleaved" | "planar"
  frame_count: "u32",         // samples per channel in this frame
  samples: {
    $type: "array",
    $items: "u8",              // raw bytes; interpret per 'encoding'
  }
}
"#;

fn parse_exposed_topic(example: &str) -> ExposedTopic {
    serde_json5::from_str(example).unwrap()
}

fn parse_subscribed_topic(example: &str) -> SubscribedTopic {
    serde_json5::from_str(example).unwrap()
}

fn parse_message_format(example: &str) -> MessageFormat {
    serde_json5::from_str(example).unwrap()
}

/// In the case of a topic, an "exposed" topic is an entity that emits messages
#[test]
fn expose_topic() {
    let topic = parse_exposed_topic(EXPOSED_TOPIC_EXAMPLE);

    let mut generator = RustGenerator::new();
    generator.add_exposed_topic(&topic).unwrap();
    let artifacts = render_artifacts(generator);
    assert_eq!(
        artifacts.len(),
        1,
        "expected a single generated artifact, got {}",
        artifacts.len()
    );
    let rendered = artifacts.into_iter().next().expect("artifact is present");

    // Core message building
    assert_contains_all(
        &rendered,
        &[
            "let mut capnp_msg = capnp::message::Builder::new_default();",
            "crate::capnp::push_frame_message_capnp::push_frame_message::Builder",
        ],
    );

    // Field handling - test different field types
    assert_contains_all(
        &rendered,
        &[
            "root.set_encoding(encoding.as_str());",
            "root.set_image(image.as_ref());",
            "root.reborrow().init_header();",
            "peppylib::encoding::convert_time",
        ],
    );

    // Generated structs and function signature
    assert_contains_all(
        &rendered,
        &[
            "pub struct MessageHeader",
            "image: [u8; 3]",
            "pub async fn emit(",
        ],
    );

    // Topic metadata
    assert_contains_all(
        &rendered,
        &[
            "let as_topic = \"push_frame\";",
            "let qos = peppylib::config::QoSProfile::SensorData;",
            "peppylib::TopicMessenger::emit(",
        ],
    );
}

#[test]
fn expose_two_topics() {
    let topic1 = parse_exposed_topic(EXPOSED_TOPIC_EXAMPLE);
    let topic2 = parse_exposed_topic(EXPOSED_TOPIC_EXAMPLE2);

    let mut generator = RustGenerator::new();
    generator.add_exposed_topic(&topic1).unwrap();
    generator.add_exposed_topic(&topic2).unwrap();
    let artifacts = render_artifacts(generator);
    assert_eq!(
        artifacts.len(),
        2,
        "expected two generated artifacts, got {}",
        artifacts.len()
    );

    // Verify each topic gets its own distinct artifact with correct schema
    assert_artifact_contains(&artifacts, "push_frame_message_capnp");
    assert_artifact_contains(&artifacts, "push_lidar_object_message_capnp");
}

/// In the case of a topic, a "subscribed" topic is an entity expects to receive messages from another entity
#[test]
fn subscribed_to_topic() {
    let topic = parse_subscribed_topic(SUBSCRIBED_TOPIC_EXAMPLE1);
    let format = parse_message_format(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE1);

    let mut generator = RustGenerator::new();
    generator.add_subscribed_topic(&topic, format).unwrap();
    let artifacts = render_artifacts(generator);
    assert_eq!(
        artifacts.len(),
        1,
        "expected a single generated artifact, got {}",
        artifacts.len()
    );
    let rendered = artifacts.into_iter().next().expect("artifact is present");

    // Generated structs with various field types
    assert_contains_all(
        &rendered,
        &[
            "pub struct Message",
            "pub struct MessageHeader",
            "pub stamp: std::time::SystemTime",
            "pub image: [u8; 3]",
        ],
    );

    // Subscriber function signature
    assert_contains_all(
        &rendered,
        &[
            "pub async fn on_next_message_received(",
            "master_node_target: Option<&str>",
            "instance_id_target: Option<&str>",
            "-> crate::Result<(String, Message)>",
        ],
    );

    // Deserialization
    assert_contains_all(
        &rendered,
        &["fn deseralize_payload(", "capnp::serialize::read_message"],
    );

    // Topic metadata
    assert_contains_all(
        &rendered,
        &[
            "let node_name = \"uvc_camera\";",
            "peppylib::TopicMessenger::subscribe(",
        ],
    );

    // Error variants
    assert_contains_all(
        &rendered,
        &[
            "crate::Error::TopicSubscribe",
            "crate::Error::InvalidFixedBytes",
        ],
    );
}

#[test]
fn subscribed_to_two_topics_same_node() {
    let video_topic = parse_subscribed_topic(SUBSCRIBED_TOPIC_EXAMPLE1);
    let video_format = parse_message_format(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE1);

    let sound_topic = parse_subscribed_topic(SUBSCRIBED_TOPIC_EXAMPLE2);
    let sound_format = parse_message_format(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE2);

    let mut generator = RustGenerator::new();
    generator
        .add_subscribed_topic(&video_topic, video_format)
        .unwrap();
    generator
        .add_subscribed_topic(&sound_topic, sound_format)
        .unwrap();
    let artifacts = render_artifacts(generator);
    assert_eq!(
        artifacts.len(),
        2,
        "expected two generated artifacts, got {}",
        artifacts.len()
    );

    // Verify each topic gets distinct artifact with correct topic name
    assert_artifact_contains(&artifacts, "let topic_name = \"stream\";");
    assert_artifact_contains(&artifacts, "let topic_name = \"sound\";");

    // Verify both reference the same source node
    for rendered in &artifacts {
        assert_contains_all(rendered, &["let node_name = \"uvc_camera\";"]);
    }
}

/// This is a long running test that verifies the generated code compiles and passes clippy
#[test]
fn compile_lib_with_exposed_and_subscribed_topics() {
    let temp_dir = TempDir::new().unwrap();
    let exposed_topic1 = parse_exposed_topic(EXPOSED_TOPIC_EXAMPLE);
    let exposed_topic2 = parse_exposed_topic(EXPOSED_TOPIC_EXAMPLE2);
    let subscribed_topic1 = parse_subscribed_topic(SUBSCRIBED_TOPIC_EXAMPLE1);
    let subscribed_format1 = parse_message_format(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE1);
    let subscribed_topic2 = parse_subscribed_topic(SUBSCRIBED_TOPIC_EXAMPLE2);
    let subscribed_format2 = parse_message_format(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE2);

    let (mut generator, output_dir, user_node, _) = init_test_env(&temp_dir);
    generator.add_exposed_topic(&exposed_topic1).unwrap();
    generator.add_exposed_topic(&exposed_topic2).unwrap();
    generator
        .add_subscribed_topic(&subscribed_topic1, subscribed_format1)
        .unwrap();
    generator
        .add_subscribed_topic(&subscribed_topic2, subscribed_format2)
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

    // Verify module structure is generated correctly
    assert!(
        output_dir.join("Cargo.toml").exists(),
        "Expected Cargo.toml to be generated"
    );
    assert!(
        !output_dir.join(NODE_CONFIG_FILE).exists(),
        "Generated crate should not keep a copy of the node configuration file"
    );

    let lib_contents =
        std::fs::read_to_string(output_dir.join("src/lib.rs")).expect("failed to read lib.rs");
    assert_contains_all(
        &lib_contents,
        &["pub mod exposed_topics;", "pub mod subscribed_topics;"],
    );

    // Verify expected module files exist
    assert!(
        output_dir.join("src/exposed_topics/push_frame.rs").exists(),
        "Expected push_frame module"
    );
    assert!(
        output_dir
            .join("src/exposed_topics/push_lidar_object.rs")
            .exists(),
        "Expected push_lidar_object module"
    );
    assert!(
        output_dir
            .join("src/subscribed_topics/uvc_camera_stream.rs")
            .exists(),
        "Expected uvc_camera_stream subscriber module"
    );
    assert!(
        output_dir
            .join("src/subscribed_topics/uvc_camera_sound.rs")
            .exists(),
        "Expected uvc_camera_sound subscriber module"
    );
}
