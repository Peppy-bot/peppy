use super::*;
use config::node::{ConsumedTopic, EmittedTopic, MessageFormat, PeppygenLanguage};

const EMITTED_TOPIC_EXAMPLE: &str = r#"
{
  name: "video_stream",
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
    frame: {
      $type: "array",
      $items: "u8"
    }
  }
}
"#;

const EMITTED_TOPIC_EXAMPLE_EMPTY_FORMAT: &str = r#"
{
  name: "video_stream",
  qos_profile: "sensor_data",
  message_format: {}
}
"#;

const EMITTED_TOPIC_EXAMPLE2: &str = r#"
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

const EMITTED_TOPIC_KEYWORD_FIELDS_EXAMPLE: &str = r#"
{
  name: "keyword_topic",
  qos_profile: "standard",
  message_format: {
    "type": "u32",
    "match": "string"
  }
}
"#;

const EMITTED_TOPIC_RESERVED_FIELD_EXAMPLE: &str = r#"
{
  name: "robot_state",
  qos_profile: "standard",
  message_format: {
    instance_id: "string",
    status: "u8"
  }
}
"#;

const EMITTED_TOPIC_FIXED_STRING_ARRAY_EXAMPLE: &str = r#"
{
  name: "labels",
  qos_profile: "standard",
  message_format: {
    labels: {
      $type: "array",
      $items: "string",
      $length: 3
    }
  }
}
"#;

const SUBSCRIBED_TOPIC_EXAMPLE1: &str = r#"
{
    local_node_id: "uvc_camera",
    name: "video_stream",
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
    frame: {
        $type: "array",
        $items: "u8"
    }
}
"#;

const SUBSCRIBED_TOPIC_EXAMPLE2: &str = r#"
{
    local_node_id: "uvc_camera",
    name: "sound",
}
"#;

const SUBSCRIBED_TOPIC_EXAMPLE_KEYWORDS: &str = r#"
{
    local_node_id: "keyword_source",
    name: "keyword_topic",
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

const SUBSCRIBED_TOPIC_FORMAT_EXAMPLE_KEYWORDS: &str = r#"
{
    "type": "u32",
    "match": "string"
}
"#;

fn parse_emitted_topic(example: &str) -> EmittedTopic {
    serde_json5::from_str(example).unwrap()
}

fn parse_consumed_topic(example: &str) -> ConsumedTopic {
    serde_json5::from_str(example).unwrap()
}

fn parse_message_format(example: &str) -> MessageFormat {
    serde_json5::from_str(example).unwrap()
}

/// In the case of a topic, an "emitted" topic is an entity that emits messages
#[test]
fn emit_topic() {
    let topic = parse_emitted_topic(EMITTED_TOPIC_EXAMPLE);

    let mut generator = RustGenerator::new();
    generator.add_emitted_topic(&topic).unwrap();
    let artifacts = render_artifacts(generator.into_artifacts());
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
            "crate::capnp::video_stream_message_capnp::video_stream_message::Builder",
        ],
    );

    // Field handling - test different field types
    assert_contains_all(
        &rendered,
        &[
            "root.set_encoding(encoding.as_str());",
            "root.set_frame(frame.as_ref());",
            "root.reborrow().init_header();",
            "peppylib::encoding::convert_time",
        ],
    );

    // Generated structs and function signature
    assert_contains_all(
        &rendered,
        &[
            "pub struct MessageHeader",
            "frame: Vec<u8>",
            "pub async fn emit(",
        ],
    );

    // Topic metadata
    assert_contains_all(
        &rendered,
        &[
            "let as_topic = \"video_stream\";",
            "let qos = peppylib::config::QoSProfile::SensorData;",
            "peppylib::TopicMessenger::emit(",
        ],
    );
}

