use std::process::Command;

use super::*;
use config::node::{ExposedService, SubscribedService};

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
// No request body for the second subscribed service
const SUBSCRIBED_SERVICE_RESPONSE_EXAMPLE2: &str = r#"
{
    card_type: "string",
    size: "string",
    interval: "string"
}
"#;

/// In the case of a service, an "exposed" service is an entity that accept incoming messages
#[test]
fn expose_service() {
    let service: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE).unwrap();

    let mut generator = RustGenerator::new();
    generator.add_exposed_service(&service).unwrap();
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

    // Response struct
    assert_rendered!(
        rendered.contains("pub struct Response"),
        &rendered,
        "expected response struct"
    );
    assert_rendered!(
        rendered.contains("enabled: bool"),
        &rendered,
        "expected bool field in response"
    );
    assert_rendered!(
        rendered.contains("error_msg: Option<String>"),
        &rendered,
        "expected optional string field in response"
    );

    // Request structs
    assert_rendered!(
        rendered.contains("pub struct RequestData"),
        &rendered,
        "expected RequestData struct"
    );
    assert_rendered!(
        rendered.contains("pub struct Request"),
        &rendered,
        "expected Request struct with metadata"
    );
    assert_rendered!(
        rendered.contains("pub instance_id: String"),
        &rendered,
        "expected instance_id field in Request"
    );
    assert_rendered!(
        rendered.contains("pub data: RequestData"),
        &rendered,
        "expected data field in Request"
    );

    // Handler signature
    assert_rendered!(
        rendered.contains("const SERVICE_NAME: &str = \"enable_camera\";"),
        &rendered,
        "expected SERVICE_NAME constant"
    );
    assert_rendered!(
        rendered.contains("pub async fn handle_next_request<F>"),
        &rendered,
        "expected async service handler"
    );
    assert_rendered!(
        rendered.contains("F: Fn(Request) -> crate::Result<Response>"),
        &rendered,
        "expected handler trait bound"
    );

    // Request processing
    assert_rendered!(
        rendered.contains("fn deserialize_request(payload: &[u8]) -> crate::Result<RequestData>"),
        &rendered,
        "expected request deserializer"
    );
    assert_rendered!(
        rendered.contains("fn handle_request_payload<F>("),
        &rendered,
        "expected payload handler helper"
    );

    // Messenger integration
    assert_rendered!(
        rendered.contains("peppylib::ServiceMessenger::listen"),
        &rendered,
        "expected service listen call"
    );
    assert_rendered!(
        rendered.contains(".handle_next_request(move |request_context|"),
        &rendered,
        "expected request handling callback"
    );
}

#[test]
fn expose_service_without_request_body() {
    let service: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE3).unwrap();

    let mut generator = RustGenerator::new();
    generator.add_exposed_service(&service).unwrap();
    let rendered = generator
        .into_artifacts()
        .into_iter()
        .map(|artifact| artifact.code_output)
        .next()
        .expect("artifact is present");

    // Service without request body should still have Request struct for metadata
    assert_rendered!(
        rendered.contains("pub struct Request"),
        &rendered,
        "expected Request struct with metadata"
    );
    assert_rendered!(
        rendered.contains("pub instance_id: String"),
        &rendered,
        "expected instance_id in Request"
    );

    // But no RequestData struct
    assert_rendered!(
        !rendered.contains("pub struct RequestData"),
        &rendered,
        "expected no RequestData struct when there is no request body"
    );

    // Request construction should omit data field
    assert_rendered!(
        rendered.contains("let request = Request {"),
        &rendered,
        "expected Request construction using struct literal"
    );
}

#[test]
fn expose_two_services() {
    let service1: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE).unwrap();
    let service2: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE2).unwrap();

    let mut generator = RustGenerator::new();
    generator.add_exposed_service(&service1).unwrap();
    generator.add_exposed_service(&service2).unwrap();

    let artifacts: Vec<String> = generator
        .into_artifacts()
        .into_iter()
        .map(|artifact| artifact.code_output)
        .collect();

    assert_eq!(
        artifacts.len(),
        2,
        "expected two generated artifacts, got {}",
        artifacts.len()
    );

    let enable_rendered = artifacts
        .iter()
        .find(|rendered| rendered.contains("enable_camera"))
        .expect("enable_camera artifact is present");
    let lidar_rendered = artifacts
        .iter()
        .find(|rendered| rendered.contains("get_lidar_info"))
        .expect("get_lidar_info artifact is present");

    // enable_camera has response, get_lidar_info does not
    assert_rendered!(
        enable_rendered.contains("pub struct Response"),
        enable_rendered,
        "expected response struct for enable_camera"
    );
    assert_rendered!(
        enable_rendered.contains("F: Fn(Request) -> crate::Result<Response>"),
        enable_rendered,
        "expected handler to return Response for enable_camera"
    );

    assert_rendered!(
        !lidar_rendered.contains("pub struct Response"),
        lidar_rendered,
        "expected get_lidar_info to omit response struct"
    );
    assert_rendered!(
        lidar_rendered.contains("F: Fn(Request) -> crate::Result<()>"),
        lidar_rendered,
        "expected handler to return unit for get_lidar_info"
    );
}

