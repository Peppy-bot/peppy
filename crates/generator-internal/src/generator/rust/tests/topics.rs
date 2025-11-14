use super::*;
use config::node::{ExposedTopic, SubscribedTopic};
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
        rendered.contains("header: MessageHeader"),
        &rendered,
        "expected structured header argument"
    );
    assert_rendered!(
        rendered.contains("image: [u8; 3]"),
        &rendered,
        "expected fixed-size array argument"
    );
    assert_rendered!(
        rendered.contains("pub struct MessageHeader"),
        &rendered,
        "expected generated struct for nested object"
    );
    assert_rendered!(
        rendered.contains("messenger: &crate::Messenger"),
        &rendered,
        "expected messenger parameter in emit signature"
    );
    assert_rendered!(
        rendered.contains("pub async fn emit("),
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
        2,
        "expected two generated artifacts, got {}",
        artifacts.len()
    );
    struct ArtifactExpectation<'a> {
        builder_snippet: &'a str,
        topic_literal: &'a str,
    }
    let expectations = [
        ArtifactExpectation {
            builder_snippet: "crate::capnp::push_frame_message_capnp::push_frame_message::Builder",
            topic_literal: "let topic_name = \"push_frame\";",
        },
        ArtifactExpectation {
            builder_snippet: "crate::capnp::push_lidar_object_message_capnp::push_lidar_object_message::Builder",
            topic_literal: "let topic_name = \"push_lidar_object\";",
        },
    ];

    for (rendered, expectation) in artifacts.iter().zip(expectations.iter()) {
        assert_rendered!(
            rendered.contains("let mut message = capnp::message::Builder::new_default();"),
            rendered,
            "expected capnp message builder"
        );
        assert_rendered!(
            rendered.contains(expectation.builder_snippet),
            rendered,
            "expected capnp root initialization `{}`",
            expectation.builder_snippet
        );
        assert_rendered!(
            rendered.contains("pub async fn emit("),
            rendered,
            "expected async emit method"
        );
        assert_rendered!(
            rendered.contains("pub struct MessageHeader"),
            rendered,
            "expected nested header struct"
        );
        assert_rendered!(
            rendered.contains(expectation.topic_literal),
            rendered,
            "expected topic literal `{}`",
            expectation.topic_literal
        );
    }
}

