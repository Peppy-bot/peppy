use super::*;
use config::node::{ExposedTopic, SubscribedTopic};

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

#[test]
fn exposed_topic() {
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
        rendered.contains("crate::Error::TopicMessengerConnect"),
        &rendered,
        "expected explicit topic messenger error variant"
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
        rendered.contains("pub struct PushFrame {"),
        &rendered,
        "expected topic messenger struct"
    );
    assert_rendered!(
        rendered.contains("std::env::var(\"PEPPY_NAMESPACE\")"),
        &rendered,
        "expected namespace initialization from environment"
    );
    assert_rendered!(
        rendered.contains("pub async fn connect(host: &str, port: u16) -> crate::Result<Self>"),
        &rendered,
        "expected async connect constructor"
    );
    assert_rendered!(
        rendered.contains("peppylib::TopicMessenger::from_host_port(host, port)"),
        &rendered,
        "expected async messenger creation"
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
        rendered.contains("let namespace = self.namespace.as_str();"),
        &rendered,
        "expected namespace borrow"
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
        rendered.contains("self.messenger.emit"),
        &rendered,
        "expected messenger emit call"
    );
}

#[test]
fn exposed_double_topic() {
    let topic1: ExposedTopic = serde_json5::from_str(EXPOSED_TOPIC_EXAMPLE).unwrap();
    let topic2: ExposedTopic = serde_json5::from_str(EXPOSED_TOPIC_EXAMPLE2).unwrap();

    let mut generator = RustGenerator::new();
    generator.add_exposed_topic(&topic1).unwrap();
    generator.add_exposed_topic(&topic2).unwrap();
    let artifacts = generator.into_artifacts();

    assert_eq!(
        artifacts.len(),
        2,
        "expected two generated artifacts, got {}",
        artifacts.len()
    );

    let push_frame_rendered = artifacts
        .iter()
        .find(|artifact| artifact.node_name == "push_frame")
        .map(|artifact| &artifact.code_output)
        .expect("expected generated artifact for `push_frame`");
    let push_lidar_rendered = artifacts
        .iter()
        .find(|artifact| artifact.node_name == "push_lidar_object")
        .map(|artifact| &artifact.code_output)
        .expect("expected generated artifact for `push_lidar_object`");

    assert_rendered!(
        push_frame_rendered.contains("let mut message = capnp::message::Builder::new_default();"),
        push_frame_rendered,
        "expected capnp message builder"
    );
    assert_rendered!(
        push_lidar_rendered.contains("pub struct PushLidarObject {"),
        push_lidar_rendered,
        "expected topic messenger struct for `push_lidar_object`"
    );
    assert_rendered!(
        push_lidar_rendered.contains("pub struct PushLidarObjectHeader"),
        push_lidar_rendered,
        "expected nested header struct for `push_lidar_object`"
    );
}

