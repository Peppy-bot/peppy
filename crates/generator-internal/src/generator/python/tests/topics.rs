use super::*;
use config::node::{ExposedTopic, MessageFormat, SubscribedTopic};

const EXPOSED_TOPIC_EXAMPLE: &str = r#"
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

const EXPOSED_TOPIC_EXAMPLE2: &str = r#"
{
  name: "push_lidar_object",
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
    return_type: "u8",
    classification: "u8",
  },
}
"#;

const SUBSCRIBED_TOPIC_EXAMPLE1: &str = r#"
{
    id: "video_stream",
    node: "uvc_camera",
    name: "video_stream",
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
    frame: {
        $type: "array",
        $items: "u8"
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
  encoding: "string",
  sample_rate: "u32",
  channels: "u32",
  layout: "string",
  frame_count: "u32",
  samples: {
    $type: "array",
    $items: "u8",
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

/// In the case of a topic, an "exposed" topic is an entity that emits messages.
#[test]
fn expose_topic() {
    let topic = parse_exposed_topic(EXPOSED_TOPIC_EXAMPLE);

    let mut generator = PythonGenerator::new();
    generator.add_exposed_topic(&topic).unwrap();
    let artifacts = render_artifacts(generator);
    assert_eq!(
        artifacts.len(),
        1,
        "expected a single generated artifact, got {}",
        artifacts.len()
    );
    let rendered = artifacts.into_iter().next().expect("artifact is present");

    // Imports
    assert_contains_all(
        &rendered,
        &["import peppylib", "from dataclasses import dataclass"],
    );

    // Python dataclass for nested header type
    assert_contains_all(
        &rendered,
        &[
            "@dataclass",
            "class MessageHeader:",
            "stamp: float",
            "frame_id: int",
        ],
    );

    // emit function signature with typed parameters
    assert_contains_all(
        &rendered,
        &[
            "async def emit(",
            "node_runner: peppylib.NodeRunner",
            "header: MessageHeader",
            "encoding: str",
            "width: int",
            "height: int",
            "frame: bytes",
        ],
    );

    // Topic metadata and messenger call
    assert_contains_all(
        &rendered,
        &[
            "\"video_stream\"",
            "peppylib.QoSProfile.SensorData",
            "peppylib.TopicMessenger.emit(",
        ],
    );
}

#[test]
fn expose_two_topics() {
    let topic1 = parse_exposed_topic(EXPOSED_TOPIC_EXAMPLE);
    let topic2 = parse_exposed_topic(EXPOSED_TOPIC_EXAMPLE2);

    let mut generator = PythonGenerator::new();
    generator.add_exposed_topic(&topic1).unwrap();
    generator.add_exposed_topic(&topic2).unwrap();
    let artifacts = render_artifacts(generator);
    assert_eq!(
        artifacts.len(),
        2,
        "expected two generated artifacts, got {}",
        artifacts.len()
    );

    // Verify each topic gets its own distinct artifact
    assert_artifact_contains(&artifacts, "\"video_stream\"");
    assert_artifact_contains(&artifacts, "\"push_lidar_object\"");
}

/// In the case of a topic, a "subscribed" topic is an entity that expects to receive messages
/// from another entity.
#[test]
fn subscribed_to_topic() {
    let topic = parse_subscribed_topic(SUBSCRIBED_TOPIC_EXAMPLE1);
    let format = parse_message_format(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE1);

    let mut generator = PythonGenerator::new();
    generator.add_subscribed_topic(&topic, format).unwrap();
    let artifacts = render_artifacts(generator);
    assert_eq!(
        artifacts.len(),
        1,
        "expected a single generated artifact, got {}",
        artifacts.len()
    );
    let rendered = artifacts.into_iter().next().expect("artifact is present");

    // Imports
    assert_contains_all(
        &rendered,
        &["import peppylib", "from dataclasses import dataclass"],
    );

    // Generated dataclasses with various field types
    assert_contains_all(
        &rendered,
        &[
            "@dataclass",
            "class Message:",
            "class MessageHeader:",
            "stamp: float",
            "frame: bytes",
        ],
    );

    // Subscriber function signature
    assert_contains_all(
        &rendered,
        &[
            "async def on_next_message_received(",
            "node_runner: peppylib.NodeRunner",
        ],
    );

    // Topic metadata
    assert_contains_all(
        &rendered,
        &[
            "\"uvc_camera\"",
            "\"video_stream\"",
            "peppylib.TopicMessenger.subscribe(",
        ],
    );
}

#[test]
fn subscribed_to_two_topics_same_node() {
    let video_topic = parse_subscribed_topic(SUBSCRIBED_TOPIC_EXAMPLE1);
    let video_format = parse_message_format(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE1);

    let sound_topic = parse_subscribed_topic(SUBSCRIBED_TOPIC_EXAMPLE2);
    let sound_format = parse_message_format(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE2);

    let mut generator = PythonGenerator::new();
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
    assert_artifact_contains(&artifacts, "\"video_stream\"");
    assert_artifact_contains(&artifacts, "\"sound\"");

    // Verify both reference the same source node
    for rendered in &artifacts {
        assert_contains_all(rendered, &["\"uvc_camera\""]);
    }
}
