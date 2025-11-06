use super::*;
use config::node::{ExposedTopic, SubscribedTopic};
use std::process::Command;

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

const EXPOSED_TOPIC_EXAMPLE2: &str = r#"
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

const SUBSCRIBED_TOPIC_EXAMPLE1: &str = r#"
{
    node: "uvc_camera",
    name: "stream",
    tag: "0.1.0"
}
"#;

const SUBSCRIBED_TOPIC_FORMAT_EXAMPLE1: &str = r#"
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

const SUBSCRIBED_TOPIC_EXAMPLE2: &str = r#"
{
    node: "uvc_camera",
    name: "sound",
    tag: "0.1.0"
}
"#;

const SUBSCRIBED_TOPIC_FORMAT_EXAMPLE2: &str = r#"
{
  header: {
    type: "object",
    stamp: "time"
  },
  encoding: "string",         // e.g., "pcm_s16le", "f32", "mp3", "opus"
  sample_rate: "u32",         // Hz
  channels: "u32",            // e.g., 1=mono, 2=stereo
  layout: "string",           // "interleaved" | "planar"
  frame_count: "u32",         // samples per channel in this frame
  samples: {
    type: "array",
    items: "u8",              // raw bytes; interpret per 'encoding'
  }
}
"#;

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
        rendered.contains("peppylib::encoding::convert_time"),
        &rendered,
        "expected timestamp conversion helper"
    );
    assert_rendered!(
        rendered.contains("capnp::serialize::write_message"),
        &rendered,
        "expected serialization call"
    );
    assert_rendered!(
        rendered.contains("crate::Error::CapnpSerialize"),
        &rendered,
        "expected explicit capnp serialization error variant"
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
    assert_rendered!(
        rendered.contains("pub struct Exposes;"),
        &rendered,
        "expected topic messenger struct"
    );
    assert_rendered!(
        rendered.contains("messenger: &crate::Messenger"),
        &rendered,
        "expected messenger parameter in emit signature"
    );
    assert_rendered!(
        rendered.contains("pub async fn emit_push_frame("),
        &rendered,
        "expected async emit method"
    );
    assert_rendered!(
        rendered.contains("-> crate::Result<()>"),
        &rendered,
        "expected crate result type"
    );
    assert_rendered!(
        rendered.contains("bytes::Bytes::from(buffer)"),
        &rendered,
        "expected bytes payload conversion"
    );
    assert_rendered!(
        rendered.contains("let topic_name = \"push_frame\";"),
        &rendered,
        "expected topic name literal"
    );
    assert_rendered!(
        rendered.contains("let qos = peppylib::config::QoSProfile::SensorData;"),
        &rendered,
        "expected qos profile literal"
    );
    assert_rendered!(
        rendered.contains("peppylib::TopicMessenger::emit("),
        &rendered,
        "expected messenger emit call"
    );
    assert_rendered!(
        rendered.contains("messenger.handle()"),
        &rendered,
        "expected messenger handle usage"
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
        1,
        "expected a single generated artifact, got {}",
        artifacts.len()
    );
    let rendered = artifacts.into_iter().next().expect("artifact is present");

    assert_rendered!(
        rendered.contains("let mut message = capnp::message::Builder::new_default();"),
        rendered,
        "expected capnp message builder"
    );
    assert_rendered!(
        rendered.contains("pub struct Exposes;"),
        rendered,
        "expected topic messenger struct for `push_lidar_object`"
    );
    assert_rendered!(
        rendered.contains("pub async fn emit_push_lidar_object("),
        rendered,
        "expected async emit method for `push_lidar_object`"
    );
    assert_rendered!(
        rendered.contains("pub struct PushLidarObjectHeader"),
        rendered,
        "expected nested header struct for `push_lidar_object`"
    );
}