#[test]
fn subscribed_to_topic() {
    let topic = r#"
        {
            node: "uvc_camera",
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
        rendered.contains("pub struct UvcCamera {"),
        &rendered,
        "expected subscriber struct definition"
    );
    assert_rendered!(
        rendered.contains("stream_subscription: peppylib::messaging::Subscription"),
        &rendered,
        "expected subscription field"
    );
    let on_next_usage_count = rendered.matches(".on_next_message()").count();
    assert_rendered!(
        on_next_usage_count == 1,
        &rendered,
        "expected subscription helper to await next message once, found {} occurrence(s)",
        on_next_usage_count
    );
    assert_rendered!(
        rendered.contains("pub async fn connect(host: &str, port: u16) -> crate::Result<Self>"),
        &rendered,
        "expected async connect constructor"
    );
    assert_rendered!(
        rendered.contains("peppylib::TopicMessenger::from_host_port(host, port)"),
        &rendered,
        "expected messenger initialization"
    );
    assert_rendered!(
        rendered.contains("pub async fn on_next_stream_message("),
        &rendered,
        "expected async subscriber method"
    );
    assert_rendered!(
        rendered.contains("&mut self"),
        &rendered,
        "expected mutable self receiver for subscriber method"
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
        rendered.contains("-> crate::Result<UvcCameraStreamMessage>"),
        &rendered,
        "expected subscriber return type"
    );
    assert_rendered!(
        rendered.contains(".subscribe(namespace_value, topic_name, qos)"),
        &rendered,
        "expected subscription call"
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
fn subscribed_to_double_topic_same_node() {
    let video_topic = r#"
        {
            node: "uvc_camera",
            name: "video",
            tag: "0.1.0"
        }
        "#;
    let video_topic: SubscribedTopic = serde_json5::from_str(video_topic).unwrap();
    let video_format = r#"
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
    let video_format: MessageFormat = serde_json5::from_str(video_format).unwrap();
    let sound_topic = r#"
        {
            node: "uvc_camera",
            name: "sound",
            tag: "0.1.0"
        }
    "#;
    let sound_topic: SubscribedTopic = serde_json5::from_str(sound_topic).unwrap();
    let sound_format = r#"
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
    let sound_format: MessageFormat = serde_json5::from_str(sound_format).unwrap();

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

    let struct_count = rendered.matches("pub struct UvcCamera {").count();
    assert_rendered!(
        struct_count == 1,
        &rendered,
        "expected a single struct definition for the subscriber node, got {}",
        struct_count
    );

    let connect_count = rendered.matches("pub async fn connect(").count();
    assert_rendered!(
        connect_count == 1,
        &rendered,
        "expected a single connect constructor, got {}",
        connect_count
    );
    assert_rendered!(
        rendered.contains("video_subscription: peppylib::messaging::Subscription"),
        &rendered,
        "expected subscription field for the first topic"
    );
    assert_rendered!(
        rendered.contains("sound_subscription: peppylib::messaging::Subscription"),
        &rendered,
        "expected dedicated subscription field for the second topic"
    );
    let on_next_usage_count = rendered.matches(".on_next_message()").count();
    assert_rendered!(
        on_next_usage_count == 2,
        &rendered,
        "expected each subscription to await the next message via helper, found {} occurrence(s)",
        on_next_usage_count
    );
    assert_rendered!(
        rendered.contains("pub struct UvcCameraVideoMessage"),
        &rendered,
        "expected video payload struct"
    );
    assert_rendered!(
        rendered.contains("pub struct UvcCameraSoundMessage"),
        &rendered,
        "expected sound payload struct"
    );
    assert_rendered!(
        rendered.contains("crate::Error::NodeMessengerConnect"),
        &rendered,
        "expected explicit node messenger error variant"
    );
    assert_rendered!(
        rendered.contains("crate::Error::TopicSubscribe"),
        &rendered,
        "expected explicit subscribe error variant for each topic"
    );
    assert_rendered!(
        rendered.contains("pub async fn on_next_video_message"),
        &rendered,
        "expected video subscriber method"
    );
    assert_rendered!(
        rendered.contains("pub async fn on_next_sound_message"),
        &rendered,
        "expected sound subscriber method"
    );
    assert_rendered!(
        rendered.contains("fn deseralize_video_payload"),
        &rendered,
        "expected video payload helper"
    );
    assert_rendered!(
        rendered.contains("fn deseralize_sound_payload"),
        &rendered,
        "expected sound payload helper"
    );
    assert_rendered!(
        rendered.contains("let topic_name = \"video\";"),
        &rendered,
        "expected connect routine to subscribe to the video topic"
    );
    assert_rendered!(
        rendered.contains("let topic_name = \"sound\";"),
        &rendered,
        "expected connect routine to subscribe to the sound topic"
    );
}

#[test]
fn create_lib_with_exposed_double_topic_artifact() {
    let temp_dir = TempDir::new().unwrap();
    let topic: ExposedTopic = serde_json5::from_str(EXPOSED_TOPIC_EXAMPLE).unwrap();
    let topic2: ExposedTopic = serde_json5::from_str(EXPOSED_TOPIC_EXAMPLE2).unwrap();

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
        push_frame_contents.contains("pub struct PushFrame {"),
        "Expected generated module to define topic struct, got:\n{}",
        push_frame_contents
    );
    assert!(
        push_frame_contents.contains("pub async fn connect("),
        "Expected generated module to expose async connect constructor, got:\n{}",
        push_frame_contents
    );
    assert!(
        push_frame_contents.contains("pub async fn emit("),
        "Expected generated module to expose async emit method, got:\n{}",
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
        push_lidar_contents.contains("pub struct PushLidarObject {"),
        "Expected generated module to define topic struct, got:\n{}",
        push_lidar_contents
    );
    assert!(
        push_lidar_contents.contains("pub async fn connect("),
        "Expected generated module to expose async connect constructor, got:\n{}",
        push_lidar_contents
    );
    assert!(
        push_lidar_contents.contains("pub async fn emit("),
        "Expected generated module to expose async emit method, got:\n{}",
        push_lidar_contents
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
        push_frame_contents.contains("pub struct PushFrame {"),
        "Expected generated module to define topic struct, got:\n{}",
        push_frame_contents
    );
    assert!(
        push_frame_contents.contains("pub async fn connect("),
        "Expected generated module to expose async connect constructor, got:\n{}",
        push_frame_contents
    );
    assert!(
        push_frame_contents.contains("pub async fn emit("),
        "Expected generated module to expose async emit method, got:\n{}",
        push_frame_contents
    );
}