#[test]
fn emit_two_topics() {
    let topic1 = parse_emitted_topic(EMITTED_TOPIC_EXAMPLE);
    let topic2 = parse_emitted_topic(EMITTED_TOPIC_EXAMPLE2);

    let mut generator = RustGenerator::new();
    generator.add_emitted_topic(&topic1).unwrap();
    generator.add_emitted_topic(&topic2).unwrap();
    let artifacts = render_artifacts(generator.into_artifacts());
    assert_eq!(
        artifacts.len(),
        2,
        "expected two generated artifacts, got {}",
        artifacts.len()
    );

    // Verify each topic gets its own distinct artifact with correct schema
    assert_artifact_contains(&artifacts, "video_stream_message_capnp");
    assert_artifact_contains(&artifacts, "push_lidar_object_message_capnp");
}

#[test]
fn emit_topic_escapes_rust_keyword_fields() {
    let topic = parse_emitted_topic(EMITTED_TOPIC_KEYWORD_FIELDS_EXAMPLE);

    let mut generator = RustGenerator::new();
    generator.add_emitted_topic(&topic).unwrap();
    let rendered = render_artifacts(generator.into_artifacts())
        .into_iter()
        .next()
        .expect("artifact is present");

    assert_contains_all(
        &rendered,
        &[
            "pub async fn emit(",
            "type_: u32",
            "match_: String",
            "root.set_type(type_);",
            "root.set_match(match_.as_str());",
        ],
    );
}

#[test]
fn emit_topic_rejects_reserved_message_field_name() {
    use crate::error::Error;

    let topic = parse_emitted_topic(EMITTED_TOPIC_RESERVED_FIELD_EXAMPLE);
    let mut generator = RustGenerator::new();

    let err = generator.add_emitted_topic(&topic).unwrap_err();

    match err {
        Error::UnauthorizedMessageFieldName {
            field,
            path,
            context,
        } => {
            assert_eq!(field, "instance_id");
            assert_eq!(path, "instance_id");
            assert_eq!(context, "message_format");
        }
        other => panic!("expected UnauthorizedMessageFieldName, got: {other:?}"),
    }
}

#[test]
fn emit_topic_rejects_fixed_string_array() {
    use crate::error::Error;

    let topic = parse_emitted_topic(EMITTED_TOPIC_FIXED_STRING_ARRAY_EXAMPLE);
    let mut generator = RustGenerator::new();

    let err = generator.add_emitted_topic(&topic).unwrap_err();

    match err {
        Error::UnsupportedFixedArrayItemType {
            language,
            field,
            item,
        } => {
            assert_eq!(language, PeppygenLanguage::Rust);
            assert_eq!(field, "labels");
            assert_eq!(item, "string");
        }
        other => panic!("expected UnsupportedFixedArrayItemType, got: {other:?}"),
    }
}

/// In the case of a topic, a "subscribed" topic is an entity expects to receive messages from another entity
#[test]
fn consumed_topic() {
    let topic = parse_consumed_topic(SUBSCRIBED_TOPIC_EXAMPLE1);
    let format = parse_message_format(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE1);

    let mut generator = RustGenerator::new();
    generator
        .add_consumed_topic(&topic, format, "uvc_camera")
        .unwrap();
    let artifacts = render_artifacts(generator.into_artifacts());
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
            "pub frame: Vec<u8>",
        ],
    );

    // Subscriber function signature
    assert_contains_all(
        &rendered,
        &[
            "pub async fn on_next_message_received(",
            "core_node_target: Option<&str>",
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
            "crate::Error::SubscriptionClosed",
        ],
    );
}

#[test]
fn consumed_topic_escapes_rust_keyword_fields() {
    let topic = parse_consumed_topic(SUBSCRIBED_TOPIC_EXAMPLE_KEYWORDS);
    let format = parse_message_format(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE_KEYWORDS);

    let mut generator = RustGenerator::new();
    generator
        .add_consumed_topic(&topic, format, "keyword_source")
        .unwrap();
    let rendered = render_artifacts(generator.into_artifacts())
        .into_iter()
        .next()
        .expect("artifact is present");

    assert_contains_all(
        &rendered,
        &[
            "pub struct Message",
            "pub type_: u32",
            "pub match_: String",
            ".get_type()",
            ".get_match()",
        ],
    );
}

