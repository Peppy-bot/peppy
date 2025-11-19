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

    assert_rendered!(
        rendered.contains("pub struct Response"),
        &rendered,
        "expected response struct for exposed service"
    );
    assert_rendered!(
        rendered.contains("impl Response {"),
        &rendered,
        "expected response struct impl block"
    );
    assert_rendered!(
        rendered.contains("pub fn new"),
        &rendered,
        "expected response constructor"
    );
    assert_rendered!(
        rendered.contains("enabled: bool"),
        &rendered,
        "expected bool field in response struct"
    );
    assert_rendered!(
        rendered.contains("error_msg: Option<String>"),
        &rendered,
        "expected optional string field in response struct"
    );
    assert_rendered!(
        rendered.contains("pub async fn handle_next_request<F>"),
        &rendered,
        "expected async service handler with generic naming"
    );
    assert_rendered!(
        rendered.contains("F: Fn(String, Request) -> crate::Result<Response>"),
        &rendered,
        "expected Fn trait bound for handler that includes instance_id"
    );
    assert_rendered!(
        rendered.contains("pub struct Request"),
        &rendered,
        "expected public request struct for enable_camera"
    );
    assert_rendered!(
        rendered.contains("pub enable: bool"),
        &rendered,
        "expected public request field for enable_camera"
    );
    assert_rendered!(
        rendered.contains("impl Request {"),
        &rendered,
        "expected request struct impl block"
    );
    assert_rendered!(
        rendered.contains("peppylib::ServiceMessenger::listen"),
        &rendered,
        "expected service listen call through peppylib helper"
    );
    assert_rendered!(
        rendered.contains(".handle_next_request(move |request_context|"),
        &rendered,
        "expected generated handler to schedule request handling through ServiceMessenger"
    );
    assert_rendered!(
        rendered.contains("request_context.message.payload"),
        &rendered,
        "expected request payload to be pulled from the request context"
    );
    assert_rendered!(
        rendered.contains("fn deserialize_request"),
        &rendered,
        "expected request deserializer helper function"
    );
    assert_rendered!(
        rendered.contains("-> crate::Result<(String, Request)>"),
        &rendered,
        "expected request deserializer to return instance_id with request struct"
    );
    assert_rendered!(
        rendered.contains("fn handle_request_payload"),
        &rendered,
        "expected generic helper for handling request payloads"
    );
    assert_rendered!(
        rendered.contains("let (instance_id, request_data) = deserialize_request(payload)?;"),
        &rendered,
        "expected helper to destructure instance_id from deserializer"
    );
    assert_rendered!(
        rendered.contains("handler(instance_id, request_data)?"),
        &rendered,
        "expected handler callback invocation with instance_id parameter"
    );
    assert_rendered!(
        rendered.contains("let response = handler(instance_id, request_data)?;"),
        &rendered,
        "expected handler result to be captured with instance context"
    );
    assert_rendered!(
        rendered.contains("bytes::Bytes::from(buffer)"),
        &rendered,
        "expected response serialization to produce bytes"
    );
    assert_rendered!(
        rendered.contains("capnp::serialize::read_message"),
        &rendered,
        "expected request deserialization"
    );
    assert_rendered!(
        rendered
            .contains("crate::capnp::enable_camera_message_capnp::enable_camera_message::Reader"),
        &rendered,
        "expected service request schema reader"
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

    assert_rendered!(
        rendered.contains("pub struct Response"),
        &rendered,
        "expected response struct for service without request body"
    );
    assert_rendered!(
        rendered.contains("impl Response {"),
        &rendered,
        "expected response impl block for service without request body"
    );
    assert_rendered!(
        rendered.contains("F: Fn(String) -> crate::Result<Response>"),
        &rendered,
        "expected handler to take only instance_id parameter"
    );
    assert_rendered!(
        rendered.contains("fn handle_request_payload"),
        &rendered,
        "expected generic helper function even without request payload"
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

    assert_rendered!(
        enable_rendered.contains("let service_name = \"enable_camera\";"),
        enable_rendered,
        "expected enable_camera service name literal"
    );
    assert_rendered!(
        enable_rendered.contains("pub struct Request"),
        enable_rendered,
        "expected request struct for enable_camera"
    );
    assert_rendered!(
        enable_rendered.contains("impl Request {"),
        enable_rendered,
        "expected request struct impl for enable_camera"
    );
    assert_rendered!(
        enable_rendered.contains("pub struct Response"),
        enable_rendered,
        "expected response struct for enable_camera"
    );
    assert_rendered!(
        enable_rendered.contains("pub async fn handle_next_request<F>("),
        enable_rendered,
        "expected async handler for enable_camera"
    );
    assert_rendered!(
        enable_rendered.contains("F: Fn(String, Request) -> crate::Result<Response>"),
        enable_rendered,
        "expected handler signature for enable_camera to include instance_id"
    );
    assert_rendered!(
        enable_rendered.contains("fn deserialize_request("),
        enable_rendered,
        "expected request deserializer for enable_camera"
    );
    assert_rendered!(
        enable_rendered.contains("fn handle_request_payload"),
        enable_rendered,
        "expected payload handler for enable_camera"
    );

    assert_rendered!(
        lidar_rendered.contains("let service_name = \"get_lidar_info\";"),
        lidar_rendered,
        "expected get_lidar_info service name literal"
    );
    assert_rendered!(
        lidar_rendered.contains("pub struct Request"),
        lidar_rendered,
        "expected request struct for get_lidar_info"
    );
    assert_rendered!(
        lidar_rendered.contains("impl Request {"),
        lidar_rendered,
        "expected request struct impl for get_lidar_info"
    );
    assert_rendered!(
        !lidar_rendered.contains("pub struct Response"),
        lidar_rendered,
        "expected get_lidar_info to omit response struct"
    );
    assert_rendered!(
        lidar_rendered.contains("F: Fn(String, Request) -> crate::Result<()>"),
        lidar_rendered,
        "expected handler signature for get_lidar_info to include instance_id"
    );
    assert_rendered!(
        lidar_rendered.contains("fn deserialize_request("),
        lidar_rendered,
        "expected request deserializer for get_lidar_info"
    );
    assert_rendered!(
        lidar_rendered.contains("fn handle_request_payload"),
        lidar_rendered,
        "expected payload handler for get_lidar_info"
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

    assert_rendered!(
        rendered.contains("#[derive(Debug, Clone)]"),
        &rendered,
        "expected response struct derives"
    );
    assert_rendered!(
        rendered.contains("pub struct Response"),
        &rendered,
        "expected generic response struct"
    );
    assert_rendered!(
        rendered.contains("impl Response {"),
        &rendered,
        "expected response implementation block"
    );
    assert_rendered!(
        rendered.contains("enabled: bool"),
        &rendered,
        "expected response bool field"
    );
    assert_rendered!(
        rendered.contains("error_msg: Option<String>"),
        &rendered,
        "expected response optional string field"
    );
    assert_rendered!(
        rendered.contains("pub struct Request"),
        &rendered,
        "expected request struct"
    );
    assert_rendered!(
        rendered.contains("pub enable: bool"),
        &rendered,
        "expected request struct field"
    );
    assert_rendered!(
        rendered.contains("impl Request {"),
        &rendered,
        "expected request struct impl block"
    );
    assert_rendered!(
        rendered.contains("pub async fn poll("),
        &rendered,
        "expected async poll helper signature"
    );
    assert_rendered!(
        rendered.contains("messenger: &crate::Messenger"),
        &rendered,
        "expected messenger parameter"
    );
    assert_rendered!(
        rendered.contains("timeout: std::time::Duration"),
        &rendered,
        "expected timeout parameter"
    );
    assert_rendered!(
        rendered.contains("target_instance_id: Option<String>"),
        &rendered,
        "expected target_instance_id parameter"
    );
    assert_rendered!(
        rendered.contains("request: Request"),
        &rendered,
        "expected request parameter"
    );
    assert_rendered!(
        rendered.contains("-> crate::Result<Response>"),
        &rendered,
        "expected generic result return type"
    );
    assert_rendered!(
        rendered.contains("let service_name = \"enable_camera\";"),
        &rendered,
        "expected service name literal"
    );
    assert_rendered!(
        rendered.contains("let service_name = match target_instance_id {"),
        &rendered,
        "expected service name matching"
    );
    assert_rendered!(
        rendered.contains("capnp::message::Builder::new_default"),
        &rendered,
        "expected capnp message builder"
    );
    assert_rendered!(
        rendered.contains("let enable = request.enable;"),
        &rendered,
        "expected request field unpacking"
    );
    assert_rendered!(
        rendered.contains("root.set_enable(enable);"),
        &rendered,
        "expected request serialization assignment"
    );
    assert_rendered!(
        rendered.contains("let mut buffer = Vec::new();"),
        &rendered,
        "expected request serialization buffer allocation"
    );
    assert_rendered!(
        rendered.contains("capnp::serialize::write_message(&mut buffer, &message)"),
        &rendered,
        "expected request serialization using capnp"
    );
    assert_rendered!(
        rendered.contains("context: String::from(\"poll uvc_camera enable_camera\")"),
        &rendered,
        "expected request serialization error context"
    );
    assert_rendered!(
        rendered.contains("peppylib::ServiceMessenger::poll("),
        &rendered,
        "expected poll helper invocation"
    );
    assert_rendered!(
        rendered.contains("timeout,\n        )\n        .await?"),
        &rendered,
        "expected poll timeout to use function parameter"
    );
    assert_rendered!(
        rendered.contains("capnp::serialize::read_message"),
        &rendered,
        "expected response deserialization"
    );
    assert_rendered!(
        rendered.contains("root.reborrow().get_enabled()"),
        &rendered,
        "expected response field reader for bool"
    );
    assert_rendered!(
        rendered.contains(".get_error_msg()"),
        &rendered,
        "expected response field reader for string"
    );
    assert_rendered!(
        rendered.contains("context: String::from(\"uvc_camera enable_camera response\")"),
        &rendered,
        "expected response deserialization error context"
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
    let enable_artifact = artifacts
        .iter()
        .find(|artifact| artifact.node_name.as_str() == enable_module_name.as_str())
        .unwrap_or_else(|| panic!("expected {enable_module_name} artifact to be generated"));
    let camera_module_name = subscribed_service_module_name(&service2);
    let camera_artifact = artifacts
        .iter()
        .find(|artifact| artifact.node_name.as_str() == camera_module_name.as_str())
        .unwrap_or_else(|| panic!("expected {camera_module_name} artifact to be generated"));
    let enable_rendered = &enable_artifact.code_output;
    let camera_rendered = &camera_artifact.code_output;

    assert_rendered!(
        enable_rendered.contains("pub struct Response"),
        enable_rendered,
        "expected response struct for `enable_camera`"
    );
    assert_rendered!(
        enable_rendered.contains("pub async fn poll("),
        enable_rendered,
        "expected poll helper function for `enable_camera`"
    );
    assert_rendered!(
        enable_rendered.contains("enable: bool"),
        enable_rendered,
        "expected request parameter for `enable_camera`"
    );
    assert_rendered!(
        enable_rendered.contains("-> crate::Result<Response>"),
        enable_rendered,
        "expected return type for `enable_camera` poll helper"
    );
    assert_rendered!(
        enable_rendered.contains("root.set_enable(enable);"),
        enable_rendered,
        "expected request serialization for `enable_camera`"
    );
    assert_rendered!(
        enable_rendered.contains("peppylib::ServiceMessenger::poll("),
        enable_rendered,
        "expected poll invocation for `enable_camera`"
    );
    assert_rendered!(
        enable_rendered.contains("context: String::from(\"poll uvc_camera enable_camera\")"),
        enable_rendered,
        "expected request context for `enable_camera`"
    );
    assert_rendered!(
        enable_rendered.contains("capnp::serialize::read_message"),
        enable_rendered,
        "expected response deserialization for `enable_camera`"
    );
    assert_rendered!(
        enable_rendered.contains("context: String::from(\"uvc_camera enable_camera response\")"),
        enable_rendered,
        "expected response context for `enable_camera`"
    );
    assert_rendered!(
        enable_rendered.contains("Ok(Response {"),
        enable_rendered,
        "expected response construction for `enable_camera`"
    );

    assert_rendered!(
        camera_rendered.contains("pub struct Response"),
        camera_rendered,
        "expected response struct for `get_camera_info`"
    );
    assert_rendered!(
        camera_rendered.contains("card_type: String"),
        camera_rendered,
        "expected response field for `card_type`"
    );
    assert_rendered!(
        camera_rendered.contains("interval: String"),
        camera_rendered,
        "expected response field for `interval`"
    );
    assert_rendered!(
        camera_rendered.contains("size: String"),
        camera_rendered,
        "expected response field for `size`"
    );
    assert_rendered!(
        camera_rendered.contains("pub async fn poll("),
        camera_rendered,
        "expected poll helper for `get_camera_info`"
    );
    assert_rendered!(
        camera_rendered.contains("-> crate::Result<Response>"),
        camera_rendered,
        "expected return type for `get_camera_info` poll helper"
    );
    assert_rendered!(
        camera_rendered.contains("peppylib::ServiceMessenger::poll("),
        camera_rendered,
        "expected poll invocation for `get_camera_info`"
    );
    assert_rendered!(
        camera_rendered.contains("capnp::serialize::read_message"),
        camera_rendered,
        "expected response deserialization for `get_camera_info`"
    );
    assert_rendered!(
        camera_rendered.contains("context: String::from(\"uvc_camera get_camera_info response\")"),
        camera_rendered,
        "expected response context for `get_camera_info`"
    );
    assert_rendered!(
        camera_rendered.contains("Ok(Response {"),
        camera_rendered,
        "expected response construction for `get_camera_info`"
    );
}

#[test]
fn subscribed_service_without_response_payload() {
    let service = r#"
        {
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

/// This is a long running test
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

    let (mut generator, output_dir, user_node) = init_test_env(&temp_dir);
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

    assert!(
        output_dir.join("Cargo.toml").exists(),
        "Expected Cargo.toml to be generated in the temporary crate directory"
    );
    assert!(
        !output_dir.join(PEPPY_NODE_CONFIG_FILE).exists(),
        "Generated crate should not keep a copy of the node configuration file"
    );

    let lib_rs = output_dir.join("src/lib.rs");
    assert!(
        lib_rs.exists(),
        "Expected lib.rs to exist so generated service modules are reachable"
    );
    let lib_contents = std::fs::read_to_string(&lib_rs).expect("failed to read generated lib.rs");
    assert!(
        lib_contents.contains("pub mod exposed_services;"),
        "Expected generated lib.rs to re-export the `exposed_services` module, got:\n{}",
        lib_contents
    );
    assert!(
        lib_contents.contains("pub mod subscribed_services;"),
        "Expected generated lib.rs to re-export the `subscribed_services` module, got:\n{}",
        lib_contents
    );

    let exposed_services_mod = output_dir.join("src/exposed_services.rs");
    assert!(
        exposed_services_mod.exists(),
        "Expected exposed services module file to exist so `peppygen::exposed_services::<service>` resolves"
    );
    let exposed_services_contents = std::fs::read_to_string(&exposed_services_mod)
        .expect("failed to read exposed_services module");
    assert!(
        exposed_services_contents.contains("pub mod enable_camera;"),
        "Expected exposed services module to declare the `enable_camera` service, got:\n{}",
        exposed_services_contents
    );
    assert!(
        exposed_services_contents.contains("pub mod get_lidar_info;"),
        "Expected exposed services module to declare the `get_lidar_info` service, got:\n{}",
        exposed_services_contents
    );

    let subscribed_services_mod = output_dir.join("src/subscribed_services.rs");
    assert!(
        subscribed_services_mod.exists(),
        "Expected subscribed services module file to exist so `peppygen::subscribed_services::<service>` resolves"
    );
    let subscribed_services_contents = std::fs::read_to_string(&subscribed_services_mod)
        .expect("failed to read subscribed_services module");
    assert!(
        subscribed_services_contents.contains("pub mod uvc_camera_enable_camera;"),
        "Expected subscribed services module to declare the `uvc_camera_enable_camera` client, got:\n{}",
        subscribed_services_contents
    );
    assert!(
        subscribed_services_contents.contains("pub mod uvc_camera_get_camera_info;"),
        "Expected subscribed services module to declare the `uvc_camera_get_camera_info` client, got:\n{}",
        subscribed_services_contents
    );

    let enable_camera_module_path = output_dir.join("src/exposed_services/enable_camera.rs");
    assert!(
        enable_camera_module_path.exists(),
        "Expected generated enable_camera service module at {:?}",
        enable_camera_module_path
    );
    let enable_module_contents = std::fs::read_to_string(&enable_camera_module_path)
        .expect("failed to read enable_camera service module");

    let get_lidar_module_path = output_dir.join("src/exposed_services/get_lidar_info.rs");
    assert!(
        get_lidar_module_path.exists(),
        "Expected generated get_lidar_info service module at {:?}",
        get_lidar_module_path
    );
    let lidar_module_contents = std::fs::read_to_string(&get_lidar_module_path)
        .expect("failed to read get_lidar_info service module");

    let subscriber_module_impl_path =
        output_dir.join("src/subscribed_services/uvc_camera_enable_camera.rs");
    assert!(
        subscriber_module_impl_path.exists(),
        "Expected enable_camera subscriber module implementation at {:?}",
        subscriber_module_impl_path
    );
    let subscriber_module_two_path =
        output_dir.join("src/subscribed_services/uvc_camera_get_camera_info.rs");
    assert!(
        subscriber_module_two_path.exists(),
        "Expected get_camera_info subscriber module implementation at {:?}",
        subscriber_module_two_path
    );
    let subscriber_module_contents = std::fs::read_to_string(&subscriber_module_impl_path)
        .expect("failed to read enable_camera subscriber module");
    assert!(
        subscriber_module_contents.contains("pub struct Response"),
        "Enable_camera subscriber module should define generic Response struct, got:\n{}",
        subscriber_module_contents
    );
    assert!(
        subscriber_module_contents.contains("pub async fn poll("),
        "Enable_camera subscriber module should define poll helper, got:\n{}",
        subscriber_module_contents
    );
    assert!(
        subscriber_module_contents
            .contains("context: String::from(\"poll uvc_camera enable_camera\")"),
        "Enable_camera subscriber module should use simplified request context, got:\n{}",
        subscriber_module_contents
    );
    assert!(
        subscriber_module_contents
            .contains("context: String::from(\"uvc_camera enable_camera response\")"),
        "Enable_camera subscriber module should use simplified response context, got:\n{}",
        subscriber_module_contents
    );
    assert!(
        enable_module_contents.contains("pub struct Response"),
        "Expected generated service module to define response struct, got:\n{}",
        enable_module_contents
    );
    assert!(
        enable_module_contents.contains("impl Response {"),
        "Expected generated service module to define response struct impl, got:\n{}",
        enable_module_contents
    );
    assert!(
        enable_module_contents.contains("enabled: bool"),
        "Expected generated response struct to include `enabled` field, got:\n{}",
        enable_module_contents
    );
    assert!(
        enable_module_contents.contains("error_msg: Option<String>"),
        "Expected generated response struct to include `error_msg` field, got:\n{}",
        enable_module_contents
    );
    assert!(
        enable_module_contents.contains("pub struct Request"),
        "Expected generated service module to define request struct, got:\n{}",
        enable_module_contents
    );
    assert!(
        enable_module_contents.contains("impl Request {"),
        "Expected generated service module to define request struct impl, got:\n{}",
        enable_module_contents
    );
    assert!(
        enable_module_contents.contains("pub async fn handle_next_request<F>("),
        "Expected generated service module to expose async handler, got:\n{}",
        enable_module_contents
    );
    assert!(
        enable_module_contents.contains("messenger: &crate::Messenger"),
        "Expected generated handler to accept messenger reference, got:\n{}",
        enable_module_contents
    );
    assert!(
        enable_module_contents.contains("peppylib::ServiceMessenger::listen("),
        "Expected generated handler to initialize ServiceMessenger listener, got:\n{}",
        enable_module_contents
    );
    assert!(
        enable_module_contents.contains(".handle_next_request(move |request_context|"),
        "Expected generated handler to use ServiceMessenger::handle_next_request, got:\n{}",
        enable_module_contents
    );
    assert!(
        enable_module_contents.contains("request_context.message.payload"),
        "Expected generated handler to deserialize request payload from the context, got:\n{}",
        enable_module_contents
    );
    assert!(
        enable_module_contents.contains("fn deserialize_request"),
        "Expected generated helper to deserialize request payload, got:\n{}",
        enable_module_contents
    );
    assert!(
        enable_module_contents.contains("fn handle_request_payload"),
        "Expected generated helper to convert handler output to bytes, got:\n{}",
        enable_module_contents
    );
    assert!(
        enable_module_contents.contains("bytes::Bytes::from(buffer)"),
        "Expected generated handler to serialize response payload, got:\n{}",
        enable_module_contents
    );
    assert!(
        enable_module_contents.contains("capnp::serialize::read_message"),
        "Expected generated module to deserialize requests using capnp, got:\n{}",
        enable_module_contents
    );
    assert!(
        lidar_module_contents.contains("pub async fn handle_next_request<F>("),
        "Expected generated module to expose handler for get_lidar_info, got:\n{}",
        lidar_module_contents
    );
    assert!(
        lidar_module_contents.contains("pub struct Request"),
        "Expected generated module to define request struct for get_lidar_info, got:\n{}",
        lidar_module_contents
    );
    assert!(
        lidar_module_contents.contains("impl Request {"),
        "Expected generated module to define request struct impl for get_lidar_info, got:\n{}",
        lidar_module_contents
    );
    assert!(
        !lidar_module_contents.contains("pub struct Response"),
        "Expected no response struct for get_lidar_info, got:\n{}",
        lidar_module_contents
    );
    assert!(
        lidar_module_contents.contains("fn handle_request_payload"),
        "Expected helper for get_lidar_info payload handling, got:\n{}",
        lidar_module_contents
    );
}
