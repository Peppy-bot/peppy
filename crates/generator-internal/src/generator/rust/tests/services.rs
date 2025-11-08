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
    error_msg: "string"
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
  error_msg: "string"
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
        rendered.contains("pub struct EnableCameraResponse"),
        &rendered,
        "expected response struct for exposed service"
    );
    assert_rendered!(
        rendered.contains("impl EnableCameraResponse {"),
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
        rendered.contains("error_msg: String"),
        &rendered,
        "expected string field in response struct"
    );
    assert_rendered!(
        rendered.contains("pub async fn handle_enable_camera_next_request<F>"),
        &rendered,
        "expected async service handler with handle_enable_camera_next_request naming"
    );
    assert_rendered!(
        rendered.contains("F: Fn(EnableCameraRequest) -> crate::Result<EnableCameraResponse>"),
        &rendered,
        "expected Fn trait bound for handler"
    );
    assert_rendered!(
        rendered.contains("pub struct EnableCameraRequest"),
        &rendered,
        "expected public request struct for enable_camera"
    );
    assert_rendered!(
        rendered.contains("pub enable: bool"),
        &rendered,
        "expected public request field for enable_camera"
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
        !rendered.contains("Self::enable_camera_handle_request_payload"),
        &rendered,
        "free function handler should invoke helper without a struct receiver"
    );
    assert_rendered!(
        rendered.contains("request_context.message.payload"),
        &rendered,
        "expected request payload to be pulled from the request context"
    );
    assert_rendered!(
        rendered.contains("fn enable_camera_deserialize_request"),
        &rendered,
        "expected request deserializer helper function"
    );
    assert_rendered!(
        rendered.contains("-> crate::Result<EnableCameraRequest>"),
        &rendered,
        "expected request deserializer to return request struct"
    );
    assert_rendered!(
        rendered.contains("handler(request_data)?"),
        &rendered,
        "expected handler callback invocation with deserialized parameter"
    );
    assert_rendered!(
        rendered.contains("let response = handler(request_data)?;"),
        &rendered,
        "expected handler result to be captured"
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
        rendered.contains("pub struct GetSystemStatusResponse"),
        &rendered,
        "expected response struct for service without request body"
    );
    assert_rendered!(
        !rendered.contains("GetSystemStatusRequest"),
        &rendered,
        "expected no request struct when request format is missing"
    );
    assert_rendered!(
        rendered.contains("F: Fn() -> crate::Result<GetSystemStatusResponse>"),
        &rendered,
        "expected handler to take no parameters"
    );
    assert_rendered!(
        rendered.contains("fn get_system_status_handle_request_payload"),
        &rendered,
        "expected helper function even without request payload"
    );
    assert_rendered!(
        !rendered.contains("fn get_system_status_deserialize_request"),
        &rendered,
        "expected no deserializer when there is no request schema"
    );
}