/// In the case of a service, a "subscribed" service is an entity expects to connect to another entity
#[test]
fn subscribed_to_service() {
    let service: SubscribedService = serde_json5::from_str(SUBSCRIBED_SERVICE_EXAMPLE1).unwrap();
    let request_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_SERVICE_REQUEST_EXAMPLE1).unwrap();
    let response_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_SERVICE_RESPONSE_EXAMPLE1).unwrap();

    let mut generator = RustGenerator::new();
    generator
        .add_subscribed_service(&service, Some(&request_format), Some(&response_format))
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

    // Module-level constants
    assert_rendered!(
        rendered.contains("const NODE_NAME: &str = \"uvc_camera\";"),
        &rendered,
        "expected NODE_NAME constant"
    );
    assert_rendered!(
        rendered.contains("const SERVICE_NAME: &str = \"enable_camera\";"),
        &rendered,
        "expected SERVICE_NAME constant"
    );

    // Response structs
    assert_rendered!(
        rendered.contains("pub struct ResponseData"),
        &rendered,
        "expected ResponseData struct"
    );
    assert_rendered!(
        rendered.contains("pub struct Response"),
        &rendered,
        "expected Response struct with metadata"
    );
    assert_rendered!(
        rendered.contains("pub data: ResponseData"),
        &rendered,
        "expected data field in Response"
    );

    // Request struct
    assert_rendered!(
        rendered.contains("pub struct Request"),
        &rendered,
        "expected request struct"
    );
    assert_rendered!(
        rendered.contains("pub enable: bool"),
        &rendered,
        "expected request field"
    );

    // Poll function signature
    assert_rendered!(
        rendered.contains("pub async fn poll("),
        &rendered,
        "expected async poll helper"
    );
    assert_rendered!(
        rendered.contains("target_master_node: Option<&str>"),
        &rendered,
        "expected target_master_node parameter"
    );
    assert_rendered!(
        rendered.contains("target_instance_id: Option<&str>"),
        &rendered,
        "expected target_instance_id parameter"
    );
    assert_rendered!(
        rendered.contains("-> crate::Result<Response>"),
        &rendered,
        "expected poll to return Response"
    );

    // Request serialization
    assert_rendered!(
        rendered.contains("root.set_enable(enable);"),
        &rendered,
        "expected request serialization"
    );

    // Messenger integration
    assert_rendered!(
        rendered.contains("peppylib::ServiceMessenger::poll("),
        &rendered,
        "expected poll helper invocation"
    );

    // Response deserialization
    assert_rendered!(
        rendered.contains("fn deserialize_response(payload: &[u8]) -> crate::Result<ResponseData>"),
        &rendered,
        "expected deserialize_response function"
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
    let response_format2: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_SERVICE_RESPONSE_EXAMPLE2).unwrap();

    let mut generator = RustGenerator::new();
    generator
        .add_subscribed_service(&service1, Some(&request_format1), Some(&response_format1))
        .unwrap();
    generator
        .add_subscribed_service(&service2, None, Some(&response_format2))
        .unwrap();
    let artifacts = generator.into_artifacts();
    assert_eq!(
        artifacts.len(),
        2,
        "expected two generated artifacts, got {}",
        artifacts.len()
    );

    let enable_module_name = subscribed_service_module_name(&service1);
    let enable_rendered = &artifacts
        .iter()
        .find(|artifact| artifact.node_name.as_str() == enable_module_name.as_str())
        .unwrap_or_else(|| panic!("expected {enable_module_name} artifact to be generated"))
        .code_output;
    let camera_module_name = subscribed_service_module_name(&service2);
    let get_info_rendered = &artifacts
        .iter()
        .find(|artifact| artifact.node_name.as_str() == camera_module_name.as_str())
        .unwrap_or_else(|| panic!("expected {camera_module_name} artifact to be generated"))
        .code_output;

    // Both services point to the same node
    assert_rendered!(
        enable_rendered.contains("const NODE_NAME: &str = \"uvc_camera\";"),
        enable_rendered,
        "expected NODE_NAME for enable_camera"
    );
    assert_rendered!(
        get_info_rendered.contains("const NODE_NAME: &str = \"uvc_camera\";"),
        get_info_rendered,
        "expected NODE_NAME for get_camera_info"
    );

    // Each has distinct SERVICE_NAME
    assert_rendered!(
        enable_rendered.contains("const SERVICE_NAME: &str = \"enable_camera\";"),
        enable_rendered,
        "expected SERVICE_NAME for enable_camera"
    );
    assert_rendered!(
        get_info_rendered.contains("const SERVICE_NAME: &str = \"get_camera_info\";"),
        get_info_rendered,
        "expected SERVICE_NAME for get_camera_info"
    );

    // get_camera_info has specific response fields
    assert_rendered!(
        get_info_rendered.contains("card_type: String"),
        get_info_rendered,
        "expected card_type response field"
    );
}

