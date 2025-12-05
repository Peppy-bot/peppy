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

/// In the case of a topic, an "exposed" topic is an entity that emits messages
#[test]
fn expose_topic() {
    let topic: ExposedTopic = serde_json5::from_str(EXPOSED_TOPIC_EXAMPLE).unwrap();

    let mut generator = RustGenerator::new();
    generator.add_exposed_topic(&topic).unwrap();
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

    // Core message building
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

    // Field handling - test different field types
    assert_rendered!(
        rendered.contains("root.set_encoding(encoding.as_str());"),
        &rendered,
        "expected string field setter"
    );
    assert_rendered!(
        rendered.contains("root.set_image(image.as_ref());"),
        &rendered,
        "expected fixed-size array setter"
    );
    assert_rendered!(
        rendered.contains("root.reborrow().init_header();"),
        &rendered,
        "expected nested object initialization"
    );
    assert_rendered!(
        rendered.contains("peppylib::encoding::convert_time"),
        &rendered,
        "expected timestamp conversion helper"
    );

    // Generated structs and function signature
    assert_rendered!(
        rendered.contains("pub struct MessageHeader"),
        &rendered,
        "expected generated struct for nested object"
    );
    assert_rendered!(
        rendered.contains("image: [u8; 3]"),
        &rendered,
        "expected fixed-size array argument"
    );
    assert_rendered!(
        rendered.contains("pub async fn emit("),
        &rendered,
        "expected async emit method"
    );

    // Topic metadata
    assert_rendered!(
        rendered.contains("let as_topic = \"push_frame\";"),
        &rendered,
        "expected topic name literal"
    );
    assert_rendered!(
        rendered.contains("let qos = peppylib::config::QoSProfile::SensorData;"),
        &rendered,
        "expected qos profile literal"
    );

    // Messenger integration
    assert_rendered!(
        rendered.contains("peppylib::TopicMessenger::emit("),
        &rendered,
        "expected messenger emit call"
    );
}

#[test]
fn expose_two_topics() {
    let topic1: ExposedTopic = serde_json5::from_str(EXPOSED_TOPIC_EXAMPLE).unwrap();
    let topic2: ExposedTopic = serde_json5::from_str(EXPOSED_TOPIC_EXAMPLE2).unwrap();

    let mut generator = RustGenerator::new();
    generator.add_exposed_topic(&topic1).unwrap();
    generator.add_exposed_topic(&topic2).unwrap();
    let artifacts: Vec<String> = generator
        .into_artifacts()
        .into_iter()
        .map(|artifact| artifact.code_output)
        .collect();
    assert_eq!(
        artifacts.len(),
        2,
        "expected two generated artifacts, got {}",
        artifacts.len()
    );

    // Verify each topic gets its own distinct artifact with correct schema
    assert!(
        artifacts
            .iter()
            .any(|r| r.contains("push_frame_message_capnp")),
        "expected push_frame artifact"
    );
    assert!(
        artifacts
            .iter()
            .any(|r| r.contains("push_lidar_object_message_capnp")),
        "expected push_lidar_object artifact"
    );
}

