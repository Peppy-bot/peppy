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

const EMITTED_TOPIC_EXAMPLE2: &str = r#"
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

const EMITTED_TOPIC_WITH_PYTHON_KEYWORD_FIELDS: &str = r#"
{
  name: "keyword_topic",
  qos_profile: "standard",
  message_format: {
    "class": "u32",
    "from": "string"
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

const EMITTED_TOPIC_FIXED_OBJECT_ARRAY_EXAMPLE: &str = r#"
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

const EMITTED_TOPIC_DYNAMIC_OBJECT_ARRAY_EXAMPLE: &str = r#"
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

const SUBSCRIBED_TOPIC_FORMAT_EXAMPLE_KEYWORDS: &str = r#"
{
    "class": "u32",
    "from": "string"
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

/// In the case of a topic, an "exposed" topic is an entity that emits messages.
#[test]
fn emit_topic() {
    let topic = parse_emitted_topic(EMITTED_TOPIC_EXAMPLE);

    let mut generator = PythonGenerator::new();
    generator.add_emitted_topic(&topic).unwrap();
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
            "from functools import lru_cache",
            "from pathlib import Path",
        ],
    );

    // Module directory and lazy cached schema loader
    assert_contains_all(
        &rendered,
        &[
            "_PKG_DIR = Path(__file__).resolve().parent.parent",
            "@lru_cache(maxsize=1)",
            "def _video_stream_message_capnp() -> types.ModuleType:",
            "return capnp.load(str(_PKG_DIR / \"capnp/video_stream_message.capnp\"))",
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
            "_video_stream_message_capnp().VideoStreamMessage.new_message()",
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
fn emit_two_topics() {
    let topic1 = parse_emitted_topic(EMITTED_TOPIC_EXAMPLE);
    let topic2 = parse_emitted_topic(EMITTED_TOPIC_EXAMPLE2);

    let mut generator = PythonGenerator::new();
    generator.add_emitted_topic(&topic1).unwrap();
    generator.add_emitted_topic(&topic2).unwrap();
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

#[test]
fn emit_topic_escapes_python_keyword_fields() {
    let topic = parse_emitted_topic(EMITTED_TOPIC_WITH_PYTHON_KEYWORD_FIELDS);

    let mut generator = PythonGenerator::new();
    generator.add_emitted_topic(&topic).unwrap();
    let rendered = render_artifacts(generator.into_artifacts())
        .into_iter()
        .next()
        .expect("artifact is present");

    assert_contains_all(
        &rendered,
        &[
            "async def emit(",
            "class_: int",
            "from_: str",
            "setattr(capnp_msg, \"class\", class_)",
            "setattr(capnp_msg, \"from\", from_)",
        ],
    );
}

#[test]
fn emit_topic_rejects_reserved_message_field_name() {
    use crate::error::Error;

    let topic = parse_emitted_topic(EMITTED_TOPIC_RESERVED_FIELD_EXAMPLE);

    let mut generator = PythonGenerator::new();
    let err = generator.add_emitted_topic(&topic).unwrap_err();

    match err {
        Error::UnauthorizedMessageFieldName {
            field,
            path,
            context,
        } => {
            assert_eq!(field, "instance_id");
            assert_eq!(path, "instance_id");
            assert_eq!(context, "robot_state");
        }
        other => panic!("expected UnauthorizedMessageFieldName, got: {other:?}"),
    }
}

#[test]
fn emit_topic_rejects_fixed_string_array() {
    use crate::error::Error;

    let topic = parse_emitted_topic(EMITTED_TOPIC_FIXED_STRING_ARRAY_EXAMPLE);

    let mut generator = PythonGenerator::new();
    let err = generator.add_emitted_topic(&topic).unwrap_err();

    match err {
        Error::UnsupportedFixedArrayItemType {
            language,
            field,
            item,
        } => {
            assert_eq!(language, PeppygenLanguage::Python);
            assert_eq!(field, "labels");
            assert_eq!(item, "string");
        }
        other => panic!("expected UnsupportedFixedArrayItemType, got: {other:?}"),
    }
}

#[test]
fn emit_topic_rejects_fixed_object_array() {
    use crate::error::Error;

    let topic = parse_emitted_topic(EMITTED_TOPIC_FIXED_OBJECT_ARRAY_EXAMPLE);

    let mut generator = PythonGenerator::new();
    let err = generator.add_emitted_topic(&topic).unwrap_err();

    match err {
        Error::UnsupportedFixedArrayItemType {
            language,
            field,
            item,
        } => {
            assert_eq!(language, PeppygenLanguage::Python);
            assert_eq!(field, "objects");
            assert_eq!(item, "object");
        }
        other => panic!("expected UnsupportedFixedArrayItemType, got: {other:?}"),
    }
}

#[test]
fn emit_topic_with_dynamic_object_array() {
    let topic = parse_emitted_topic(EMITTED_TOPIC_DYNAMIC_OBJECT_ARRAY_EXAMPLE);

    let mut generator = PythonGenerator::new();
    generator.add_emitted_topic(&topic).unwrap();
    let artifacts = render_artifacts(generator.into_artifacts());
    assert_eq!(
        artifacts.len(),
        1,
        "expected a single generated artifact, got {}",
        artifacts.len()
    );
    let rendered = artifacts.into_iter().next().expect("artifact is present");

    // Object array serialization: list init with runtime length and element iteration
    assert_contains_all(&rendered, &[".init(\"objects\", len(", "in enumerate("]);

    // Dynamic-length path must not emit a fixed-length guard
    assert!(
        !rendered.contains("raise ValueError"),
        "dynamic object array must not emit a length check"
    );
}

/// In the case of a topic, a "subscribed" topic is an entity that expects to receive messages
/// from another entity.
#[test]
fn consumed_topic() {
    let topic = parse_consumed_topic(SUBSCRIBED_TOPIC_EXAMPLE1);
    let format = parse_message_format(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE1);

    let mut generator = PythonGenerator::new();
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

    // Imports
    assert_contains_all(
        &rendered,
        &[
            "import capnp",
            "import peppylib",
            "import types",
            "from dataclasses import dataclass",
            "from functools import lru_cache",
            "from pathlib import Path",
            "from typing import Optional, Tuple",
        ],
    );

    // Module directory and lazy cached schema loader
    assert_contains_all(
        &rendered,
        &[
            "_PKG_DIR = Path(__file__).resolve().parent.parent",
            "@lru_cache(maxsize=1)",
            "def _video_stream_message_capnp() -> types.ModuleType:",
            "return capnp.load(str(_PKG_DIR / \"capnp/video_stream_message.capnp\"))",
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
            "def _deserialize_payload(payload: bytes) -> Message:",
            "with _video_stream_message_capnp().VideoStreamMessage.from_bytes(payload) as capnp_msg:",
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
            "core_node_target: Optional[str] = None",
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
            "core_node_target,",
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
fn consumed_topic_escapes_python_keyword_fields() {
    let topic = parse_consumed_topic(SUBSCRIBED_TOPIC_EXAMPLE_KEYWORDS);
    let format = parse_message_format(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE_KEYWORDS);

    let mut generator = PythonGenerator::new();
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
            "class_: int",
            "from_: str",
            "getattr(capnp_msg, \"class\")",
            "getattr(capnp_msg, \"from\")",
        ],
    );
}

#[test]
fn consumed_two_topics_same_node() {
    let video_topic = parse_consumed_topic(SUBSCRIBED_TOPIC_EXAMPLE1);
    let video_format = parse_message_format(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE1);

    let sound_topic = parse_consumed_topic(SUBSCRIBED_TOPIC_EXAMPLE2);
    let sound_format = parse_message_format(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE2);

    let mut generator = PythonGenerator::new();
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

    let mut generator = PythonGenerator::new();
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

    // Imports
    assert_contains_all(
        &rendered,
        &[
            "import capnp",
            "import peppylib",
            "from dataclasses import dataclass",
            "from typing import Optional, Tuple",
        ],
    );

    // Generated dataclass
    assert_contains_all(
        &rendered,
        &[
            "@dataclass",
            "class Message:",
            "linear_x: float",
            "angular_z: float",
        ],
    );

    // _deserialize_payload helper function
    assert_contains_all(
        &rendered,
        &["def _deserialize_payload(payload: bytes) -> Message:"],
    );

    // Subscriber function signature
    assert_contains_all(
        &rendered,
        &[
            "async def on_next_message_received(",
            "node_runner: peppylib.NodeRunner",
            "core_node_target: Optional[str] = None",
            "instance_id_target: Optional[str] = None",
            ") -> Tuple[str, Message]:",
        ],
    );

    // External subscribe: uses consume_external, no node_name
    assert_contains_all(
        &rendered,
        &["\"cmd_vel\"", "peppylib.TopicMessenger.consume_external("],
    );
    assert!(
        !rendered.contains("node_name"),
        "external topic should not have a node_name variable"
    );

    // on_next_message_received body
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