/// In the case of a topic, a "subscribed" topic is an entity expects to receive messages from another entity
#[test]
fn subscribed_to_topic() {
    let topic: SubscribedTopic = serde_json5::from_str(SUBSCRIBED_TOPIC_EXAMPLE1).unwrap();
    let format: MessageFormat = serde_json5::from_str(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE1).unwrap();

    let mut generator = RustGenerator::new();
    generator
        .add_subscribed_topic(&topic, vec![format])
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
        rendered.contains("pub frame_id: u32"),
        &rendered,
        "expected header frame identifier to be public"
    );
    assert_rendered!(
        rendered.contains("pub stamp: std::time::SystemTime"),
        &rendered,
        "expected header stamp to be public"
    );
    assert_rendered!(
        rendered.contains("pub encoding: String"),
        &rendered,
        "expected encoding field to be public"
    );
    assert_rendered!(
        rendered.contains("pub header: MessageHeader"),
        &rendered,
        "expected nested header field to be public"
    );
    assert_rendered!(
        rendered.contains("pub height: u32"),
        &rendered,
        "expected height field to be public"
    );
    assert_rendered!(
        rendered.contains("pub image: [u8; 3]"),
        &rendered,
        "expected array element field to be public"
    );
    assert_rendered!(
        rendered.contains("pub width: u32"),
        &rendered,
        "expected width field to be public"
    );
    assert_rendered!(
        rendered.contains("pub async fn on_next_message_received("),
        &rendered,
        "expected async subscriber method"
    );
    assert_rendered!(
        rendered.contains("messenger: &crate::Messenger"),
        &rendered,
        "expected messenger reference parameter"
    );
    assert_rendered!(
        rendered.contains("deseralize_payload(message.payload.as_ref())"),
        &rendered,
        "expected helper payload deserializer invocation"
    );
    assert_rendered!(
        rendered.contains("fn deseralize_payload("),
        &rendered,
        "expected private payload deserializer function"
    );
    assert_rendered!(
        rendered.contains("-> crate::Result<Message>"),
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
        .add_subscribed_topic(&video_topic, vec![video_format])
        .unwrap();
    generator
        .add_subscribed_topic(&sound_topic, vec![sound_format])
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
    struct ArtifactExpectation<'a> {
        topic_literal: &'a str,
    }
    let expectations = [
        ArtifactExpectation {
            topic_literal: "let topic_name = \"stream\";",
        },
        ArtifactExpectation {
            topic_literal: "let topic_name = \"sound\";",
        },
    ];

    for (rendered, expectation) in artifacts.iter().zip(expectations.iter()) {
        let on_next_usage_count = rendered.matches(".on_next_message()").count();
        assert_rendered!(
            on_next_usage_count == 1,
            rendered,
            "expected a single subscription helper invocation per artifact, found {} occurrence(s)",
            on_next_usage_count
        );
        assert_rendered!(
            rendered.contains("pub struct Message"),
            rendered,
            "expected payload struct `Message`"
        );
        assert_rendered!(
            rendered.contains("crate::Error::TopicSubscribe"),
            rendered,
            "expected explicit subscribe error variant"
        );
        assert_rendered!(
            rendered.contains("pub async fn on_next_message_received("),
            rendered,
            "expected subscriber method"
        );
        assert_rendered!(
            rendered.contains("fn deseralize_payload("),
            rendered,
            "expected payload helper"
        );
        assert_rendered!(
            rendered.contains("messenger.handle()"),
            rendered,
            "expected messenger handle usage"
        );
        assert_rendered!(
            rendered.contains("messenger: &crate::Messenger"),
            rendered,
            "expected messenger parameter"
        );
        assert_rendered!(
            rendered.contains("messenger.namespace()"),
            rendered,
            "expected namespace resolution via messenger"
        );
        assert_rendered!(
            rendered.contains(expectation.topic_literal),
            rendered,
            "expected subscriber routine to set topic literal `{}`",
            expectation.topic_literal
        );
    }
}

/// Topics are the only entities that can subscribe to a particular topic name without specifying the `node` attribute.
/// Since the topic does not point to a specific node, the `message_format` cannot be determined in advance.
/// In that case the generated code simply generates all functions known to have the same `topic` name.
#[test]
fn subscribed_to_topic_no_node() {
    // Here we don't use a node name so any node in the network with a topic named "stream" is gonna be captured
    let topic = r#"
        {
            name: "stream",
            tag: "0.1.0" // Tag can be specified here, but will be ignored
        }
        "#;
    let topic: SubscribedTopic = serde_json5::from_str(topic).unwrap();
    let format1: &str = r#"
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
    let format1: MessageFormat = serde_json5::from_str(format1).unwrap();

    let format2: &str = r#"
    {
        width: "u32",
        height: "u32",
        image: {
            $type: "array",
            $items: "u8",
            $length: 3
        }
    }
    "#;
    let format2: MessageFormat = serde_json5::from_str(format2).unwrap();

    let mut generator = RustGenerator::new();
    generator
        .add_subscribed_topic(&topic, vec![format1, format2])
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
        rendered.contains("pub struct Message1"),
        &rendered,
        "expected first typed message struct"
    );
    assert_rendered!(
        rendered.contains("pub struct Message1Header"),
        &rendered,
        "expected nested header struct for first schema"
    );
    assert_rendered!(
        rendered.contains("pub struct Message2"),
        &rendered,
        "expected second typed message struct"
    );
    assert_rendered!(
        rendered.contains("pub async fn on_next_message_received_1("),
        &rendered,
        "expected subscriber for first schema"
    );
    assert_rendered!(
        rendered.contains("pub async fn on_next_message_received_2("),
        &rendered,
        "expected subscriber for second schema"
    );
    assert_rendered!(
        rendered.contains("fn deseralize_payload_1("),
        &rendered,
        "expected helper for first schema"
    );
    assert_rendered!(
        rendered.contains("fn deseralize_payload_2("),
        &rendered,
        "expected helper for second schema"
    );
    assert_rendered!(
        rendered.contains("-> crate::Result<Message1>"),
        &rendered,
        "expected typed return for first schema"
    );
    assert_rendered!(
        rendered.contains("-> crate::Result<Message2>"),
        &rendered,
        "expected typed return for second schema"
    );
    assert_rendered!(
        rendered.contains("peppylib::TopicMessenger::subscribe"),
        &rendered,
        "expected subscription helper usage"
    );
    assert_rendered!(
        rendered.contains("capnp::serialize::read_message"),
        &rendered,
        "expected capnp deserialization in all variants"
    );
    assert_rendered!(
        !rendered.contains("-> crate::Result<bytes::Bytes>"),
        &rendered,
        "expected typed payloads instead of raw bytes"
    );
}

