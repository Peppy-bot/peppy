use super::*;
use config::node::{ExposedService, MessageFormat, SubscribedService};

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

const SUBSCRIBED_SERVICE_EXAMPLE1: &str = r#"
{
  id: "uvc_camera_enable_camera",
  node: "uvc_camera",
  name: "enable_camera",
  tag: "0.1.0"
}
"#;

const SUBSCRIBED_SERVICE_REQUEST_EXAMPLE1: &str = r#"
{
  enable: "bool"
}
"#;

const SUBSCRIBED_SERVICE_RESPONSE_EXAMPLE1: &str = r#"
{
  enabled: "bool",
  error_msg: {
    $type: "string",
    $optional: true
  },
}
"#;

const SUBSCRIBED_SERVICE_EXAMPLE2: &str = r#"
{
    id: "uvc_camera_get_camera_info",
    node: "uvc_camera",
    name: "get_camera_info",
    tag: "0.1.0"
}
"#;

// No request body for the second subscribed service.
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
    generator.add_exposed_service(&service).unwrap();
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

    // master_node field in Request
    assert_contains_all(&rendered, &["master_node: str"]);

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
            "def _handle_request_payload(payload: bytes, handler: Callable[[Request], Response], master_node: str, instance_id: str) -> bytes:",
            "request_data = _deserialize_request(payload)",
            "request = Request(instance_id=instance_id, master_node=master_node, data=request_data)",
            "response = handler(request)",
            "return capnp_msg.to_bytes()",
        ],
    );

    // _on_request wrapper inside handle_next_request
    assert_contains_all(
        &rendered,
        &[
            "async def _on_request(request_context):",
            "return _handle_request_payload(payload, handler, master_node, instance_id)",
            "await endpoint.handle_next_request(_on_request)",
        ],
    );
}

#[test]
fn expose_service_without_request_body() {
    let service: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE3).unwrap();

    let mut generator = PythonGenerator::new();
    generator.add_exposed_service(&service).unwrap();
    let rendered = render_artifacts(generator.into_artifacts())
        .into_iter()
        .next()
        .expect("artifact is present");

    // Service without request body should still have Request class for metadata
    assert_contains_all(
        &rendered,
        &["class Request:", "instance_id: str", "master_node: str"],
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
            "def _handle_request_payload(handler: Callable[[Request], Response], master_node: str, instance_id: str) -> bytes:",
            "request = Request(instance_id=instance_id, master_node=master_node)",
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
            "return _handle_request_payload(handler, master_node, instance_id)",
            "await endpoint.handle_next_request(_on_request)",
        ],
    );
}

#[test]
fn expose_two_services() {
    let service1: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE).unwrap();
    let service2: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE2).unwrap();

    let mut generator = PythonGenerator::new();
    generator.add_exposed_service(&service1).unwrap();
    generator.add_exposed_service(&service2).unwrap();

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
fn subscribed_to_service() {
    let service: SubscribedService = serde_json5::from_str(SUBSCRIBED_SERVICE_EXAMPLE1).unwrap();
    let request_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_SERVICE_REQUEST_EXAMPLE1).unwrap();
    let response_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_SERVICE_RESPONSE_EXAMPLE1).unwrap();

    let mut generator = PythonGenerator::new();
    generator
        .add_subscribed_service(&service, &request_format, &response_format)
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
            "master_node: str",
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

    // Poll function signature with typed params and return type
    assert_contains_all(
        &rendered,
        &[
            "async def poll(",
            "node_runner: peppylib.NodeRunner",
            "request: Request",
            "timeout: float",
            "target_master_node: Optional[str] = None",
            "target_instance_id: Optional[str] = None",
            ") -> Response:",
        ],
    );

    // Request serialization
    assert_contains_all(&rendered, &["request_payload = capnp_msg.to_bytes()"]);

    // Messenger integration
    assert_contains_all(&rendered, &["peppylib.ServiceMessenger.poll("]);

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
fn subscribed_to_two_services_same_node() {
    let service1: SubscribedService = serde_json5::from_str(SUBSCRIBED_SERVICE_EXAMPLE1).unwrap();
    let request_format1: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_SERVICE_REQUEST_EXAMPLE1).unwrap();
    let response_format1: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_SERVICE_RESPONSE_EXAMPLE1).unwrap();

    // Second service pointing to the same node
    let service2: SubscribedService = serde_json5::from_str(SUBSCRIBED_SERVICE_EXAMPLE2).unwrap();
    let empty_format: MessageFormat = serde_json5::from_str(EMPTY_MESSAGE_FORMAT).unwrap();
    let response_format2: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_SERVICE_RESPONSE_EXAMPLE2).unwrap();

    let mut generator = PythonGenerator::new();
    generator
        .add_subscribed_service(&service1, &request_format1, &response_format1)
        .unwrap();
    generator
        .add_subscribed_service(&service2, &empty_format, &response_format2)
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

    // get_camera_info has specific response fields
    assert_artifact_contains(&artifacts, "card_type: str");
}

#[test]
fn subscribed_service_without_response_payload() {
    let service: SubscribedService = serde_json5::from_str(
        r#"
        {
            id: "uvc_camera_get_camera_info",
            node: "uvc_camera",
            name: "get_camera_info",
            tag: "0.1.0"
        }
        "#,
    )
    .unwrap();
    let empty_format: MessageFormat = serde_json5::from_str(EMPTY_MESSAGE_FORMAT).unwrap();

    let mut generator = PythonGenerator::new();
    generator
        .add_subscribed_service(&service, &empty_format, &empty_format)
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