/// In the case of a topic, a "subscribed" topic is an entity expects to receive messages from another entity
#[test]
fn subscribed_to_topic() {
    let topic: SubscribedTopic = serde_json5::from_str(SUBSCRIBED_TOPIC_EXAMPLE1).unwrap();
    let format: MessageFormat = serde_json5::from_str(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE1).unwrap();

    let mut generator = RustGenerator::new();
    generator.add_subscribed_topic(&topic, format).unwrap();
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

    // Generated structs with various field types
    assert_rendered!(
        rendered.contains("pub struct Message"),
        &rendered,
        "expected return struct definition"
    );
    assert_rendered!(
        rendered.contains("pub struct MessageHeader"),
        &rendered,
        "expected nested struct definition"
    );
    assert_rendered!(
        rendered.contains("pub stamp: std::time::SystemTime"),
        &rendered,
        "expected time field type"
    );
    assert_rendered!(
        rendered.contains("pub image: [u8; 3]"),
        &rendered,
        "expected fixed-size array field"
    );

    // Subscriber function signature
    assert_rendered!(
        rendered.contains("pub async fn on_next_message_received("),
        &rendered,
        "expected async subscriber method"
    );
    assert_rendered!(
        rendered.contains("master_node_target: Option<&str>"),
        &rendered,
        "expected master_node_target parameter"
    );
    assert_rendered!(
        rendered.contains("instance_id_target: Option<&str>"),
        &rendered,
        "expected instance_id_target parameter"
    );
    assert_rendered!(
        rendered.contains("-> crate::Result<(String, Message)>"),
        &rendered,
        "expected subscriber return type including instance id"
    );

    // Deserialization
    assert_rendered!(
        rendered.contains("fn deseralize_payload("),
        &rendered,
        "expected private payload deserializer function"
    );
    assert_rendered!(
        rendered.contains("capnp::serialize::read_message"),
        &rendered,
        "expected capnp deserialization"
    );

    // Topic metadata
    assert_rendered!(
        rendered.contains("let node_name = \"uvc_camera\";"),
        &rendered,
        "expected node_name literal"
    );

    // Messenger integration
    assert_rendered!(
        rendered.contains("peppylib::TopicMessenger::subscribe("),
        &rendered,
        "expected subscription helper invocation"
    );

    // Error variants
    assert_rendered!(
        rendered.contains("crate::Error::TopicSubscribe"),
        &rendered,
        "expected topic subscribe error variant"
    );
    assert_rendered!(
        rendered.contains("crate::Error::InvalidFixedBytes"),
        &rendered,
        "expected fixed-size byte validation error variant"
    );
}

#[test]
fn subscribed_to_two_topics_same_node() {
    let video_topic: SubscribedTopic = serde_json5::from_str(SUBSCRIBED_TOPIC_EXAMPLE1).unwrap();
    let video_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE1).unwrap();

    let sound_topic: SubscribedTopic = serde_json5::from_str(SUBSCRIBED_TOPIC_EXAMPLE2).unwrap();
    let sound_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE2).unwrap();

    let mut generator = RustGenerator::new();
    generator
        .add_subscribed_topic(&video_topic, video_format)
        .unwrap();
    generator
        .add_subscribed_topic(&sound_topic, sound_format)
        .unwrap();
    let artifacts: Vec<String> = generator
        .into_artifacts()
        .into_iter()
        .map(|artifact| artifact.code_output)
        .collect();
    assert_eq!(
        artifacts.len(),
        2,
        "expected two generated artifacts, got {}",
        artifacts.len()
    );

    // Verify each topic gets distinct artifact with correct topic name
    assert!(
        artifacts
            .iter()
            .any(|r| r.contains("let topic_name = \"stream\";")),
        "expected stream topic artifact"
    );
    assert!(
        artifacts
            .iter()
            .any(|r| r.contains("let topic_name = \"sound\";")),
        "expected sound topic artifact"
    );

    // Verify both reference the same source node
    for rendered in &artifacts {
        assert_rendered!(
            rendered.contains("let node_name = \"uvc_camera\";"),
            rendered,
            "expected node_name to reference source node"
        );
    }
}

/// This is a long running test that verifies the generated code compiles and passes clippy
#[test]
fn compile_lib_with_exposed_and_subscribed_topics() {
    let temp_dir = TempDir::new().unwrap();
    let exposed_topic1: ExposedTopic = serde_json5::from_str(EXPOSED_TOPIC_EXAMPLE).unwrap();
    let exposed_topic2: ExposedTopic = serde_json5::from_str(EXPOSED_TOPIC_EXAMPLE2).unwrap();
    let subscribed_topic1: SubscribedTopic =
        serde_json5::from_str(SUBSCRIBED_TOPIC_EXAMPLE1).unwrap();
    let subscribed_format1: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE1).unwrap();
    let subscribed_topic2: SubscribedTopic =
        serde_json5::from_str(SUBSCRIBED_TOPIC_EXAMPLE2).unwrap();
    let subscribed_format2: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE2).unwrap();

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
        !output_dir.join(PEPPY_NODE_CONFIG_FILE).exists(),
        "Generated crate should not keep a copy of the node configuration file"
    );

    let lib_contents =
        std::fs::read_to_string(output_dir.join("src/lib.rs")).expect("failed to read lib.rs");
    assert!(
        lib_contents.contains("pub mod exposed_topics;")
            && lib_contents.contains("pub mod subscribed_topics;"),
        "Expected lib.rs to re-export topic modules"
    );

    // Verify expected module files exist
    assert!(
        output_dir
            .join("src/exposed_topics/push_frame.rs")
            .exists(),
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
