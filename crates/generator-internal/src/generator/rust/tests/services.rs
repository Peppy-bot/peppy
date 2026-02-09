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

const EMPTY_MESSAGE_FORMAT: &str = r#"{}"#;

/// In the case of a service, an "exposed" service is an entity that accept incoming messages
#[test]
fn expose_service() {
    let service: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE).unwrap();

    let mut generator = RustGenerator::new();
    generator.add_exposed_service(&service).unwrap();
    let artifacts = render_artifacts(generator.into_artifacts());
    assert_eq!(
        artifacts.len(),
        1,
        "expected a single generated artifact, got {}",
        artifacts.len()
    );
    let rendered = artifacts.into_iter().next().expect("artifact is present");

    // Response struct
    assert_contains_all(
        &rendered,
        &[
            "pub struct Response",
            "enabled: bool",
            "error_msg: Option<String>",
        ],
    );

    // Request structs
    assert_contains_all(
        &rendered,
        &[
            "pub struct RequestData",
            "pub struct Request",
            "pub instance_id: String",
            "pub data: RequestData",
        ],
    );

    // Handler signature
    assert_contains_all(
        &rendered,
        &[
            "const SERVICE_NAME: &str = \"enable_camera\";",
            "pub async fn handle_next_request<F>",
            "F: Fn(Request) -> crate::Result<Response>",
        ],
    );

    // Request processing
    assert_contains_all(
        &rendered,
        &[
            "fn deserialize_request(payload: &[u8]) -> crate::Result<RequestData>",
            "fn handle_request_payload<F>(",
        ],
    );

    // Messenger integration
    assert_contains_all(
        &rendered,
        &[
            "peppylib::ServiceMessenger::listen",
            ".handle_next_request(move |request_context|",
        ],
    );
}

#[test]
fn expose_service_without_request_body() {
    let service: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE3).unwrap();

    let mut generator = RustGenerator::new();
    generator.add_exposed_service(&service).unwrap();
    let rendered = render_artifacts(generator.into_artifacts())
        .into_iter()
        .next()
        .expect("artifact is present");

    // Service without request body should still have Request struct for metadata
    assert_contains_all(
        &rendered,
        &[
            "pub struct Request",
            "pub instance_id: String",
            "let request = Request {",
        ],
    );

    // But no RequestData struct
    assert_rendered!(
        !rendered.contains("pub struct RequestData"),
        &rendered,
        "expected no RequestData struct when there is no request body"
    );

    // And no request payload deserialization when there is no request body
    assert_rendered!(
        !rendered.contains("fn deserialize_request("),
        &rendered,
        "expected no request deserializer when there is no request body"
    );
    assert_rendered!(
        !rendered.contains("capnp::serialize::read_message"),
        &rendered,
        "expected no Cap'n Proto parsing when there is no request body"
    );
}