/// This is a long running test
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

    let (mut generator, output_dir, user_node) = init_test_env(&temp_dir);
    generator.add_exposed_topic(&exposed_topic1).unwrap();
    generator.add_exposed_topic(&exposed_topic2).unwrap();
    generator
        .add_subscribed_topic(&subscribed_topic1, vec![subscribed_format1])
        .unwrap();
    generator
        .add_subscribed_topic(&subscribed_topic2, vec![subscribed_format2])
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
    assert!(lib_rs.exists(), "Expected generated lib.rs to exist");
    let lib_contents = std::fs::read_to_string(&lib_rs).expect("failed to read generated lib.rs");
    assert!(
        lib_contents.contains("pub mod exposed_topics;"),
        "Expected generated lib.rs to re-export the `exposed_topics` module, got:\n{}",
        lib_contents
    );
    assert!(
        lib_contents.contains("pub mod subscribed_topics;"),
        "Expected generated lib.rs to re-export the `subscribed_topics` module, got:\n{}",
        lib_contents
    );

    let exposes_mod = output_dir.join("src/exposed_topics.rs");
    assert!(
        exposes_mod.exists(),
        "Expected exposed_topics module file at {:?}",
        exposes_mod
    );
    let exposes_contents =
        std::fs::read_to_string(&exposes_mod).expect("failed to read exposed_topics module");
    for expected in ["pub mod push_frame;", "pub mod push_lidar_object;"] {
        assert!(
            exposes_contents.contains(expected),
            "Expected exposed_topics module to declare `{expected}`, got:\n{}",
            exposes_contents
        );
    }

    let subscribes_mod = output_dir.join("src/subscribed_topics.rs");
    assert!(
        subscribes_mod.exists(),
        "Expected subscribed_topics module file at {:?}",
        subscribes_mod
    );
    let subscribes_contents =
        std::fs::read_to_string(&subscribes_mod).expect("failed to read subscribed_topics module");
    for expected in ["pub mod uvc_camera_stream;", "pub mod uvc_camera_sound;"] {
        assert!(
            subscribes_contents.contains(expected),
            "Expected subscribed_topics module to declare `{expected}`, got:\n{}",
            subscribes_contents
        );
    }

    let push_frame_module = output_dir.join("src/exposed_topics").join("push_frame.rs");
    let push_frame_contents =
        std::fs::read_to_string(&push_frame_module).expect("failed to read generated topic module");
    assert!(
        push_frame_contents.contains("pub struct MessageHeader"),
        "Expected generated module to define message struct, got:\n{}",
        push_frame_contents
    );
    assert!(
        push_frame_contents.contains("pub async fn emit("),
        "Expected generated module to expose async emit method, got:\n{}",
        push_frame_contents
    );
    assert!(
        push_frame_contents.contains("messenger: &crate::Messenger"),
        "Expected generated emit method to accept messenger reference, got:\n{}",
        push_frame_contents
    );

    let subscriber_module = output_dir
        .join("src/subscribed_topics")
        .join("uvc_camera_stream.rs");
    let subscriber_contents = std::fs::read_to_string(&subscriber_module)
        .expect("failed to read generated subscriber module");
    assert!(
        subscriber_contents.contains("pub struct Message"),
        "Expected generated subscriber module to define message struct, got:\n{}",
        subscriber_contents
    );
    assert!(
        subscriber_contents.contains("pub async fn on_next_message_received("),
        "Expected generated subscriber module to expose async callback, got:\n{}",
        subscriber_contents
    );
    assert!(
        subscriber_contents.contains("messenger: &crate::Messenger"),
        "Expected generated subscriber module to accept messenger reference, got:\n{}",
        subscriber_contents
    );
}
