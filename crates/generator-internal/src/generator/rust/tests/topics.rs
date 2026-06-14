use super::*;
use crate::error::Error;
use config::node::{ConsumedAction, ConsumedService, ConsumedTopic, EmittedTopic, MessageFormat};

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

const SUBSCRIBED_TOPIC_EXAMPLE1: &str = r#"
{
    link_id: "uvc_camera",
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
    link_id: "uvc_camera",
    name: "sound",
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
    generator.add_emitted_topic(&topic, None).unwrap();
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
    generator.add_emitted_topic(&topic1, None).unwrap();
    generator.add_emitted_topic(&topic2, None).unwrap();
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
    let emitted_topic_keyword_fields_example: &str = r#"
    {
      name: "keyword_topic",
      qos_profile: "standard",
      message_format: {
        "type": "u32",
        "match": "string"
      }
    }
    "#;
    let topic = parse_emitted_topic(emitted_topic_keyword_fields_example);

    let mut generator = RustGenerator::new();
    generator.add_emitted_topic(&topic, None).unwrap();
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
    let emitted_topic_reserved_field_example: &str = r#"
    {
      name: "robot_state",
      qos_profile: "standard",
      message_format: {
        instance_id: "string",
        status: "u8"
      }
    }
    "#;
    let topic = parse_emitted_topic(emitted_topic_reserved_field_example);
    let mut generator = RustGenerator::new();

    let err = generator.add_emitted_topic(&topic, None).unwrap_err();

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
    let emitted_topic_fixed_string_array_example: &str = r#"
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
    let topic = parse_emitted_topic(emitted_topic_fixed_string_array_example);
    let mut generator = RustGenerator::new();

    let err = generator.add_emitted_topic(&topic, None).unwrap_err();

    match err {
        Error::UnsupportedFixedArrayItemType { field, item } => {
            assert_eq!(field, "labels");
            assert_eq!(item, "string");
        }
        other => panic!("expected UnsupportedFixedArrayItemType, got: {other:?}"),
    }
}

#[test]
fn emit_topic_rejects_fixed_object_array() {
    let emitted_topic_fixed_object_array_example: &str = r#"
    {
      name: "detections",
      qos_profile: "sensor_data",
      message_format: {
        objects: {
          $type: "array",
          $items: {
            $type: "object",
              x: "f32",
              y: "f32"
          },
          $length: 4
        }
      }
    }
    "#;

    let topic = parse_emitted_topic(emitted_topic_fixed_object_array_example);
    let mut generator = RustGenerator::new();

    let err = generator.add_emitted_topic(&topic, None).unwrap_err();

    match err {
        Error::UnsupportedFixedArrayItemType { field, item } => {
            assert_eq!(field, "objects");
            assert_eq!(item, "object");
        }
        other => panic!("expected UnsupportedFixedArrayItemType, got: {other:?}"),
    }
}

#[test]
fn emit_topic_with_dynamic_object_array() {
    let emitted_topic_dynamic_object_array_example: &str = r#"
    {
      name: "detections",
      qos_profile: "sensor_data",
      message_format: {
        objects: {
          $type: "array",
          $items: {
            $type: "object",
              x: "f32",
              y: "f32",
              label: "string"
          }
        }
      }
    }
    "#;
    let topic = parse_emitted_topic(emitted_topic_dynamic_object_array_example);

    let mut generator = RustGenerator::new();
    generator.add_emitted_topic(&topic, None).unwrap();
    let artifacts = render_artifacts(generator.into_artifacts());
    assert_eq!(
        artifacts.len(),
        1,
        "expected a single generated artifact, got {}",
        artifacts.len()
    );
    let rendered = artifacts.into_iter().next().expect("artifact is present");

    // Object array serialization: list init and element access
    assert_contains_all(&rendered, &["init_objects(", ".reborrow().get(", ".len()"]);

    // Dynamic-length path must not emit a fixed-length guard
    assert!(
        !rendered.contains("assert_eq"),
        "dynamic object array must not emit a length check"
    );
}