#[test]
fn consumed_two_topics_same_node() {
    let video_topic = parse_consumed_topic(SUBSCRIBED_TOPIC_EXAMPLE1);
    let video_format = parse_message_format(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE1);

    let sound_topic = parse_consumed_topic(SUBSCRIBED_TOPIC_EXAMPLE2);
    let sound_format = parse_message_format(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE2);

    let mut generator = RustGenerator::new();
    generator
        .add_consumed_topic(&video_topic, video_format, "uvc_camera")
        .unwrap();
    generator
        .add_consumed_topic(&sound_topic, sound_format, "uvc_camera")
        .unwrap();
    let artifacts = render_artifacts(generator.into_artifacts());
    assert_eq!(
        artifacts.len(),
        2,
        "expected two generated artifacts, got {}",
        artifacts.len()
    );

    // Verify each topic gets distinct artifact with correct topic name
    assert_artifact_contains(&artifacts, "let topic_name = \"video_stream\";");
    assert_artifact_contains(&artifacts, "let topic_name = \"sound\";");

    // Verify both reference the same source node
    for rendered in &artifacts {
        assert_contains_all(rendered, &["let node_name = \"uvc_camera\";"]);
    }
}

#[test]
fn external_consumed_topic() {
    let format = parse_message_format(
        r#"
        {
            linear_x: "f64",
            angular_z: "f64",
        }
        "#,
    );

    let mut generator = RustGenerator::new();
    generator
        .add_external_consumed_topic("cmd_vel", format)
        .unwrap();
    let artifacts = render_artifacts(generator.into_artifacts());
    assert_eq!(
        artifacts.len(),
        1,
        "expected a single generated artifact, got {}",
        artifacts.len()
    );
    let rendered = artifacts.into_iter().next().expect("artifact is present");

    // Generated struct
    assert_contains_all(
        &rendered,
        &[
            "pub struct Message",
            "pub linear_x: f64",
            "pub angular_z: f64",
        ],
    );

    // Subscriber function signature
    assert_contains_all(
        &rendered,
        &[
            "pub async fn on_next_message_received(",
            "core_node_target: Option<&str>",
            "instance_id_target: Option<&str>",
            "-> crate::Result<(String, Message)>",
        ],
    );

    // External subscribe: uses consume_external, no node_name
    assert_contains_all(
        &rendered,
        &[
            "let topic_name = \"cmd_vel\";",
            "peppylib::TopicMessenger::consume_external(",
        ],
    );
    assert!(
        !rendered.contains("let node_name"),
        "external topic should not have a node_name variable"
    );

    // Deserialization
    assert_contains_all(
        &rendered,
        &["fn deseralize_payload(", "capnp::serialize::read_message"],
    );

    // Error variants
    assert_contains_all(
        &rendered,
        &[
            "crate::Error::TopicSubscribe",
            "crate::Error::SubscriptionClosed",
        ],
    );
}

