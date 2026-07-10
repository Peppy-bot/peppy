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
    generator.add_emitted_topic(&topic, None).unwrap();
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
            "from importlib.resources import files",
        ],
    );

    // Lazy cached schema loader. Resource lookup goes through
    // `importlib.resources.files("peppygen")` so it's independent of where
    // the calling file lives in the package tree.
    assert_contains_all(
        &rendered,
        &[
            "@lru_cache(maxsize=1)",
            "def _video_stream_message_capnp() -> types.ModuleType:",
            "return capnp.load(str(files(\"peppygen\") / \"capnp\" / \"video_stream_message.capnp\"))",
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

    // build_message signature carries the typed message fields (no node_runner);
    // declare_publisher takes the node_runner.
    assert_contains_all(
        &rendered,
        &[
            "def build_message(",
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
            "return capnp_msg.to_bytes()",
        ],
    );

    // Pure serializer and a declared (lock-free) publisher are generated; the
    // declared publisher is the only publish path (no one-shot emit).
    assert_contains_all(
        &rendered,
        &[
            "TOPIC_NAME = \"video_stream\"",
            "QOS = peppylib.QoSProfile.SensorData",
            "def build_message(",
            "async def declare_publisher(node_runner: peppylib.NodeRunner) -> peppylib.TopicPublisher:",
            "peppylib.TopicMessenger.declare_publisher(",
        ],
    );
    assert!(
        !rendered.contains("async def emit("),
        "emit() should no longer be generated; declare_publisher is the only publish path; rendered:\n{rendered}"
    );
    assert!(
        !rendered.contains("TopicMessenger.emit"),
        "TopicMessenger.emit should no longer be generated; rendered:\n{rendered}"
    );
}

#[test]
fn emit_two_topics() {
    let topic1 = parse_emitted_topic(EMITTED_TOPIC_EXAMPLE);
    let topic2 = parse_emitted_topic(EMITTED_TOPIC_EXAMPLE2);

    let mut generator = PythonGenerator::new();
    generator.add_emitted_topic(&topic1, None).unwrap();
    generator.add_emitted_topic(&topic2, None).unwrap();
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
    let emitted_topic_with_python_keyword_fields: &str = r#"
    {
      name: "keyword_topic",
      qos_profile: "standard",
      message_format: {
        "class": "u32",
        "from": "string"
      }
    }
    "#;

    let topic = parse_emitted_topic(emitted_topic_with_python_keyword_fields);

    let mut generator = PythonGenerator::new();
    generator.add_emitted_topic(&topic, None).unwrap();
    let rendered = render_artifacts(generator.into_artifacts())
        .into_iter()
        .next()
        .expect("artifact is present");

    assert_contains_all(
        &rendered,
        &[
            "def build_message(",
            "class_: int",
            "from_: str",
            "setattr(capnp_msg, \"class\", class_)",
            "setattr(capnp_msg, \"from\", from_)",
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

    let mut generator = PythonGenerator::new();
    let err = generator.add_emitted_topic(&topic, None).unwrap_err();

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

    let mut generator = PythonGenerator::new();
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
    let emitted_topic_fixed_object_array_example = r#"
    {
      name: "detections",
      qos_profile: "sensor_data",
      message_format: {
        objects: {
          $type: "array",
          $length: 4,
          $items: {
            $type: "object",
            x: "f32",
            y: "f32"
          },
        }
      }
    }
    "#;

    let topic = parse_emitted_topic(emitted_topic_fixed_object_array_example);

    let mut generator = PythonGenerator::new();
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
            x: "f32",
            y: "f32",
            label: "string"
          }
        }
      }
    }
    "#;
    let topic = parse_emitted_topic(emitted_topic_dynamic_object_array_example);

    let mut generator = PythonGenerator::new();
    generator.add_emitted_topic(&topic, None).unwrap();
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

/// A real manifest dep (link_id present) splices the runtime binding
/// lookup as the `filter` argument of the generated subscribe:
/// `node_runner.consumer_filter(<link_id>)` resolves at runtime to the
/// slot's daemon-resolved `ConsumerFilter`, so a pinned topic slot sets
/// both wire slots (and can never receive from a same-instance_id
/// producer on another core node), a multi-bound slot subscribes to its
/// bound set only, and an unbound slot stays silent.
#[test]
fn consumed_topic_with_link_id_splices_runtime_binding_target() {
    let topic = parse_consumed_topic(SUBSCRIBED_TOPIC_EXAMPLE1);
    let format = parse_message_format(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE1);

    let mut generator = PythonGenerator::new();
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

    assert_contains_all(&rendered, &["node_runner.consumer_filter(\"cam_left\"),"]);
}

