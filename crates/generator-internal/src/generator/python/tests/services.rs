use super::*;
use config::node::{ConsumedService, ExposedService, MessageFormat};

const EXPOSED_SERVICE_EXAMPLE: &str = r#"
{
  name: "enable_camera",
  request_message_format: {
    enable: "bool"
  },
  response_message_format: {
    enabled: "bool",
    error_msg: {
      $type: "string",
      $optional: true
    }
  }
}
"#;

const EXPOSED_SERVICE_EXAMPLE2: &str = r#"
{
    name: "get_lidar_info",
    request_message_format: {
      channels: "string",
      horizontal_fov: "string",
      vertical_fov: "string",
      resolution: "string",
      frequency: "string"
    }
}
"#;

const EXPOSED_SERVICE_EXAMPLE3: &str = r#"
{
    name: "get_system_status",
    response_message_format: {
        healthy: "bool"
    }
}
"#;

pub(super) const SUBSCRIBED_SERVICE_EXAMPLE1: &str = r#"
{
  link_id: "uvc_camera",
  name: "enable_camera",
}
"#;

pub(super) const SUBSCRIBED_SERVICE_REQUEST_EXAMPLE1: &str = r#"
{
  enable: "bool"
}
"#;

pub(super) const SUBSCRIBED_SERVICE_RESPONSE_EXAMPLE1: &str = r#"
{
  enabled: "bool",
  error_msg: {
    $type: "string",
    $optional: true
  },
}
"#;

const SUBSCRIBED_SERVICE_RESPONSE_OPTIONAL_SCALAR_AND_BYTES: &str = r#"
{
  maybe_code: {
    $type: "u32",
    $optional: true
  },
  maybe_payload: {
    $type: "bytes",
    $optional: true
  }
}
"#;

const SUBSCRIBED_SERVICE_EXAMPLE2: &str = r#"
{
    link_id: "uvc_camera",
    name: "get_camera_info",
}
"#;

// No request body for the second consumed service.
const SUBSCRIBED_SERVICE_RESPONSE_EXAMPLE2: &str = r#"
{
    card_type: "string",
    size: "string",
    interval: "string"
}
"#;

const EMPTY_MESSAGE_FORMAT: &str = r#"{}"#;

/// In the case of a service, an "exposed" service is an entity that accepts incoming requests.
#[test]
fn expose_service() {
    let service: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE).unwrap();

    let mut generator = PythonGenerator::new();
    generator.add_exposed_service(&service, None).unwrap();
    let artifacts = render_artifacts(generator.into_artifacts());
    assert_eq!(
        artifacts.len(),
        1,
        "expected a single generated artifact, got {}",
        artifacts.len()
    );
    let rendered = artifacts.into_iter().next().expect("artifact is present");

    // Response dataclass
    assert_contains_all(
        &rendered,
        &[
            "@dataclass",
            "class Response:",
            "enabled: bool",
            "error_msg: Optional[str]",
        ],
    );

    // Request structs
    assert_contains_all(
        &rendered,
        &[
            "class RequestData:",
            "class Request:",
            "instance_id: str",
            "data: RequestData",
        ],
    );

    // Imports
    assert_contains_all(
        &rendered,
        &[
            "import peppylib",
            "from dataclasses import dataclass",
            "from typing import Callable",
            "from typing import Optional",
        ],
    );

    // Handler signature (includes typed handler parameter and return type)
    assert_contains_all(
        &rendered,
        &[
            "\"enable_camera\"",
            "async def handle_next_request(",
            "node_runner: peppylib.NodeRunner",
            "handler: Callable[[Request], Response]",
            ") -> None:",
        ],
    );

    // Messenger integration
    assert_contains_all(&rendered, &["peppylib.ServiceMessenger.listen("]);

    // core_node field in Request
    assert_contains_all(&rendered, &["core_node: str"]);

    // _deserialize_request function with typed signature
    assert_contains_all(
        &rendered,
        &[
            "def _deserialize_request(payload: bytes) -> RequestData:",
            "return RequestData(",
        ],
    );

    // _handle_request_payload function with typed signature
    assert_contains_all(
        &rendered,
        &[
            "async def _handle_request_payload(payload: bytes, handler: Callable[[Request], Response], core_node: str, instance_id: str) -> bytes:",
            "request_data = _deserialize_request(payload)",
            "request = Request(instance_id=instance_id, core_node=core_node, data=request_data)",
            "response = handler(request)",
            "if hasattr(response, \"__await__\"):",
            "response = await response",
            "return capnp_msg.to_bytes()",
        ],
    );

    // _on_request wrapper inside handle_next_request
    assert_contains_all(
        &rendered,
        &[
            "async def _on_request(request_context):",
            "return await _handle_request_payload(payload, handler, core_node, instance_id)",
            "await endpoint.handle_next_request(_on_request)",
        ],
    );
}

