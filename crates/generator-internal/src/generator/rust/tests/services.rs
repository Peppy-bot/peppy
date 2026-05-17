use super::*;
use config::node::{ConsumedService, ExposedService, PeppygenLanguage};

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
  local_node_id: "uvc_camera",
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

const SUBSCRIBED_SERVICE_RESPONSE_OPTIONAL_SCALAR: &str = r#"
{
  maybe_code: {
    $type: "u32",
    $optional: true
  }
}
"#;

const SUBSCRIBED_SERVICE_EXAMPLE2: &str = r#"
{
    local_node_id: "uvc_camera",
    name: "get_camera_info",
}
"#;
// No request body for the second consumed service
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
    generator.add_exposed_service(&service, None).unwrap();
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
    generator.add_exposed_service(&service, None).unwrap();
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
    assert_artifact_contains(&artifacts, "const SERVICE_NAME: &str = \"enable_camera\";");
    assert_artifact_contains(&artifacts, "const SERVICE_NAME: &str = \"get_lidar_info\";");

    // enable_camera has response, get_lidar_info does not
    assert_artifact_contains(&artifacts, "F: Fn(Request) -> crate::Result<Response>");
    assert_artifact_contains(&artifacts, "F: Fn(Request) -> crate::Result<()>");
}

/// In the case of a service, a "subscribed" service is an entity expects to connect to another entity
#[test]
fn consumed_service() {
    let service: ConsumedService = serde_json5::from_str(SUBSCRIBED_SERVICE_EXAMPLE1).unwrap();
    let request_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_SERVICE_REQUEST_EXAMPLE1).unwrap();
    let response_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_SERVICE_RESPONSE_EXAMPLE1).unwrap();

    let mut generator = RustGenerator::new();
    generator
        .add_consumed_service(&service, &request_format, &response_format, "uvc_camera")
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
            "target_core_node: Option<&str>",
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

    let mut generator = RustGenerator::new();
    generator
        .add_consumed_service(&service1, &request_format1, &response_format1, "uvc_camera")
        .unwrap();
    generator
        .add_consumed_service(&service2, &empty_format, &response_format2, "uvc_camera")
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
fn consumed_service_without_response_payload() {
    let service = r#"
        {
            local_node_id: "uvc_camera",
            name: "get_camera_info",
        }
        "#;
    let service: ConsumedService = serde_json5::from_str(service).unwrap();
    let empty_format: MessageFormat = serde_json5::from_str(EMPTY_MESSAGE_FORMAT).unwrap();

    let mut generator = RustGenerator::new();
    generator
        .add_consumed_service(&service, &empty_format, &empty_format, "uvc_camera")
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

#[test]
fn consumed_service_rejects_optional_scalar_response_field() {
    use crate::error::Error;

    let service: ConsumedService = serde_json5::from_str(SUBSCRIBED_SERVICE_EXAMPLE1).unwrap();
    let empty_format: MessageFormat = serde_json5::from_str(EMPTY_MESSAGE_FORMAT).unwrap();
    let response_format: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_SERVICE_RESPONSE_OPTIONAL_SCALAR).unwrap();

    let mut generator = RustGenerator::new();
    let err = generator
        .add_consumed_service(&service, &empty_format, &response_format, "uvc_camera")
        .unwrap_err();

    match err {
        Error::UnsupportedOptionalScalarType {
            language,
            field,
            item,
        } => {
            assert_eq!(language, PeppygenLanguage::Rust);
            assert_eq!(field, "maybe_code");
            assert_eq!(item, "u32");
        }
        other => panic!("expected UnsupportedOptionalScalarType, got: {other:?}"),
    }
}

/// Checks for clippy warnings when there is only one exposed service without a request body.
#[test]
fn clippy_single_exposed_service_without_request_body() {
    let temp_dir = TempDir::new().unwrap();
    let exposed_service: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE3).unwrap();

    let consumed_action1: ConsumedAction = serde_json5::from_str(
        r#"
        {
          local_node_id: "brain",
          name: "move_arm",
        }
        "#,
    )
    .unwrap();
    let consumed_action2: ConsumedAction = serde_json5::from_str(
        r#"
        {
          local_node_id: "controller",
          name: "rotate_servo_clockwise",
        }
        "#,
    )
    .unwrap();
    let goal_response_format: MessageFormat =
        serde_json5::from_str(r#"{ accepted: "bool" }"#).unwrap();
    let action_messages = ConsumedActionMessage {
        goal_request: None,
        goal_response: Some(goal_response_format),
        feedback: None,
        result_request: None,
        result_response: None,
    };

    let (mut generator, output_dir, user_node, _) = init_test_env::<RustGenerator>(&temp_dir);
    generator
        .add_exposed_service(&exposed_service, None)
        .unwrap();
    generator
        .add_consumed_action(&consumed_action1, &action_messages, "brain")
        .unwrap();
    generator
        .add_consumed_action(&consumed_action2, &action_messages, "controller")
        .unwrap();
    let output_config = copy_config_to_output(&user_node, &output_dir);
    generator
        .build(
            &output_dir,
            &config::consts::PeppyDirs::default(),
            Default::default(),
        )
        .unwrap();
    fs::remove_file(output_config).unwrap();

    run_clippy(&output_dir);

    let exposed_services_contents =
        std::fs::read_to_string(output_dir.join("src/exposed_services.rs"))
            .expect("failed to read exposed_services module");
    assert_contains_all(&exposed_services_contents, &["pub mod get_system_status;"]);

    let consumed_actions_contents =
        std::fs::read_to_string(output_dir.join("src/consumed_actions.rs"))
            .expect("failed to read consumed_actions module");
    assert_contains_all(
        &consumed_actions_contents,
        &[
            "pub mod brain_move_arm;",
            "pub mod controller_rotate_servo_clockwise;",
        ],
    );
}

