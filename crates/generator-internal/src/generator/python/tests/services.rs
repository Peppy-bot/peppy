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
            "from typing import Optional",
        ],
    );

    // Handler signature (includes handler parameter)
    assert_contains_all(
        &rendered,
        &[
            "\"enable_camera\"",
            "async def handle_next_request(",
            "node_runner: peppylib.NodeRunner",
            "handler",
        ],
    );

    // Messenger integration
    assert_contains_all(&rendered, &["peppylib.ServiceMessenger.listen("]);

    // master_node field in Request
    assert_contains_all(&rendered, &["master_node: str"]);

    // _deserialize_request function
    assert_contains_all(
        &rendered,
        &[
            "def _deserialize_request(payload):",
            "return RequestData(",
        ],
    );

    // _handle_request_payload function
    assert_contains_all(
        &rendered,
        &[
            "def _handle_request_payload(payload, handler, master_node, instance_id):",
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
    assert_contains_all(&rendered, &["class Request:", "instance_id: str", "master_node: str"]);

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

    // _handle_request_payload without payload parameter
    assert_contains_all(
        &rendered,
        &[
            "def _handle_request_payload(handler, master_node, instance_id):",
            "request = Request(instance_id=instance_id, master_node=master_node)",
        ],
    );

    // handler parameter in handle_next_request
    assert_contains_all(&rendered, &["async def handle_next_request(", "handler"]);

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

    // Response dataclasses
    assert_contains_all(
        &rendered,
        &[
            "@dataclass",
            "class ResponseData:",
            "class Response:",
            "data: ResponseData",
        ],
    );

    // Request dataclass
    assert_contains_all(&rendered, &["class Request:", "enable: bool"]);

    // Poll function signature
    assert_contains_all(
        &rendered,
        &["async def poll(", "node_runner: peppylib.NodeRunner"],
    );

    // Messenger integration
    assert_contains_all(&rendered, &["peppylib.ServiceMessenger.poll("]);
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

    // Poll function should still be generated
    assert_artifact_contains(&artifacts, "peppylib.ServiceMessenger.poll(");
}
