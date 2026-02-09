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
    let artifacts = render_artifacts(generator.into_artifacts());
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
        &[
            "import capnp",
            "import peppylib",
            "import types",
            "from dataclasses import dataclass",
            "from pathlib import Path",
        ],
    );

    // Module directory and schema loading as module-level typed constant
    assert_contains_all(
        &rendered,
        &[
            "_PKG_DIR = Path(__file__).resolve().parent.parent",
            "VIDEO_STREAM_MESSAGE_CAPNP: types.ModuleType = capnp.load(str(_PKG_DIR / \"capnp/video_stream_message.capnp\"))",
        ],
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

    // Cap'n Proto serialization via pycapnp
    assert_contains_all(
        &rendered,
        &[
            "VIDEO_STREAM_MESSAGE_CAPNP.VideoStreamMessage.new_message()",
            ".init(\"header\")",
            "peppylib.encoding.convert_time(header.stamp)",
            ".init(\"stamp\")",
            ".sec = timestamp_",
            ".nsec = timestamp_",
            ".frameId = header.frame_id",
            "capnp_msg.encoding = encoding",
            "capnp_msg.width = width",
            "capnp_msg.height = height",
            "capnp_msg.frame = frame",
            "payload = capnp_msg.to_bytes()",
        ],
    );

    // Topic metadata and messenger call
    assert_contains_all(
        &rendered,
        &[
            "TOPIC_NAME = \"video_stream\"",
            "peppylib.QoSProfile.SensorData",
            "peppylib.TopicMessenger.emit(",
            "TOPIC_NAME,",
            "payload,",
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
    let artifacts = render_artifacts(generator.into_artifacts());
    assert_eq!(
        artifacts.len(),
        2,
        "expected two generated artifacts, got {}",
        artifacts.len()
    );
    // Verify each topic gets its own distinct artifact
    assert_artifact_contains(&artifacts, "\"video_stream\"");
    assert_artifact_contains(&artifacts, "\"push_lidar_object\"");

    // Verify each artifact is self-contained: has its own schema and doesn't leak the other topic
    let video_rendered = artifacts
        .iter()
        .find(|a| a.contains("\"video_stream\""))
        .expect("video_stream artifact is present");
    let lidar_rendered = artifacts
        .iter()
        .find(|a| a.contains("\"push_lidar_object\""))
        .expect("push_lidar_object artifact is present");

    assert_contains_all(
        video_rendered,
        &[
            "video_stream_message.capnp\")",
            "TOPIC_NAME = \"video_stream\"",
        ],
    );
    assert!(
        !video_rendered.contains("push_lidar_object"),
        "video_stream artifact should not reference push_lidar_object"
    );

    assert_contains_all(
        lidar_rendered,
        &[
            "push_lidar_object_message.capnp\")",
            "TOPIC_NAME = \"push_lidar_object\"",
        ],
    );
    assert!(
        !lidar_rendered.contains("video_stream"),
        "push_lidar_object artifact should not reference video_stream"
    );
}

/// In the case of a topic, a "subscribed" topic is an entity that expects to receive messages
/// from another entity.
#[test]
fn subscribed_to_topic() {
    let topic = parse_subscribed_topic(SUBSCRIBED_TOPIC_EXAMPLE1);
    let format = parse_message_format(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE1);

    let mut generator = PythonGenerator::new();
    generator.add_subscribed_topic(&topic, format).unwrap();
    let artifacts = render_artifacts(generator.into_artifacts());
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
        &[
            "import capnp",
            "import peppylib",
            "import types",
            "from dataclasses import dataclass",
            "from pathlib import Path",
            "from typing import Optional, Tuple",
        ],
    );

    // Module directory and schema loading as module-level typed constant
    assert_contains_all(
        &rendered,
        &[
            "_PKG_DIR = Path(__file__).resolve().parent.parent",
            "VIDEO_STREAM_MESSAGE_CAPNP: types.ModuleType = capnp.load(str(_PKG_DIR / \"capnp/video_stream_message.capnp\"))",
        ],
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

    // _deserialize_payload helper function
    assert_contains_all(
        &rendered,
        &[
            "def _deserialize_payload(payload):",
            "with VIDEO_STREAM_MESSAGE_CAPNP.VideoStreamMessage.from_bytes(payload) as capnp_msg:",
        ],
    );

    // Cap'n Proto deserialization: nested object reader, time conversion, primitive reads, bytes
    assert_contains_all(
        &rendered,
        &[
            "capnp_msg.header",
            "peppylib.encoding.convert_time_from_capnp(",
            ".frameId",
            "MessageHeader(stamp=",
            "capnp_msg.encoding",
            "capnp_msg.width",
            "capnp_msg.height",
            "bytes(capnp_msg.frame)",
            "return Message(",
        ],
    );

    // Subscriber function signature with optional targeting parameters and return type
    assert_contains_all(
        &rendered,
        &[
            "async def on_next_message_received(",
            "node_runner: peppylib.NodeRunner",
            "master_node_target: Optional[str] = None",
            "instance_id_target: Optional[str] = None",
            ") -> Tuple[str, Message]:",
        ],
    );

    // Topic metadata and subscribe call passes targeting parameters
    assert_contains_all(
        &rendered,
        &[
            "\"uvc_camera\"",
            "\"video_stream\"",
            "peppylib.TopicMessenger.subscribe(",
            "master_node_target,",
            "instance_id_target,",
        ],
    );

    // on_next_message_received body: receive, deserialize, return
    assert_contains_all(
        &rendered,
        &[
            "raw_message = await subscription.on_next_message()",
            "payload = raw_message.payload",
            "instance_id = raw_message.instance_id",
            "message = _deserialize_payload(payload)",
            "return instance_id, message",
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
    let artifacts = render_artifacts(generator.into_artifacts());
    assert_eq!(
        artifacts.len(),
        2,
        "expected two generated artifacts, got {}",
        artifacts.len()
    );

    // Verify each topic gets its own distinct artifact
    assert_artifact_contains(&artifacts, "\"video_stream\"");
    assert_artifact_contains(&artifacts, "\"sound\"");

    // Verify each artifact is self-contained: has its own schema and doesn't leak the other topic
    let video_rendered = artifacts
        .iter()
        .find(|a| a.contains("\"video_stream\""))
        .expect("video_stream artifact is present");
    let sound_rendered = artifacts
        .iter()
        .find(|a| a.contains("\"sound\""))
        .expect("sound artifact is present");

    assert_contains_all(
        video_rendered,
        &[
            "video_stream_message.capnp\")",
            "topic_name = \"video_stream\"",
            "node_name = \"uvc_camera\"",
        ],
    );
    assert!(
        !video_rendered.contains("\"sound\""),
        "video_stream artifact should not reference sound"
    );

    assert_contains_all(
        sound_rendered,
        &[
            "sound_message.capnp\")",
            "topic_name = \"sound\"",
            "node_name = \"uvc_camera\"",
        ],
    );
    assert!(
        !sound_rendered.contains("video_stream"),
        "sound artifact should not reference video_stream"
    );
}