/// This is a long running test that verifies the generated code compiles and passes clippy
#[test]
fn compile_lib_with_exposed_and_consumed_services() {
    let temp_dir = TempDir::new().unwrap();
    let exposed_service1: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE).unwrap();
    let exposed_service2: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE2).unwrap();

    let consumed_service1: ConsumedService =
        serde_json5::from_str(SUBSCRIBED_SERVICE_EXAMPLE1).unwrap();

    let consumed_service_request1: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_SERVICE_REQUEST_EXAMPLE1).unwrap();
    let consumed_service_response1: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_SERVICE_RESPONSE_EXAMPLE1).unwrap();

    // Second service pointing to the same node
    let consumed_service2: ConsumedService =
        serde_json5::from_str(SUBSCRIBED_SERVICE_EXAMPLE2).unwrap();
    let consumed_service_response2: MessageFormat =
        serde_json5::from_str(SUBSCRIBED_SERVICE_RESPONSE_EXAMPLE2).unwrap();

    let empty_format: MessageFormat = serde_json5::from_str(EMPTY_MESSAGE_FORMAT).unwrap();

    let (mut generator, output_dir, user_node, _) = init_test_env::<RustGenerator>(&temp_dir);
    generator
        .add_exposed_service(&exposed_service1, None)
        .unwrap();
    generator
        .add_exposed_service(&exposed_service2, None)
        .unwrap();
    generator
        .add_consumed_service(
            &consumed_service1,
            &consumed_service_request1,
            &consumed_service_response1,
            "uvc_camera",
        )
        .unwrap();
    generator
        .add_consumed_service(
            &consumed_service2,
            &empty_format,
            &consumed_service_response2,
            "uvc_camera",
        )
        .unwrap();
    let output_config = copy_config_to_output(&user_node, &output_dir);
    generator
        .build(
            &output_dir,
            &config::consts::PeppyDirs::default(),
            Default::default(),
        )
        .unwrap();
    fs::remove_file(output_config).unwrap();

    run_cargo_build(&output_dir);
    run_clippy(&output_dir);

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
        &["pub mod exposed_services;", "pub mod consumed_services;"],
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
            .join("src/consumed_services/uvc_camera_enable_camera.rs")
            .exists(),
        "Expected uvc_camera_enable_camera subscriber module"
    );
    assert!(
        output_dir
            .join("src/consumed_services/uvc_camera_get_camera_info.rs")
            .exists(),
        "Expected uvc_camera_get_camera_info subscriber module"
    );
}

/// Checks for clippy warnings when there is a consumed service with an empty request format.
#[test]
fn clippy_consumed_service_empty_request_format() {
    let temp_dir = TempDir::new().unwrap();

    let consumed_service: ConsumedService = serde_json5::from_str(
        r#"
        {
          local_node_id: "sensor",
          name: "get_status",
        }
        "#,
    )
    .unwrap();
    let empty_format: MessageFormat = serde_json5::from_str(EMPTY_MESSAGE_FORMAT).unwrap();
    let response_format: MessageFormat = serde_json5::from_str(r#"{ status: "string" }"#).unwrap();

    let (mut generator, output_dir, user_node, _) = init_test_env::<RustGenerator>(&temp_dir);
    generator
        .add_consumed_service(&consumed_service, &empty_format, &response_format, "sensor")
        .unwrap();
    let output_config = copy_config_to_output(&user_node, &output_dir);
    generator
        .build(
            &output_dir,
            &config::consts::PeppyDirs::default(),
            Default::default(),
        )
        .unwrap();
    fs::remove_file(output_config).unwrap();

    run_clippy(&output_dir);
}

/// Checks for clippy warnings when there is a consumed service with an empty response format.
#[test]
fn clippy_consumed_service_empty_response_format() {
    let temp_dir = TempDir::new().unwrap();

    let consumed_service: ConsumedService = serde_json5::from_str(
        r#"
        {
          local_node_id: "sensor",
          name: "trigger_action",
        }
        "#,
    )
    .unwrap();
    let request_format: MessageFormat = serde_json5::from_str(r#"{ action_id: "u32" }"#).unwrap();
    let empty_format: MessageFormat = serde_json5::from_str(EMPTY_MESSAGE_FORMAT).unwrap();

    let (mut generator, output_dir, user_node, _) = init_test_env::<RustGenerator>(&temp_dir);
    generator
        .add_consumed_service(&consumed_service, &request_format, &empty_format, "sensor")
        .unwrap();
    let output_config = copy_config_to_output(&user_node, &output_dir);
    generator
        .build(
            &output_dir,
            &config::consts::PeppyDirs::default(),
            Default::default(),
        )
        .unwrap();
    fs::remove_file(output_config).unwrap();

    run_clippy(&output_dir);
}