#[test]
fn expose_service_without_request_body() {
    let service: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE3).unwrap();

    let mut generator = PythonGenerator::new();
    generator.add_exposed_service(&service, None).unwrap();
    let rendered = render_artifacts(generator.into_artifacts())
        .into_iter()
        .next()
        .expect("artifact is present");

    // Service without request body should still have Request class for metadata
    assert_contains_all(
        &rendered,
        &["class Request:", "instance_id: str", "core_node: str"],
    );

    // But no RequestData class
    assert_rendered!(
        !rendered.contains("class RequestData"),
        &rendered,
        "expected no RequestData class when there is no request body"
    );

    // Should NOT have _deserialize_request (no request format)
    assert_rendered!(
        !rendered.contains("def _deserialize_request"),
        &rendered,
        "expected no _deserialize_request when there is no request body"
    );

    // _handle_request_payload without payload parameter, typed signature
    assert_contains_all(
        &rendered,
        &[
            "async def _handle_request_payload(handler: Callable[[Request], Response], core_node: str, instance_id: str) -> bytes:",
            "request = Request(instance_id=instance_id, core_node=core_node)",
        ],
    );

    // handler parameter in handle_next_request with return type
    assert_contains_all(
        &rendered,
        &[
            "async def handle_next_request(",
            "handler: Callable[[Request], Response]",
            ") -> None:",
        ],
    );

    // _on_request wrapper without payload
    assert_contains_all(
        &rendered,
        &[
            "async def _on_request(request_context):",
            "return await _handle_request_payload(handler, core_node, instance_id)",
            "await endpoint.handle_next_request(_on_request)",
        ],
    );
}

#[test]
fn expose_two_services() {
    let service1: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE).unwrap();
    let service2: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE2).unwrap();

    let mut generator = PythonGenerator::new();
    generator.add_exposed_service(&service1, None).unwrap();
    generator.add_exposed_service(&service2, None).unwrap();

    let artifacts = render_artifacts(generator.into_artifacts());
    assert_eq!(
        artifacts.len(),
        2,
        "expected two generated artifacts, got {}",
        artifacts.len()
    );

    // Verify each service gets its own distinct artifact
    assert_artifact_contains(&artifacts, "\"enable_camera\"");
    assert_artifact_contains(&artifacts, "\"get_lidar_info\"");

    // Verify each artifact is self-contained and doesn't leak the other service
    let camera_rendered = artifacts
        .iter()
        .find(|a| a.contains("\"enable_camera\""))
        .expect("enable_camera artifact is present");
    let lidar_rendered = artifacts
        .iter()
        .find(|a| a.contains("\"get_lidar_info\""))
        .expect("get_lidar_info artifact is present");

    assert_contains_all(
        camera_rendered,
        &[
            "\"enable_camera\"",
            "async def handle_next_request(",
            "peppylib.ServiceMessenger.listen(",
            "enable: bool",
        ],
    );
    assert!(
        !camera_rendered.contains("get_lidar_info"),
        "enable_camera artifact should not reference get_lidar_info"
    );

    assert_contains_all(
        lidar_rendered,
        &[
            "\"get_lidar_info\"",
            "async def handle_next_request(",
            "peppylib.ServiceMessenger.listen(",
            "channels: str",
        ],
    );
    assert!(
        !lidar_rendered.contains("enable_camera"),
        "get_lidar_info artifact should not reference enable_camera"
    );
}