#[test]
fn subscribed_to_topic() {
    let topic: SubscribedTopic = serde_json5::from_str(SUBSCRIBED_TOPIC_EXAMPLE1).unwrap();
    let format: MessageFormat = serde_json5::from_str(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE1).unwrap();

    let mut generator = RustGenerator::new();
    generator
        .add_subscribed_topic(&topic, Some(&format))
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
        rendered.contains("pub struct UvcCameraStreamMessage"),
        &rendered,
        "expected return struct definition"
    );
    assert_rendered!(
        rendered.contains("pub struct UvcCameraStreamMessageHeader"),
        &rendered,
        "expected nested struct definition"
    );
    assert_rendered!(
        rendered.contains("image: [u8; 3]"),
        &rendered,
        "expected array element type"
    );
    assert_rendered!(
        rendered.contains("pub struct Subscribes;"),
        &rendered,
        "expected subscriber struct definition"
    );
    assert_rendered!(
        !rendered.contains("pub async fn connect("),
        &rendered,
        "subscriber should not expose a connect constructor"
    );
    assert_rendered!(
        rendered.contains("pub async fn on_next_uvc_camera_stream_message("),
        &rendered,
        "expected async subscriber method"
    );
    assert_rendered!(
        rendered.contains("messenger: &crate::Messenger"),
        &rendered,
        "expected messenger reference parameter"
    );
    assert_rendered!(
        rendered.contains("Self::deseralize_uvc_camera_stream_payload"),
        &rendered,
        "expected helper payload deserializer invocation"
    );
    assert_rendered!(
        rendered.contains("fn deseralize_uvc_camera_stream_payload("),
        &rendered,
        "expected private payload deserializer function"
    );
    assert_rendered!(
        rendered.contains("-> crate::Result<UvcCameraStreamMessage>"),
        &rendered,
        "expected subscriber return type"
    );
    assert_rendered!(
        rendered.contains("peppylib::TopicMessenger::subscribe("),
        &rendered,
        "expected subscription helper invocation"
    );
    assert_rendered!(
        rendered.contains("messenger.handle()"),
        &rendered,
        "expected messenger handle to be passed to subscription helper"
    );
    assert_rendered!(
        rendered.contains("let namespace = messenger.namespace();"),
        &rendered,
        "expected namespace lookup via messenger helper"
    );
    assert_rendered!(
        rendered.contains("capnp::serialize::read_message"),
        &rendered,
        "expected capnp deserialization"
    );
    assert_rendered!(
        rendered.contains("crate::Error::TopicSubscribe"),
        &rendered,
        "expected explicit topic subscribe error variant"
    );
    assert_rendered!(
        rendered.contains("crate::Error::SubscriptionClosed"),
        &rendered,
        "expected explicit subscription closed error variant"
    );
    assert_rendered!(
        rendered.contains("crate::Error::CapnpDeserialize"),
        &rendered,
        "expected explicit capnp deserialize error variant"
    );
    assert_rendered!(
        rendered.contains("crate::Error::CapnpField"),
        &rendered,
        "expected explicit capnp field access error variant"
    );
    assert_rendered!(
        rendered.contains("crate::Error::InvalidFixedBytes"),
        &rendered,
        "expected explicit fixed-size byte validation error variant"
    );
    assert_rendered!(
        rendered.contains("let qos = peppylib::config::QoSProfile::Standard;"),
        &rendered,
        "expected qos initialization"
    );
}