#[test]
fn expose_two_services() {
    let service1: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE).unwrap();
    let service2: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE2).unwrap();

    let mut generator = RustGenerator::new();
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
    assert_artifact_contains(&artifacts, "const SERVICE_NAME: &str = \"enable_camera\";");
    assert_artifact_contains(&artifacts, "const SERVICE_NAME: &str = \"get_lidar_info\";");

    // enable_camera has response, get_lidar_info does not
    assert_artifact_contains(&artifacts, "F: Fn(Request) -> crate::Result<Response>");
    assert_artifact_contains(&artifacts, "F: Fn(Request) -> crate::Result<()>");
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
    assert_contains_all(
        &rendered,
        &[
            "const NODE_NAME: &str = \"uvc_camera\";",
            "const SERVICE_NAME: &str = \"enable_camera\";",
        ],
    );

    // Response structs
    assert_contains_all(
        &rendered,
        &[
            "pub struct ResponseData",
            "pub struct Response",
            "pub data: ResponseData",
        ],
    );

    // Request struct
    assert_contains_all(&rendered, &["pub struct Request", "pub enable: bool"]);

    // Poll function signature
    assert_contains_all(
        &rendered,
        &[
            "pub async fn poll(",
            "target_master_node: Option<&str>",
            "target_instance_id: Option<&str>",
            "-> crate::Result<Response>",
        ],
    );

    // Request serialization and messenger integration
    assert_contains_all(
        &rendered,
        &[
            "root.set_enable(enable);",
            "peppylib::ServiceMessenger::poll(",
            "fn deserialize_response(payload: &[u8]) -> crate::Result<ResponseData>",
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

    let mut generator = RustGenerator::new();
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

    // Verify each topic gets distinct artifact with correct service name
    assert_artifact_contains(&artifacts, "const SERVICE_NAME: &str = \"enable_camera\";");
    assert_artifact_contains(
        &artifacts,
        "const SERVICE_NAME: &str = \"get_camera_info\";",
    );

    // Verify both reference the same source node
    for rendered in &artifacts {
        assert_contains_all(rendered, &["const NODE_NAME: &str = \"uvc_camera\";"]);
    }

    // get_camera_info has specific response fields
    assert_artifact_contains(&artifacts, "card_type: String");
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
    let empty_format: MessageFormat = serde_json5::from_str(EMPTY_MESSAGE_FORMAT).unwrap();

    let mut generator = RustGenerator::new();
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

    assert_artifact_contains(&artifacts, "let _ = peppylib::ServiceMessenger::poll(");
}

/// Checks for clippy warnings when there is only one exposed service without a request body.
#[test]
fn clippy_single_exposed_service_without_request_body() {
    let temp_dir = TempDir::new().unwrap();
    let exposed_service: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE3).unwrap();

    let subscribed_action1: SubscribedAction = serde_json5::from_str(
        r#"
        {
          id: "brain_move_arm",
          node: "brain",
          name: "move_arm",
          tag: "0.1.0"
        }
        "#,
    )
    .unwrap();
    let subscribed_action2: SubscribedAction = serde_json5::from_str(
        r#"
        {
          id: "controller_rotate_servo",
          node: "controller",
          name: "rotate_servo_clockwise",
          tag: "0.1.0"
        }
        "#,
    )
    .unwrap();
    let goal_response_format: MessageFormat =
        serde_json5::from_str(r#"{ accepted: "bool" }"#).unwrap();
    let action_messages = SubscribedActionMessage {
        goal_request: None,
        goal_response: Some(goal_response_format),
        feedback: None,
        result_request: None,
        result_response: None,
    };

    let (mut generator, output_dir, user_node, _) = init_test_env(&temp_dir);
    generator.add_exposed_service(&exposed_service).unwrap();
    generator
        .add_subscribed_action(&subscribed_action1, &action_messages)
        .unwrap();
    generator
        .add_subscribed_action(&subscribed_action2, &action_messages)
        .unwrap();
    let output_config = copy_config_to_output(&user_node, &output_dir);
    generator.build(&output_dir).unwrap();
    fs::remove_file(output_config).unwrap();

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

    let exposed_services_contents =
        std::fs::read_to_string(output_dir.join("src/exposed_services.rs"))
            .expect("failed to read exposed_services module");
    assert_contains_all(&exposed_services_contents, &["pub mod get_system_status;"]);

    let subscribed_actions_contents =
        std::fs::read_to_string(output_dir.join("src/subscribed_actions.rs"))
            .expect("failed to read subscribed_actions module");
    assert_contains_all(
        &subscribed_actions_contents,
        &[
            "pub mod brain_move_arm;",
            "pub mod controller_rotate_servo_clockwise;",
        ],
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

    let empty_format: MessageFormat = serde_json5::from_str(EMPTY_MESSAGE_FORMAT).unwrap();

    let (mut generator, output_dir, user_node, _) = init_test_env(&temp_dir);
    generator.add_exposed_service(&exposed_service1).unwrap();
    generator.add_exposed_service(&exposed_service2).unwrap();
    generator
        .add_subscribed_service(
            &subscribed_service1,
            &subscribed_service_request1,
            &subscribed_service_response1,
        )
        .unwrap();
    generator
        .add_subscribed_service(
            &subscribed_service2,
            &empty_format,
            &subscribed_service_response2,
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
        !output_dir.join(NODE_CONFIG_FILE).exists(),
        "Generated crate should not keep a copy of the node configuration file"
    );

    let lib_contents =
        std::fs::read_to_string(output_dir.join("src/lib.rs")).expect("failed to read lib.rs");
    assert_contains_all(
        &lib_contents,
        &["pub mod exposed_services;", "pub mod subscribed_services;"],
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

/// Checks for clippy warnings when there is a subscribed service with an empty request format.
#[test]
fn clippy_subscribed_service_empty_request_format() {
    let temp_dir = TempDir::new().unwrap();

    let subscribed_service: SubscribedService = serde_json5::from_str(
        r#"
        {
          id: "sensor_get_status",
          node: "sensor",
          name: "get_status",
          tag: "0.1.0"
        }
        "#,
    )
    .unwrap();
    let empty_format: MessageFormat = serde_json5::from_str(EMPTY_MESSAGE_FORMAT).unwrap();
    let response_format: MessageFormat = serde_json5::from_str(r#"{ status: "string" }"#).unwrap();

    let (mut generator, output_dir, user_node, _) = init_test_env(&temp_dir);
    generator
        .add_subscribed_service(&subscribed_service, &empty_format, &response_format)
        .unwrap();
    let output_config = copy_config_to_output(&user_node, &output_dir);
    generator.build(&output_dir).unwrap();
    fs::remove_file(output_config).unwrap();

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
}

/// Checks for clippy warnings when there is a subscribed service with an empty response format.
#[test]
fn clippy_subscribed_service_empty_response_format() {
    let temp_dir = TempDir::new().unwrap();

    let subscribed_service: SubscribedService = serde_json5::from_str(
        r#"
        {
          id: "sensor_trigger_action",
          node: "sensor",
          name: "trigger_action",
          tag: "0.1.0"
        }
        "#,
    )
    .unwrap();
    let request_format: MessageFormat = serde_json5::from_str(r#"{ action_id: "u32" }"#).unwrap();
    let empty_format: MessageFormat = serde_json5::from_str(EMPTY_MESSAGE_FORMAT).unwrap();

    let (mut generator, output_dir, user_node, _) = init_test_env(&temp_dir);
    generator
        .add_subscribed_service(&subscribed_service, &request_format, &empty_format)
        .unwrap();
    let output_config = copy_config_to_output(&user_node, &output_dir);
    generator.build(&output_dir).unwrap();
    fs::remove_file(output_config).unwrap();

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
}