/// In the case of a service, a "subscribed" service is an entity that connects to another
/// entity to call its service.
#[test]
fn consumed_service() {
    let service: ConsumedService = serde_json5::from_str(SUBSCRIBED_SERVICE_EXAMPLE1).unwrap();
    let request_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_SERVICE_REQUEST_EXAMPLE1).unwrap();
    let response_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_SERVICE_RESPONSE_EXAMPLE1).unwrap();

    let mut generator = PythonGenerator::new();
    generator
        .add_consumed_service(
            &service,
            &request_format,
            &response_format,
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

    // Module-level constants
    assert_contains_all(&rendered, &["\"uvc_camera\"", "\"enable_camera\""]);

    // Imports
    assert_contains_all(
        &rendered,
        &[
            "import peppylib",
            "from dataclasses import dataclass",
            "from typing import Optional",
        ],
    );

    // Capnp schema loaders
    assert_contains_all(
        &rendered,
        &["import capnp", "from functools import lru_cache"],
    );

    // Request dataclass
    assert_contains_all(&rendered, &["@dataclass", "class Request:", "enable: bool"]);

    // Response dataclasses
    assert_contains_all(
        &rendered,
        &[
            "class ResponseData:",
            "enabled: bool",
            "error_msg: Optional[str]",
            "class Response:",
            "core_node: str",
            "instance_id: str",
            "data: ResponseData",
        ],
    );

    // _deserialize_response function
    assert_contains_all(
        &rendered,
        &[
            "def _deserialize_response(payload: bytes) -> ResponseData:",
            "return ResponseData(",
        ],
    );

    // Poll function signature with typed params and return type. The
    // fixture's `DependencyContext::native` defaults to
    // `WireLinkId::wildcard()` (no manifest link_id), so the poll call
    // splices `None` at the filter slot and the user-facing
    // `target_instance_id` parameter is gone. `target_core_node` is never
    // exposed in the user-facing generated API, and the deleted
    // `pinned_producer_for` accessor must never be emitted (the runtime
    // helper is `consumer_filter`).
    assert_contains_all(
        &rendered,
        &[
            "async def poll(",
            "node_runner: peppylib.NodeRunner",
            "request: Request",
            "timeout: float",
            ") -> Response:",
        ],
    );
    assert!(
        !rendered.contains("target_instance_id: Optional[str] = None"),
        "target_instance_id should no longer appear as a generated parameter; got:\n{rendered}"
    );
    assert!(
        !rendered.contains("target_core_node"),
        "target_core_node should not appear in the generated API; got:\n{rendered}"
    );
    assert!(
        !rendered.contains("pinned_producer_for"),
        "pinned_producer_for is deleted and must never be emitted; the runtime helper is consumer_filter; got:\n{rendered}"
    );

    // Request serialization
    assert_contains_all(&rendered, &["request_payload = capnp_msg.to_bytes()"]);

    // Messenger integration, including the `None` spliced at the poll
    // call's filter slot (the fixture models no manifest dep).
    assert_contains_all(
        &rendered,
        &[
            "peppylib.ServiceMessenger.poll(",
            "SERVICE_NAME,\n        None,\n        request_payload,",
        ],
    );

    // Response deserialization in poll body
    assert_contains_all(
        &rendered,
        &[
            "response_data = _deserialize_response(payload)",
            "return Response(",
        ],
    );
}

#[test]
fn consumed_service_optional_scalar_and_bytes_use_has_checks() {
    let service: ConsumedService = serde_json5::from_str(SUBSCRIBED_SERVICE_EXAMPLE1).unwrap();
    let request_format: MessageFormat = serde_json5::from_str(EMPTY_MESSAGE_FORMAT).unwrap();
    let response_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_SERVICE_RESPONSE_OPTIONAL_SCALAR_AND_BYTES).unwrap();

    let mut generator = PythonGenerator::new();
    generator
        .add_consumed_service(
            &service,
            &request_format,
            &response_format,
            &crate::DependencyContext::native("uvc_camera", "v1"),
        )
        .unwrap();
    let rendered = render_artifacts(generator.into_artifacts())
        .into_iter()
        .next()
        .expect("artifact is present");

    assert_contains_all(
        &rendered,
        &[
            "maybe_code: Optional[int]",
            "maybe_payload: Optional[bytes]",
            "if not capnp_msg._has(\"maybeCode\"):",
            "if not capnp_msg._has(\"maybePayload\"):",
            "maybe_code_0 = None",
            "maybe_payload_1 = None",
        ],
    );
}