#[test]
fn subscribed_to_topic_no_node() {
    // Here we don't use a node name so any node in the network with a topic named "stream" is gonna be captured
    let topic = r#"
        {
            name: "stream",
            tag: "0.1.0"
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
    assert_eq!(
        artifacts.len(),
        1,
        "expected a single generated artifact, got {}",
        artifacts.len()
    );
    let rendered = artifacts.into_iter().next().expect("artifact is present");

    assert_rendered!(
        rendered.contains("pub struct StreamMessage"),
        &rendered,
        "expected return struct definition"
    );
    assert_rendered!(
        rendered.contains("pub struct StreamMessageHeader"),
        &rendered,
        "expected nested struct definition"
    );
    assert_rendered!(
        rendered.contains("image: [u8; 3]"),
        &rendered,
        "expected array element type"
    );
    assert_rendered!(
        rendered.contains("pub struct Subscribes;"),
        &rendered,
        "expected subscriber struct definition"
    );
    assert_rendered!(
        !rendered.contains("pub async fn connect("),
        &rendered,
        "subscriber should not expose a connect constructor"
    );
    assert_rendered!(
        rendered.contains("pub async fn on_next_stream_message("),
        &rendered,
        "expected async subscriber method"
    );
    assert_rendered!(
        rendered.contains("messenger: &crate::Messenger"),
        &rendered,
        "expected messenger reference parameter"
    );
    assert_rendered!(
        rendered.contains("Self::deseralize_stream_payload"),
        &rendered,
        "expected helper payload deserializer invocation"
    );
    assert_rendered!(
        rendered.contains("fn deseralize_stream_payload("),
        &rendered,
        "expected private payload deserializer function"
    );
    assert_rendered!(
        rendered.contains("-> crate::Result<StreamMessage>"),
        &rendered,
        "expected subscriber return type"
    );
    assert_rendered!(
        rendered.contains("peppylib::TopicMessenger::subscribe("),
        &rendered,
        "expected subscription helper invocation"
    );
    assert_rendered!(
        rendered.contains("messenger.handle()"),
        &rendered,
        "expected messenger handle to be passed to subscription helper"
    );
    assert_rendered!(
        rendered.contains("let namespace = messenger.namespace();"),
        &rendered,
        "expected namespace lookup via messenger helper"
    );
    assert_rendered!(
        rendered.contains("capnp::serialize::read_message"),
        &rendered,
        "expected capnp deserialization"
    );
    assert_rendered!(
        rendered.contains("crate::Error::TopicSubscribe"),
        &rendered,
        "expected explicit topic subscribe error variant"
    );
    assert_rendered!(
        rendered.contains("crate::Error::SubscriptionClosed"),
        &rendered,
        "expected explicit subscription closed error variant"
    );
    assert_rendered!(
        rendered.contains("crate::Error::CapnpDeserialize"),
        &rendered,
        "expected explicit capnp deserialize error variant"
    );
    assert_rendered!(
        rendered.contains("crate::Error::CapnpField"),
        &rendered,
        "expected explicit capnp field access error variant"
    );
    assert_rendered!(
        rendered.contains("crate::Error::InvalidFixedBytes"),
        &rendered,
        "expected explicit fixed-size byte validation error variant"
    );
    assert_rendered!(
        rendered.contains("let qos = peppylib::config::QoSProfile::Standard;"),
        &rendered,
        "expected qos initialization"
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
        .add_subscribed_topic(&video_topic, Some(&video_format))
        .unwrap();
    generator
        .add_subscribed_topic(&sound_topic, Some(&sound_format))
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

    let struct_count = rendered.matches("pub struct Subscribes;").count();
    assert_rendered!(
        struct_count == 1,
        &rendered,
        "expected a single struct definition for the subscriber node, got {}",
        struct_count
    );

    assert_rendered!(
        !rendered.contains("pub async fn connect("),
        &rendered,
        "subscriber should not expose a connect constructor"
    );
    let on_next_usage_count = rendered.matches(".on_next_message()").count();
    assert_rendered!(
        on_next_usage_count == 2,
        &rendered,
        "expected each subscription to await the next message via helper, found {} occurrence(s)",
        on_next_usage_count
    );
    assert_rendered!(
        rendered.contains("pub struct UvcCameraStreamMessage"),
        &rendered,
        "expected stream payload struct"
    );
    assert_rendered!(
        rendered.contains("pub struct UvcCameraSoundMessage"),
        &rendered,
        "expected sound payload struct"
    );
    assert_rendered!(
        rendered.contains("crate::Error::TopicSubscribe"),
        &rendered,
        "expected explicit subscribe error variant for each topic"
    );
    assert_rendered!(
        rendered.contains("pub async fn on_next_uvc_camera_stream_message("),
        &rendered,
        "expected stream subscriber method"
    );
    assert_rendered!(
        rendered.contains("pub async fn on_next_uvc_camera_sound_message("),
        &rendered,
        "expected sound subscriber method"
    );
    assert_rendered!(
        rendered.contains("fn deseralize_uvc_camera_stream_payload"),
        &rendered,
        "expected stream payload helper"
    );
    assert_rendered!(
        rendered.contains("fn deseralize_uvc_camera_sound_payload"),
        &rendered,
        "expected sound payload helper"
    );
    let handle_usage_count = rendered.matches("messenger.handle()").count();
    assert_rendered!(
        handle_usage_count == 2,
        &rendered,
        "expected each subscriber method to pass the messenger handle, got {} occurrence(s)",
        handle_usage_count
    );
    let messenger_param_count = rendered.matches("messenger: &crate::Messenger").count();
    assert_rendered!(
        messenger_param_count == 2,
        &rendered,
        "expected each subscriber method to accept a messenger reference, got {} occurrence(s)",
        messenger_param_count
    );
    let namespace_usage_count = rendered.matches("messenger.namespace()").count();
    assert_rendered!(
        namespace_usage_count == 2,
        &rendered,
        "expected each subscriber method to resolve namespace from messenger, got {} occurrence(s)",
        namespace_usage_count
    );
    assert_rendered!(
        rendered.contains("let topic_name = \"stream\";"),
        &rendered,
        "expected stream subscriber routine to set topic literal"
    );
    assert_rendered!(
        rendered.contains("let topic_name = \"sound\";"),
        &rendered,
        "expected sound subscriber routine to set topic literal"
    );
}

/// This is a long running test
#[test]
fn compile_lib_with_exposed_topic_artifact() {
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

    // TODO: Add a second exposed topic and subscribed topics

    let (mut generator, output_dir, user_node) = init_test_env(&temp_dir);
    generator.add_exposed_topic(&exposed_topic1).unwrap();
    generator.add_exposed_topic(&exposed_topic2).unwrap();
    generator
        .add_subscribed_topic(&subscribed_topic1, Some(&subscribed_format1))
        .unwrap();
    generator
        .add_subscribed_topic(&subscribed_topic2, Some(&subscribed_format2))
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
    let topic_modules: Vec<String> = topics_contents
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            trimmed
                .strip_prefix("pub mod ")
                .map(|rest| rest.trim_end_matches(';').trim().to_string())
        })
        .collect();
    assert!(
        !topic_modules.is_empty(),
        "Expected topics module to expose at least one generated topic module, got:\n{}",
        topics_contents
    );
    assert_eq!(
        topic_modules.len(),
        1,
        "Expected a single generated topic module, got {:?}",
        topic_modules
    );
    let generated_module = &topic_modules[0];

    let topic_module_path = output_dir
        .join("src/topics")
        .join(format!("{generated_module}.rs"));
    assert!(
        topic_module_path.exists(),
        "Expected generated topic module at {:?}",
        topic_module_path
    );
    let push_frame_contents =
        std::fs::read_to_string(&topic_module_path).expect("failed to read generated topic module");
    assert!(
        push_frame_contents.contains("pub struct PushFrameHeader"),
        "Expected generated module to define message struct, got:\n{}",
        push_frame_contents
    );
    assert!(
        push_frame_contents.contains("pub struct Exposes;"),
        "Expected generated module to define exposes struct, got:\n{}",
        push_frame_contents
    );
    assert!(
        push_frame_contents.contains("pub async fn emit_push_frame("),
        "Expected generated module to expose async emit method, got:\n{}",
        push_frame_contents
    );
    assert!(
        push_frame_contents.contains("messenger: &crate::Messenger"),
        "Expected generated emit method to accept messenger reference, got:\n{}",
        push_frame_contents
    );
}