#[test]
fn expose_two_services() {
    let service1: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE).unwrap();
    let service2: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE2).unwrap();

    let mut generator = RustGenerator::new();
    generator.add_exposed_service(&service1).unwrap();
    generator.add_exposed_service(&service2).unwrap();

    let artifacts = generator.into_artifacts();

    assert_eq!(
        artifacts.len(),
        1,
        "expected a single generated artifact, got {}",
        artifacts.len()
    );

    let rendered = artifacts
        .into_iter()
        .next()
        .map(|artifact| artifact.code_output)
        .expect("artifact is present");

    assert_rendered!(
        rendered.contains("pub async fn handle_enable_camera_next_request<F>("),
        &rendered,
        "expected async handler for `enable_camera`"
    );
    assert_rendered!(
        rendered.contains("F: Fn(EnableCameraRequest) -> crate::Result<EnableCameraResponse>"),
        &rendered,
        "expected handler signature for `enable_camera`"
    );
    assert_rendered!(
        rendered.contains("pub struct EnableCameraRequest"),
        &rendered,
        "expected public request struct for `enable_camera`"
    );
    assert_rendered!(
        rendered.contains("pub enable: bool"),
        &rendered,
        "expected public request field for `enable_camera`"
    );
    assert_rendered!(
        rendered.contains("fn enable_camera_deserialize_request("),
        &rendered,
        "expected request deserializer helper for `enable_camera`"
    );
    assert_rendered!(
        rendered.contains("-> crate::Result<EnableCameraRequest>"),
        &rendered,
        "expected request deserializer to return request struct for `enable_camera`"
    );
    assert_rendered!(
        rendered.contains("pub struct EnableCameraResponse"),
        &rendered,
        "expected response struct for `enable_camera`"
    );
    assert_rendered!(
        rendered.contains("pub async fn handle_get_lidar_info_next_request<F>("),
        &rendered,
        "expected async handler for `get_lidar_info`"
    );
    assert_rendered!(
        rendered.contains("F: Fn(GetLidarInfoRequest) -> crate::Result<()>"),
        &rendered,
        "expected handler signature for `get_lidar_info`"
    );
    assert_rendered!(
        rendered.contains("pub struct GetLidarInfoRequest"),
        &rendered,
        "expected public request struct for `get_lidar_info`"
    );
    assert_rendered!(
        !rendered.contains("GetLidarInfoResponse"),
        &rendered,
        "expected no response struct for `get_lidar_info`"
    );
    assert_rendered!(
        rendered.contains("pub channels: String"),
        &rendered,
        "expected public request field for `get_lidar_info`"
    );
    assert_rendered!(
        rendered.contains("fn get_lidar_info_deserialize_request("),
        &rendered,
        "expected request deserializer helper for `get_lidar_info`"
    );
}

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
        rendered.contains("pub struct EnableCameraResponse"),
        &rendered,
        "expected response struct"
    );
    assert_rendered!(
        rendered.contains("enabled: bool"),
        &rendered,
        "expected response bool field"
    );
    assert_rendered!(
        rendered.contains("error_msg: String"),
        &rendered,
        "expected response string field"
    );
    assert_rendered!(
        rendered.contains("pub async fn poll_uvc_camera_enable_camera("),
        &rendered,
        "expected async poll helper signature"
    );
    assert_rendered!(
        rendered.contains("messenger: &crate::Messenger"),
        &rendered,
        "expected messenger parameter"
    );
    assert_rendered!(
        rendered.contains("-> crate::Result<EnableCameraResponse>"),
        &rendered,
        "expected result return type"
    );
    assert_rendered!(
        rendered.contains("let service_name = \"enable_camera\";"),
        &rendered,
        "expected service name literal"
    );
    assert_rendered!(
        rendered.contains("capnp::message::Builder::new_default"),
        &rendered,
        "expected capnp message builder"
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
        rendered.contains("context: String::from(\"poll_uvc_camera_enable_camera\")"),
        &rendered,
        "expected request serialization error context"
    );
    assert_rendered!(
        rendered.contains("peppylib::ServiceMessenger::poll("),
        &rendered,
        "expected poll helper invocation"
    );
    assert_rendered!(
        rendered.contains("std::time::Duration::from_secs(3)"),
        &rendered,
        "expected poll timeout constant"
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
        rendered.contains("Ok(EnableCameraResponse {"),
        &rendered,
        "expected response construction"
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
        rendered.contains("pub struct EnableCameraResponse"),
        rendered,
        "expected response struct for `enable_camera`"
    );
    assert_rendered!(
        rendered.contains("pub async fn poll_uvc_camera_enable_camera("),
        rendered,
        "expected poll helper function for `enable_camera`"
    );
    assert_rendered!(
        rendered.contains("enable: bool"),
        rendered,
        "expected request parameter for `enable_camera`"
    );
    assert_rendered!(
        rendered.contains("-> crate::Result<EnableCameraResponse>"),
        rendered,
        "expected return type for `enable_camera` poll helper"
    );
    assert_rendered!(
        rendered.contains("root.set_enable(enable);"),
        rendered,
        "expected request serialization for `enable_camera`"
    );
    assert_rendered!(
        rendered.contains("peppylib::ServiceMessenger::poll("),
        rendered,
        "expected poll invocation for `enable_camera`"
    );
    assert_rendered!(
        rendered.contains("capnp::serialize::read_message"),
        rendered,
        "expected response deserialization for `enable_camera`"
    );
    assert_rendered!(
        rendered.contains("Ok(EnableCameraResponse {"),
        rendered,
        "expected response construction for `enable_camera`"
    );

    assert_rendered!(
        rendered.contains("pub struct GetCameraInfoResponse"),
        rendered,
        "expected response struct for `get_camera_info`"
    );
    assert_rendered!(
        rendered.contains("card_type: String"),
        rendered,
        "expected response field for `card_type`"
    );
    assert_rendered!(
        rendered.contains("interval: String"),
        rendered,
        "expected response field for `interval`"
    );
    assert_rendered!(
        rendered.contains("size: String"),
        rendered,
        "expected response field for `size`"
    );
    assert_rendered!(
        rendered.contains("-> crate::Result<GetCameraInfoResponse>"),
        rendered,
        "expected return type for `get_camera_info` poll helper"
    );
    assert_rendered!(
        rendered.contains("peppylib::ServiceMessenger::poll("),
        rendered,
        "expected poll invocation for `get_camera_info`"
    );
    assert_rendered!(
        rendered.contains("capnp::serialize::read_message"),
        rendered,
        "expected response deserialization for `get_camera_info`"
    );
    assert_rendered!(
        rendered.contains("Ok(GetCameraInfoResponse {"),
        rendered,
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
        !rendered.contains("pub struct Subscribes;"),
        rendered,
        "poll helpers should be emitted as free functions"
    );
    assert_rendered!(
        !rendered.contains("GetCameraInfoResponse"),
        rendered,
        "expected no response struct when response format is missing"
    );
    assert_rendered!(
        rendered.contains("let _ = peppylib::ServiceMessenger::poll("),
        rendered,
        "expected poll invocation that discards response bytes"
    );
}