#[test]
fn subscribed_service_without_response_payload() {
    let service = r#"
        {
            id: "uvc_camera_get_camera_info",
            node: "uvc_camera",
            name: "get_camera_info",
            tag: "0.1.0"
        }
        "#;
    let service: SubscribedService = serde_json5::from_str(service).unwrap();

    let mut generator = RustGenerator::new();
    generator
        .add_subscribed_service(&service, None, None)
        .expect("generator should allow services without response format");

    let artifacts = generator.into_artifacts();
    assert_eq!(
        artifacts.len(),
        1,
        "expected single generated artifact, got {}",
        artifacts.len()
    );

    let rendered = &artifacts[0].code_output;

    assert_rendered!(
        rendered.contains("let _ = peppylib::ServiceMessenger::poll("),
        rendered,
        "expected poll invocation that discards response bytes"
    );
}

/// This is a long running test that verifies the generated code compiles and passes clippy
#[test]
fn compile_lib_with_exposed_and_subscribed_services() {
    let temp_dir = TempDir::new().unwrap();
    let exposed_service1: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE).unwrap();
    let exposed_service2: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE2).unwrap();

    let subscribed_service1: SubscribedService =
        serde_json5::from_str(SUBSCRIBED_SERVICE_EXAMPLE1).unwrap();

    let subscribed_service_request1: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_SERVICE_REQUEST_EXAMPLE1).unwrap();
    let subscribed_service_response1: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_SERVICE_RESPONSE_EXAMPLE1).unwrap();

    // Second service pointing to the same node
    let subscribed_service2: SubscribedService =
        serde_json5::from_str(SUBSCRIBED_SERVICE_EXAMPLE2).unwrap();
    let subscribed_service_response2: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_SERVICE_RESPONSE_EXAMPLE2).unwrap();

    let (mut generator, output_dir, user_node, _) = init_test_env(&temp_dir);
    generator.add_exposed_service(&exposed_service1).unwrap();
    generator.add_exposed_service(&exposed_service2).unwrap();
    generator
        .add_subscribed_service(
            &subscribed_service1,
            Some(&subscribed_service_request1),
            Some(&subscribed_service_response1),
        )
        .unwrap();
    generator
        .add_subscribed_service(
            &subscribed_service2,
            None,
            Some(&subscribed_service_response2),
        )
        .unwrap();
    let output_config = copy_config_to_output(&user_node, &output_dir);
    generator.build(&output_dir).unwrap();
    fs::remove_file(output_config).unwrap();

    let cargo_output = Command::new("cargo")
        .arg("build")
        .env("CARGO_NET_OFFLINE", "true")
        .current_dir(&output_dir)
        .output()
        .expect("failed to invoke cargo build on generated crate");
    assert!(
        cargo_output.status.success(),
        "cargo build failed for generated crate with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        cargo_output.status.code(),
        String::from_utf8_lossy(&cargo_output.stdout),
        String::from_utf8_lossy(&cargo_output.stderr)
    );

    let clippy_output = Command::new("cargo")
        .arg("clippy")
        .arg("--all-targets")
        .arg("--color")
        .arg("always")
        .arg("--")
        .arg("-D")
        .arg("warnings")
        .env("CARGO_NET_OFFLINE", "true")
        .current_dir(&output_dir)
        .output()
        .expect("failed to run cargo clippy on generated crate");
    assert!(
        clippy_output.status.success(),
        "cargo clippy failed for generated crate with status: {:?}\nstdout:\n{}\nstderr:\n{}",
        clippy_output.status.code(),
        String::from_utf8_lossy(&clippy_output.stdout),
        String::from_utf8_lossy(&clippy_output.stderr)
    );

    // Verify module structure is generated correctly
    assert!(
        output_dir.join("Cargo.toml").exists(),
        "Expected Cargo.toml to be generated"
    );
    assert!(
        !output_dir.join(PEPPY_NODE_CONFIG_FILE).exists(),
        "Generated crate should not keep a copy of the node configuration file"
    );

    let lib_contents =
        std::fs::read_to_string(output_dir.join("src/lib.rs")).expect("failed to read lib.rs");
    assert!(
        lib_contents.contains("pub mod exposed_services;")
            && lib_contents.contains("pub mod subscribed_services;"),
        "Expected lib.rs to re-export service modules"
    );

    // Verify expected module files exist
    assert!(
        output_dir
            .join("src/exposed_services/enable_camera.rs")
            .exists(),
        "Expected enable_camera exposed service module"
    );
    assert!(
        output_dir
            .join("src/exposed_services/get_lidar_info.rs")
            .exists(),
        "Expected get_lidar_info exposed service module"
    );
    assert!(
        output_dir
            .join("src/subscribed_services/uvc_camera_enable_camera.rs")
            .exists(),
        "Expected uvc_camera_enable_camera subscriber module"
    );
    assert!(
        output_dir
            .join("src/subscribed_services/uvc_camera_get_camera_info.rs")
            .exists(),
        "Expected uvc_camera_get_camera_info subscriber module"
    );
}