/// A real manifest dep (link_id present) splices the runtime
/// consumer-filter lookup into the generated subscribe call: the
/// resolved filter carries the bound producer's full
/// `(core_node, instance_id)`, so a pinned topic slot sets both wire
/// slots and can never receive from a same-instance_id producer on
/// another core node.
#[test]
fn consumed_topic_with_link_id_splices_runtime_consumer_filter() {
    let topic = parse_consumed_topic(SUBSCRIBED_TOPIC_EXAMPLE1);
    let format = parse_message_format(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE1);

    let mut generator = RustGenerator::new();
    generator
        .add_consumed_topic(
            &topic,
            format,
            &crate::DependencyContext::native("uvc_camera", "v1")
                .with_link_id(crate::WireLinkId::from_link_id("cam_left", false)),
        )
        .unwrap();
    let artifacts = render_artifacts(generator.into_artifacts());
    let rendered = artifacts.into_iter().next().expect("artifact is present");

    assert_contains_all(&rendered, &[".consumer_filter(\"cam_left\")"]);
    assert_rendered!(
        !rendered.contains("ConsumerFilter::Any"),
        rendered,
        "a linked dep must resolve its filter from the bindings map, not emit a wildcard",
    );
}

/// In the case of a topic, a "subscribed" topic is an entity expects to receive messages from another entity
#[test]
fn consumed_topic() {
    let topic = parse_consumed_topic(SUBSCRIBED_TOPIC_EXAMPLE1);
    let format = parse_message_format(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE1);

    let mut generator = RustGenerator::new();
    generator
        .add_consumed_topic(
            &topic,
            format,
            &crate::DependencyContext::native("uvc_camera", "v1"),
        )
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

    // Subscriber function signature — the producer identity is returned with
    // every message as the full `peppylib::messaging::ProducerRef`; it never
    // appears as a user-facing core_node (or instance_id) parameter.
    assert_contains_all(
        &rendered,
        &[
            "pub async fn on_next_message_received(",
            "node_runner: &crate::NodeRunner",
            "-> crate::Result<(peppylib::messaging::ProducerRef, Message)>",
            "peppylib::messaging::ProducerRef::new(",
            "Ok((producer, message))",
        ],
    );
    assert!(
        !rendered.contains("from_core_node"),
        "from_core_node should no longer appear in the generated API; got: {rendered}"
    );

    // Deserialization
    assert_contains_all(
        &rendered,
        &["fn deseralize_payload(", "capnp::serialize::read_message"],
    );

    // Topic metadata. The fixture's `DependencyContext::native` defaults to
    // `WireLinkId::wildcard()` (no manifest link_id), so the subscribe call
    // splices a wildcard `&ConsumerFilter::Any` at the consumer-filter slot.
    assert_contains_all(
        &rendered,
        &[
            "let node_name = \"uvc_camera\";",
            "peppylib::TopicMessenger::subscribe(",
            "&peppylib::messaging::ConsumerFilter::Any,",
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
    let subscribed_topic_example_keywords: &str = r#"
    {
        link_id: "keyword_source",
        name: "keyword_topic",
    }
    "#;
    let topic = parse_consumed_topic(subscribed_topic_example_keywords);
    let subscribed_topic_format_example_keywords: &str = r#"
    {
        "type": "u32",
        "match": "string"
    }
    "#;
    let format = parse_message_format(subscribed_topic_format_example_keywords);

    let mut generator = RustGenerator::new();
    generator
        .add_consumed_topic(
            &topic,
            format,
            &crate::DependencyContext::native("keyword_source", "v1"),
        )
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
        .add_consumed_topic(
            &video_topic,
            video_format,
            &crate::DependencyContext::native("uvc_camera", "v1"),
        )
        .unwrap();
    generator
        .add_consumed_topic(
            &sound_topic,
            sound_format,
            &crate::DependencyContext::native("uvc_camera", "v1"),
        )
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

/// Checks for clippy warnings when there is only one emitted topic with an empty message format.
#[test]
fn clippy_single_emitted_topic_empty_format() {
    let temp_dir = TempDir::new().unwrap();
    let emitted_topic_example_empty_format: &str = r#"
    {
      name: "video_stream",
      qos_profile: "sensor_data",
      message_format: {}
    }
    "#;
    let emitted_topic = parse_emitted_topic(emitted_topic_example_empty_format);

    let consumed_action1: ConsumedAction = serde_json5::from_str(
        r#"
        {
          link_id: "brain",
          name: "move_arm",
        }
        "#,
    )
    .unwrap();
    let consumed_action2: ConsumedAction = serde_json5::from_str(
        r#"
        {
          link_id: "controller",
          name: "rotate_servo_clockwise",
        }
        "#,
    )
    .unwrap();
    let action_messages = ConsumedActionMessage {
        goal_request: None,
        feedback: None,
        result_response: None,
    };

    let (mut generator, output_dir, user_node, _) = init_test_env::<RustGenerator>(&temp_dir);
    generator.add_emitted_topic(&emitted_topic, None).unwrap();
    generator
        .add_consumed_action(
            &consumed_action1,
            &action_messages,
            &crate::DependencyContext::native("brain", "v1"),
        )
        .unwrap();
    generator
        .add_consumed_action(
            &consumed_action2,
            &action_messages,
            &crate::DependencyContext::native("controller", "v1"),
        )
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
    generator.add_emitted_topic(&emitted_topic1, None).unwrap();
    generator.add_emitted_topic(&emitted_topic2, None).unwrap();
    generator
        .add_consumed_topic(
            &consumed_topic1,
            subscribed_format1,
            &crate::DependencyContext::native("uvc_camera", "v1"),
        )
        .unwrap();
    generator
        .add_consumed_topic(
            &consumed_topic2,
            subscribed_format2,
            &crate::DependencyContext::native("uvc_camera", "v1"),
        )
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

/// Regression guard: generated consumer entry points (topic subscribers,
/// service pollers, action callers) expose no user-facing producer-identity
/// parameters. Producer identity travels only as the full
/// `(core_node, instance_id)` resolved at runtime from the bindings map.
/// This test fails loudly if any generator drifts back to exposing a
/// `from_*` / `target_*` core_node or instance_id parameter.
#[test]
fn no_user_facing_producer_identity_params() {
    let topic = parse_consumed_topic(SUBSCRIBED_TOPIC_EXAMPLE1);
    let topic_format = parse_message_format(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE1);

    let service: ConsumedService =
        serde_json5::from_str(super::services::SUBSCRIBED_SERVICE_EXAMPLE1).unwrap();
    let request_format: MessageFormat =
        serde_json5::from_str(super::services::SUBSCRIBED_SERVICE_REQUEST_EXAMPLE1).unwrap();
    let response_format: MessageFormat =
        serde_json5::from_str(super::services::SUBSCRIBED_SERVICE_RESPONSE_EXAMPLE1).unwrap();

    let action: ConsumedAction =
        serde_json5::from_str(super::actions::SUBSCRIBED_ACTION_EXAMPLE1).unwrap();
    let goal_request_format: MessageFormat =
        serde_json5::from_str(super::actions::SUBSCRIBED_ACTION_GOAL_FORMAT1).unwrap();
    let feedback_format: MessageFormat =
        serde_json5::from_str(super::actions::SUBSCRIBED_ACTION_FEEDBACK_FORMAT1).unwrap();
    let result_response_format: MessageFormat =
        serde_json5::from_str(super::actions::SUBSCRIBED_ACTION_RESULT_RESPONSE_FORMAT1).unwrap();
    let action_messages = ConsumedActionMessage {
        goal_request: Some(goal_request_format),
        feedback: Some(feedback_format),
        result_response: Some(result_response_format),
    };

    let mut generator = RustGenerator::new();
    generator
        .add_consumed_topic(
            &topic,
            topic_format,
            &crate::DependencyContext::native("uvc_camera", "v1"),
        )
        .unwrap();
    generator
        .add_consumed_service(
            &service,
            &request_format,
            &response_format,
            &crate::DependencyContext::native("uvc_camera", "v1"),
        )
        .unwrap();
    generator
        .add_consumed_action(
            &action,
            &action_messages,
            &crate::DependencyContext::native("brain", "v1"),
        )
        .unwrap();
    let rendered = render_artifacts(generator.into_artifacts()).join("\n");

    // Topic subscriber: producer identity travels only as the full
    // `(core_node, instance_id)` resolved at runtime from the bindings map;
    // there is no user-facing core_node parameter, and `from_instance_id`
    // is no longer a parameter either.
    assert!(
        !rendered.contains("from_core_node"),
        "from_core_node should no longer appear in the generated API; rendered:\n{rendered}"
    );
    assert!(
        !rendered.contains("from_instance_id: Option<&str>"),
        "from_instance_id should no longer appear as a generated parameter; rendered:\n{rendered}"
    );

    // The fixture's `DependencyContext::native` defaults to
    // `WireLinkId::wildcard()` (no manifest link_id), so the consumed
    // service/action call sites splice a typed
    // `Option::<&peppylib::messaging::ProducerRef>::None` at the single
    // target slot and the user-facing `target_instance_id` parameter is
    // gone. `target_core_node` is never exposed in the generated API.
    assert!(
        !rendered.contains("target_core_node"),
        "target_core_node should not appear in the generated API; rendered:\n{rendered}"
    );
    assert!(
        !rendered.contains("target_instance_id: Option<&str>"),
        "target_instance_id should no longer appear as a generated parameter; rendered:\n{rendered}"
    );
}