/// Checks for clippy warnings when there is only one emitted topic with an empty message format.
#[test]
fn clippy_single_emitted_topic_empty_format() {
    let temp_dir = TempDir::new().unwrap();
    let emitted_topic = parse_emitted_topic(EMITTED_TOPIC_EXAMPLE_EMPTY_FORMAT);

    let consumed_action1: ConsumedAction = serde_json5::from_str(
        r#"
        {
          local_node_id: "brain",
          name: "move_arm",
        }
        "#,
    )
    .unwrap();
    let consumed_action2: ConsumedAction = serde_json5::from_str(
        r#"
        {
          local_node_id: "controller",
          name: "rotate_servo_clockwise",
        }
        "#,
    )
    .unwrap();
    let goal_response_format: MessageFormat =
        serde_json5::from_str(r#"{ accepted: "bool" }"#).unwrap();
    let action_messages = ConsumedActionMessage {
        goal_request: None,
        goal_response: Some(goal_response_format),
        feedback: None,
        result_request: None,
        result_response: None,
    };

    let (mut generator, output_dir, user_node, _) = init_test_env::<RustGenerator>(&temp_dir);
    generator.add_emitted_topic(&emitted_topic).unwrap();
    generator
        .add_consumed_action(&consumed_action1, &action_messages, "brain")
        .unwrap();
    generator
        .add_consumed_action(&consumed_action2, &action_messages, "controller")
        .unwrap();
    let output_config = copy_config_to_output(&user_node, &output_dir);
    generator
        .build(
            &output_dir,
            &config::consts::PeppyDirs::default(),
            Default::default(),
        )
        .unwrap();
    fs::remove_file(output_config).unwrap();

    run_clippy(&output_dir);

    let emitted_topics_contents = std::fs::read_to_string(output_dir.join("src/emitted_topics.rs"))
        .expect("failed to read emitted_topics module");
    assert_contains_all(&emitted_topics_contents, &["pub mod video_stream;"]);

    let consumed_actions_contents =
        std::fs::read_to_string(output_dir.join("src/consumed_actions.rs"))
            .expect("failed to read consumed_actions module");
    assert_contains_all(
        &consumed_actions_contents,
        &[
            "pub mod brain_move_arm;",
            "pub mod controller_rotate_servo_clockwise;",
        ],
    );
}

/// This is a long running test that verifies the generated code compiles and passes clippy
#[test]
fn compile_lib_with_emitted_and_consumed_topics() {
    let temp_dir = TempDir::new().unwrap();
    let emitted_topic1 = parse_emitted_topic(EMITTED_TOPIC_EXAMPLE);
    let emitted_topic2 = parse_emitted_topic(EMITTED_TOPIC_EXAMPLE2);
    let consumed_topic1 = parse_consumed_topic(SUBSCRIBED_TOPIC_EXAMPLE1);
    let subscribed_format1 = parse_message_format(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE1);
    let consumed_topic2 = parse_consumed_topic(SUBSCRIBED_TOPIC_EXAMPLE2);
    let subscribed_format2 = parse_message_format(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE2);

    let (mut generator, output_dir, user_node, _) = init_test_env::<RustGenerator>(&temp_dir);
    generator.add_emitted_topic(&emitted_topic1).unwrap();
    generator.add_emitted_topic(&emitted_topic2).unwrap();
    generator
        .add_consumed_topic(&consumed_topic1, subscribed_format1, "uvc_camera")
        .unwrap();
    generator
        .add_consumed_topic(&consumed_topic2, subscribed_format2, "uvc_camera")
        .unwrap();
    let output_config = copy_config_to_output(&user_node, &output_dir);
    generator
        .build(
            &output_dir,
            &config::consts::PeppyDirs::default(),
            Default::default(),
        )
        .unwrap();
    fs::remove_file(output_config).unwrap();

    run_cargo_build(&output_dir);
    run_clippy(&output_dir);

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
        &["pub mod emitted_topics;", "pub mod consumed_topics;"],
    );

    // Verify expected module files exist
    assert!(
        output_dir
            .join("src/emitted_topics/video_stream.rs")
            .exists(),
        "Expected video_stream module"
    );
    assert!(
        output_dir
            .join("src/emitted_topics/push_lidar_object.rs")
            .exists(),
        "Expected push_lidar_object module"
    );
    assert!(
        output_dir
            .join("src/consumed_topics/uvc_camera_video_stream.rs")
            .exists(),
        "Expected uvc_camera_video_stream subscriber module"
    );
    assert!(
        output_dir
            .join("src/consumed_topics/uvc_camera_sound.rs")
            .exists(),
        "Expected uvc_camera_sound subscriber module"
    );
}