/// In the case of a topic, a "subscribed" topic is an entity that expects to receive messages
/// from another entity.
#[test]
fn consumed_topic() {
    let topic = parse_consumed_topic(SUBSCRIBED_TOPIC_EXAMPLE1);
    let format = parse_message_format(SUBSCRIBED_TOPIC_FORMAT_EXAMPLE1);

    let mut generator = PythonGenerator::new();
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

    // Imports
    assert_contains_all(
        &rendered,
        &[
            "import capnp",
            "import peppylib",
            "import types",
            "from dataclasses import dataclass",
            "from functools import lru_cache",
            "from importlib.resources import files",
            "from typing import Optional, Tuple",
        ],
    );

    // Lazy cached schema loader using `importlib.resources`. The
    // `on_next_` prefix on the file_stem comes from the shared
    // `consumed_topic_schema_key` helper in `naming.rs`, matching the Rust
    // generator's output.
    assert_contains_all(
        &rendered,
        &[
            "@lru_cache(maxsize=1)",
            "def _on_next_video_stream_message_capnp() -> types.ModuleType:",
            "return capnp.load(str(files(\"peppygen\") / \"capnp\" / \"on_next_video_stream_message.capnp\"))",
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
            "with _on_next_video_stream_message_capnp().OnNextVideoStreamMessage.from_bytes(payload) as capnp_msg:",
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

    // Held-subscription API: `subscribe()` returns a `Subscription` whose
    // `next()` yields the producer identity as a structured
    // `peppylib.ProducerRef` (its full `(core_node, instance_id)` pair); it
    // never appears as a user-facing core_node (or instance_id) parameter. The
    // class is also async-iterable via `__aiter__` / `__anext__`.
    assert_contains_all(
        &rendered,
        &[
            "class Subscription:",
            "async def subscribe(node_runner: peppylib.NodeRunner) -> Subscription:",
            "async def next(self) -> Optional[Tuple[peppylib.ProducerRef, Message]]:",
            "def __aiter__(self) -> \"Subscription\":",
            "async def __anext__(self) -> Tuple[peppylib.ProducerRef, Message]:",
            "return Subscription(inner)",
        ],
    );

    // `__anext__` must terminate async iteration by raising `StopAsyncIteration`
    // once `next()` reports the subscription has closed (returns None). Without
    // this stop path, an `async for` over the subscription would hang or keep
    // yielding past completion, so assert the full delegate-to-`next` body.
    assert_contains_all(
        &rendered,
        &[
            "result = await self.next()",
            "if result is None:",
            "raise StopAsyncIteration",
            "return result",
        ],
    );
    assert!(
        !rendered.contains("on_next_message_received"),
        "the per-call on_next_message_received API must be gone; rendered:\n{rendered}"
    );
    assert!(
        !rendered.contains("from_core_node"),
        "from_core_node should no longer appear in the generated API; rendered:\n{rendered}"
    );
    assert!(
        !rendered.contains("from_instance_id: Optional[str] = None"),
        "from_instance_id should no longer appear as a generated parameter; rendered:\n{rendered}"
    );

    // Topic metadata and subscribe call. The fixture's
    // `DependencyContext::native` defaults to `WireLinkId::wildcard()` (no
    // manifest link_id), so the subscribe call splices `None` at the
    // from_producer slot.
    assert_contains_all(
        &rendered,
        &[
            "\"uvc_camera\"",
            "\"video_stream\"",
            "peppylib.TopicMessenger.subscribe(",
            "topic_name,\n        peppylib.ConsumerFilter.any(),\n        peppylib.QoSProfile.Standard,",
        ],
    );

    // next() body: receive (None once closed), deserialize, return.
    assert_contains_all(
        &rendered,
        &[
            "raw_message = await self._inner.on_next_message()",
            "if raw_message is None:",
            "return None",
            "producer = raw_message.producer",
            "message = _deserialize_payload(raw_message.payload)",
            "return producer, message",
        ],
    );

    // Regression guard for the dropped-message bug: the topic is subscribed
    // exactly once (in `subscribe`), never per `next` call. The old
    // `on_next_message_received` re-subscribed on every call, so anything
    // published in the re-subscribe gap was silently lost; with a held
    // subscription the buffer keeps every message between `next` calls.
    assert_eq!(
        rendered
            .matches("peppylib.TopicMessenger.subscribe(")
            .count(),
        1,
        "topic must be subscribed once, not per next() call; rendered:\n{rendered}"
    );
}

#[test]
fn consumed_topic_escapes_python_keyword_fields() {
    let subscribed_topic_example_keywords: &str = r#"
    {
        link_id: "keyword_source",
        name: "keyword_topic",
    }
    "#;

    let topic = parse_consumed_topic(subscribed_topic_example_keywords);
    let subscribed_topic_format_example_keywords: &str = r#"
    {
        "class": "u32",
        "from": "string"
    }
    "#;

    let format = parse_message_format(subscribed_topic_format_example_keywords);

    let mut generator = PythonGenerator::new();
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
            "peppylib.SenderTarget.node(\"uvc_camera\", \"v1\")",
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
            "peppylib.SenderTarget.node(\"uvc_camera\", \"v1\")",
        ],
    );
    assert!(
        !sound_rendered.contains("video_stream"),
        "sound artifact should not reference video_stream"
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

    let mut generator = PythonGenerator::new();
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
        !rendered.contains("from_instance_id: Optional[str] = None"),
        "from_instance_id should no longer appear as a generated parameter; rendered:\n{rendered}"
    );

    // The fixture's `DependencyContext::native` defaults to
    // `WireLinkId::wildcard()` (no manifest link_id), so the consumed call
    // sites splice `None` at the filter slot and the user-facing
    // `target_instance_id` parameter is gone. `target_core_node` is never
    // exposed in the generated API, and the deleted `pinned_producer_for`
    // accessor must never be emitted (the runtime helper is
    // `consumer_filter`).
    assert!(
        !rendered.contains("target_core_node"),
        "target_core_node should not appear in the generated API; rendered:\n{rendered}"
    );
    assert!(
        !rendered.contains("target_instance_id: Optional[str] = None"),
        "target_instance_id should no longer appear as a generated parameter; rendered:\n{rendered}"
    );
    assert!(
        !rendered.contains("pinned_producer_for"),
        "pinned_producer_for is deleted and must never be emitted; the runtime helper is consumer_filter; rendered:\n{rendered}"
    );
}