/// This is a long running test
#[test]
fn compile_lib_with_exposed_services_artifact() {
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
        "Expected lib.rs to exist so `peppygen::services` is reachable"
    );
    let lib_contents = std::fs::read_to_string(&lib_rs).expect("failed to read generated lib.rs");
    assert!(
        lib_contents.contains("pub mod services;"),
        "Expected generated lib.rs to re-export the `services` module, got:\n{}",
        lib_contents
    );

    let services_mod = output_dir.join("src/services.rs");
    assert!(
        services_mod.exists(),
        "Expected services module file to exist so `peppygen::services::<module>` resolves"
    );
    let services_contents =
        std::fs::read_to_string(&services_mod).expect("failed to read services module");
    assert!(
        services_contents.contains("pub mod exposers;"),
        "Expected services module to declare generated `exposers` module, got:\n{}",
        services_contents
    );
    assert!(
        services_contents.contains("pub mod subscribers;"),
        "Expected services module to declare subscribed `subscribers` module, got:\n{}",
        services_contents
    );
    assert!(
        !services_contents.contains("pub use exposers::*;"),
        "Services module should not glob re-export exposers to avoid duplicate type names, got:\n{}",
        services_contents
    );
    assert!(
        !services_contents.contains("pub use subscribers::*;"),
        "Services module should not glob re-export subscribers to avoid duplicate type names, got:\n{}",
        services_contents
    );
    let service_modules: Vec<String> = services_contents
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            let module_name = trimmed
                .strip_prefix("mod ")
                .or_else(|| trimmed.strip_prefix("pub mod "));
            module_name.map(|rest| rest.trim_end_matches(';').trim().to_string())
        })
        .collect();
    assert!(
        !service_modules.is_empty(),
        "Expected services module to expose at least one generated service module, got:\n{}",
        services_contents
    );
    assert_eq!(
        service_modules.len(),
        2,
        "Expected two generated service modules, got {:?}",
        service_modules
    );

    assert!(
        service_modules.iter().any(|module| module == "exposers"),
        "Expected exposers service module, got {:?}",
        service_modules
    );
    assert!(
        service_modules.iter().any(|module| module == "subscribers"),
        "Expected subscribers service module, got {:?}",
        service_modules
    );

    let exposers_module_path = output_dir.join("src/services/exposers.rs");
    assert!(
        exposers_module_path.exists(),
        "Expected generated exposers service module at {:?}",
        exposers_module_path
    );
    let module_contents = std::fs::read_to_string(&exposers_module_path)
        .expect("failed to read generated service module");
    let subscribers_module_path = output_dir.join("src/services/subscribers.rs");
    assert!(
        subscribers_module_path.exists(),
        "Expected generated subscribers service module at {:?}",
        subscribers_module_path
    );
    assert!(
        module_contents.contains("pub struct EnableCameraResponse"),
        "Expected generated service module to define response struct, got:\n{}",
        module_contents
    );
    assert!(
        module_contents.contains("impl EnableCameraResponse {"),
        "Expected generated service module to define response struct impl, got:\n{}",
        module_contents
    );
    assert!(
        module_contents.contains("enabled: bool"),
        "Expected generated response struct to include `enabled` field, got:\n{}",
        module_contents
    );
    assert!(
        module_contents.contains("error_msg: String"),
        "Expected generated response struct to include `error_msg` field, got:\n{}",
        module_contents
    );
    assert!(
        module_contents.contains("pub async fn handle_enable_camera_next_request<F>("),
        "Expected generated service module to expose async handler, got:\n{}",
        module_contents
    );
    assert!(
        module_contents.contains("messenger: &crate::Messenger"),
        "Expected generated handler to accept messenger reference, got:\n{}",
        module_contents
    );
    assert!(
        module_contents.contains("peppylib::ServiceMessenger::listen("),
        "Expected generated handler to initialize ServiceMessenger listener, got:\n{}",
        module_contents
    );
    assert!(
        module_contents.contains(".handle_next_request(move |request_context|"),
        "Expected generated handler to use ServiceMessenger::handle_next_request, got:\n{}",
        module_contents
    );
    assert!(
        module_contents.contains("request_context.message.payload"),
        "Expected generated handler to deserialize request payload from the context, got:\n{}",
        module_contents
    );
    assert!(
        module_contents.contains("enable_camera_deserialize_request"),
        "Expected generated helper to deserialize request payload, got:\n{}",
        module_contents
    );
    assert!(
        module_contents.contains("bytes::Bytes::from(buffer)"),
        "Expected generated handler to serialize response payload, got:\n{}",
        module_contents
    );
    assert!(
        module_contents.contains("capnp::serialize::read_message"),
        "Expected generated module to deserialize requests using capnp, got:\n{}",
        module_contents
    );
    assert!(
        module_contents.contains("pub async fn handle_get_lidar_info_next_request<F>("),
        "Expected generated module to expose handler for get_lidar_info, got:\n{}",
        module_contents
    );
    assert!(
        module_contents.contains("pub struct GetLidarInfoRequest"),
        "Expected generated module to define request struct for get_lidar_info, got:\n{}",
        module_contents
    );
    assert!(
        !module_contents.contains("pub struct GetLidarInfoResponse"),
        "Expected no response struct for get_lidar_info, got:\n{}",
        module_contents
    );
    assert!(
        module_contents.contains("fn get_lidar_info_handle_request_payload"),
        "Expected helper for get_lidar_info payload handling, got:\n{}",
        module_contents
    );
}