#[test]
fn consumed_two_services_same_node() {
    let service1: ConsumedService = serde_json5::from_str(SUBSCRIBED_SERVICE_EXAMPLE1).unwrap();
    let request_format1: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_SERVICE_REQUEST_EXAMPLE1).unwrap();
    let response_format1: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_SERVICE_RESPONSE_EXAMPLE1).unwrap();

    // Second service pointing to the same node
    let service2: ConsumedService = serde_json5::from_str(SUBSCRIBED_SERVICE_EXAMPLE2).unwrap();
    let empty_format: MessageFormat = serde_json5::from_str(EMPTY_MESSAGE_FORMAT).unwrap();
    let response_format2: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_SERVICE_RESPONSE_EXAMPLE2).unwrap();

    let mut generator = PythonGenerator::new();
    generator
        .add_consumed_service(
            &service1,
            &request_format1,
            &response_format1,
            &crate::DependencyContext::native("uvc_camera", "v1"),
        )
        .unwrap();
    generator
        .add_consumed_service(
            &service2,
            &empty_format,
            &response_format2,
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

    // Verify each service gets distinct artifact with correct service name
    assert_artifact_contains(&artifacts, "\"enable_camera\"");
    assert_artifact_contains(&artifacts, "\"get_camera_info\"");

    // Verify both reference the same source node
    for rendered in &artifacts {
        assert_contains_all(rendered, &["\"uvc_camera\""]);
    }

    // enable_camera has request+response: full path
    let enable_camera = artifacts
        .iter()
        .find(|a| a.contains("\"enable_camera\""))
        .expect("enable_camera artifact is present");
    assert_contains_all(
        enable_camera,
        &[
            "request: Request",
            "def _deserialize_response(payload: bytes) -> ResponseData:",
            ") -> Response:",
        ],
    );

    // get_camera_info has no request body but has response
    let camera_info = artifacts
        .iter()
        .find(|a| a.contains("\"get_camera_info\""))
        .expect("get_camera_info artifact is present");
    assert_contains_all(
        camera_info,
        &[
            "card_type: str",
            "request_payload = b\"\"",
            "def _deserialize_response(payload: bytes) -> ResponseData:",
            ") -> Response:",
        ],
    );
    assert_rendered!(
        !camera_info.contains("class Request:"),
        camera_info,
        "get_camera_info should not have Request class (no request format)"
    );
}

#[test]
fn consumed_service_without_response_payload() {
    let service: ConsumedService = serde_json5::from_str(
        r#"
        {
            link_id: "uvc_camera",
            name: "get_camera_info",
        }
        "#,
    )
    .unwrap();
    let empty_format: MessageFormat = serde_json5::from_str(EMPTY_MESSAGE_FORMAT).unwrap();

    let mut generator = PythonGenerator::new();
    generator
        .add_consumed_service(
            &service,
            &empty_format,
            &empty_format,
            &crate::DependencyContext::native("uvc_camera", "v1"),
        )
        .expect("generator should allow services without response format");

    let artifacts = render_artifacts(generator.into_artifacts());
    assert_eq!(
        artifacts.len(),
        1,
        "expected single generated artifact, got {}",
        artifacts.len()
    );
    let rendered = artifacts.into_iter().next().expect("artifact is present");

    // Poll function should still be generated
    assert_contains_all(&rendered, &["peppylib.ServiceMessenger.poll("]);

    // Return type should be None
    assert_contains_all(&rendered, &["-> None:"]);

    // Empty request payload (no request format)
    assert_contains_all(&rendered, &["request_payload = b\"\""]);

    // Should NOT have Response/ResponseData classes
    assert_rendered!(
        !rendered.contains("class ResponseData"),
        &rendered,
        "expected no ResponseData class when there is no response format"
    );
    assert_rendered!(
        !rendered.contains("class Response"),
        &rendered,
        "expected no Response class when there is no response format"
    );

    // Should NOT have _deserialize_response
    assert_rendered!(
        !rendered.contains("def _deserialize_response"),
        &rendered,
        "expected no _deserialize_response when there is no response format"
    );
}
