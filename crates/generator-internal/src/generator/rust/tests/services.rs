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
        rendered.contains("struct EnableCameraService;"),
        &rendered,
        "expected service struct declaration"
    );
    assert_rendered!(
        rendered.contains("pub struct EnableCameraResponse"),
        &rendered,
        "expected response struct for exposed service"
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
        rendered.contains("pub async fn handle_next_request<F>("),
        &rendered,
        "expected async service handler with handle_next_request naming"
    );
    assert_rendered!(
        rendered.contains("handler: F"),
        &rendered,
        "expected handler callback parameter"
    );
    assert_rendered!(
        rendered.contains("F: Fn(bool) -> crate::Result<EnableCameraResponse>"),
        &rendered,
        "expected Fn trait bound for handler"
    );
    assert_rendered!(
        rendered.contains("-> crate::Result<()>"),
        &rendered,
        "expected async function to return unit result"
    );
    assert_rendered!(
        rendered.contains("messenger.handle().listen(namespace, service_name).await?"),
        &rendered,
        "expected service listen call"
    );
    assert_rendered!(
        rendered.contains("service.next_request().await?"),
        &rendered,
        "expected next_request call to receive incoming request"
    );
    assert_rendered!(
        rendered.contains("enable_camera_deserialize_request"),
        &rendered,
        "expected request deserializer helper function"
    );
    assert_rendered!(
        rendered.contains("let response_payload = bytes::Bytes::new();"),
        &rendered,
        "expected inline response serialization"
    );
    assert_rendered!(
        rendered.contains("handler(enable)?"),
        &rendered,
        "expected handler callback invocation with deserialized parameter"
    );
    assert_rendered!(
        rendered.contains("service.send_response(request.id, response_payload).await?"),
        &rendered,
        "expected send_response call"
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
fn expose_two_services() {
    let service1: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE).unwrap();
    let service2: ExposedService = serde_json5::from_str(EXPOSED_SERVICE_EXAMPLE2).unwrap();

    let mut generator = RustGenerator::new();
    generator.add_exposed_service(&service1).unwrap();
    generator.add_exposed_service(&service2).unwrap();

    let artifacts = generator.into_artifacts();

    assert_eq!(
        artifacts.len(),
        2,
        "expected two generated artifacts, got {}",
        artifacts.len()
    );

    let enable_camera_rendered = artifacts
        .iter()
        .find(|artifact| artifact.node_name == "enable_camera")
        .map(|artifact| &artifact.code_output)
        .expect("expected generated artifact for `enable_camera`");
    let get_lidar_info_rendered = artifacts
        .iter()
        .find(|artifact| artifact.node_name == "get_lidar_info")
        .map(|artifact| &artifact.code_output)
        .expect("expected generated artifact for `get_lidar_info`");

    assert_rendered!(
        enable_camera_rendered.contains("struct EnableCameraService;"),
        enable_camera_rendered,
        "expected service struct declaration for `enable_camera`"
    );
    assert_rendered!(
        enable_camera_rendered.contains("pub async fn handle_next_request<F>("),
        enable_camera_rendered,
        "expected async handler for `enable_camera`"
    );
    assert_rendered!(
        enable_camera_rendered.contains("F: Fn(bool) -> crate::Result<EnableCameraResponse>"),
        enable_camera_rendered,
        "expected handler signature for `enable_camera`"
    );

    assert_rendered!(
        get_lidar_info_rendered.contains("struct GetLidarInfoService;"),
        get_lidar_info_rendered,
        "expected service struct declaration for `get_lidar_info`"
    );
    assert_rendered!(
        get_lidar_info_rendered.contains("struct GetLidarInfoRequest"),
        get_lidar_info_rendered,
        "expected private request struct for `get_lidar_info`"
    );
    assert_rendered!(
        get_lidar_info_rendered.contains("pub async fn handle_next_request<F>("),
        get_lidar_info_rendered,
        "expected async handler for `get_lidar_info`"
    );
    assert_rendered!(
        get_lidar_info_rendered.contains("F: Fn(GetLidarInfoRequest) -> crate::Result<()"),
        get_lidar_info_rendered,
        "expected handler signature for `get_lidar_info`"
    );
    assert_rendered!(
        get_lidar_info_rendered
            .contains("let request_data = Self::get_lidar_info_deserialize_request"),
        get_lidar_info_rendered,
        "expected deserializer binding for `get_lidar_info`"
    );
    assert_rendered!(
        get_lidar_info_rendered.contains("handler(request_data)?"),
        get_lidar_info_rendered,
        "expected handler invocation with request struct for `get_lidar_info`"
    );
    assert_rendered!(
        get_lidar_info_rendered.contains("fn get_lidar_info_deserialize_request("),
        get_lidar_info_rendered,
        "expected request deserializer helper for `get_lidar_info`"
    );
    assert_rendered!(
        get_lidar_info_rendered.contains("-> crate::Result<GetLidarInfoRequest>"),
        get_lidar_info_rendered,
        "expected request deserializer to return request struct for `get_lidar_info`"
    );
}

#[test]
fn subscribed_to_service() {
    let service = r#"
        {
            node: "uvc_camera",
            name: "enable_camera",
            tag: "0.1.0"
        }
        "#;
    let service: SubscribedService = serde_json5::from_str(service).unwrap();
    let request_format = r#"
        {
          enable: "bool"
        }
        "#;
    let response_format = r#"
        {
          enabled: "bool",
          error_msg: "string"
        }
        "#;
    let request_format: MessageFormat = serde_json5::from_str(request_format).unwrap();
    let response_format: MessageFormat = serde_json5::from_str(response_format).unwrap();

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
        rendered.contains("pub struct OnEnableCameraResponse"),
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
        rendered.contains("pub struct EnableCameraServicePoll;"),
        &rendered,
        "expected poll helper struct for subscribed service"
    );
    assert_rendered!(
        rendered.contains("impl EnableCameraServicePoll {"),
        &rendered,
        "expected poll helper impl"
    );
    assert_rendered!(
        rendered.contains("pub async fn enable_camera("),
        &rendered,
        "expected async poll helper signature"
    );
    assert_rendered!(
        rendered.contains("messenger: &crate::Messenger"),
        &rendered,
        "expected messenger parameter"
    );
    assert_rendered!(
        rendered.contains("-> crate::Result<OnEnableCameraResponse>"),
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
        rendered.contains("crate::messaging::to_bytes(message)?"),
        &rendered,
        "expected capnp to bytes conversion"
    );
    assert_rendered!(
        rendered.contains("crate::messaging::ServiceMessenger::poll("),
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
        rendered.contains("Ok(OnEnableCameraResponse {"),
        &rendered,
        "expected response construction"
    );
}

#[test]
fn subscribed_to_two_services_same_node() {
    let service1 = r#"
        {
            node: "uvc_camera",
            name: "enable_camera",
            tag: "0.1.0"
        }
        "#;
    let service1: SubscribedService = serde_json5::from_str(service1).unwrap();
    let request_format1 = r#"
        {
          enable: "bool"
        }
        "#;
    let response_format1 = r#"
        {
          enabled: "bool",
          error_msg: "string"
        }
        "#;
    let request_format1: MessageFormat = serde_json5::from_str(request_format1).unwrap();
    let response_format1: MessageFormat = serde_json5::from_str(response_format1).unwrap();

    // Second service pointing to the same node
    let service2 = r#"
        {
            node: "uvc_camera",
            name: "get_camera_info",
            tag: "0.1.0"
        }
        "#;
    let service2: SubscribedService = serde_json5::from_str(service2).unwrap();
    let response_format2 = r#"
        {
          card_type: "string",
          size: "string",
          interval: "string"
        }
        "#;
    let response_format2: MessageFormat = serde_json5::from_str(response_format2).unwrap();

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
    let enable_camera_rendered = artifacts
        .iter()
        .find(|artifact| artifact.node_name == "enable_camera")
        .map(|artifact| &artifact.code_output)
        .expect("expected generated artifact for `enable_camera`");
    let get_camera_info_rendered = artifacts
        .iter()
        .find(|artifact| artifact.node_name == "get_camera_info")
        .map(|artifact| &artifact.code_output)
        .expect("expected generated artifact for `get_camera_info`");

    assert_rendered!(
        enable_camera_rendered.contains("pub struct OnEnableCameraResponse"),
        enable_camera_rendered,
        "expected response struct for `enable_camera`"
    );
    assert_rendered!(
        enable_camera_rendered.contains("pub struct EnableCameraServicePoll;"),
        enable_camera_rendered,
        "expected service poll struct for `enable_camera`"
    );
    assert_rendered!(
        enable_camera_rendered.contains("pub async fn enable_camera("),
        enable_camera_rendered,
        "expected poll helper function for `enable_camera`"
    );
    assert_rendered!(
        enable_camera_rendered.contains("enable: bool"),
        enable_camera_rendered,
        "expected request parameter for `enable_camera`"
    );
    assert_rendered!(
        enable_camera_rendered.contains("-> crate::Result<OnEnableCameraResponse>"),
        enable_camera_rendered,
        "expected return type for `enable_camera` poll helper"
    );
    assert_rendered!(
        enable_camera_rendered.contains("root.set_enable(enable);"),
        enable_camera_rendered,
        "expected request serialization for `enable_camera`"
    );
    assert_rendered!(
        enable_camera_rendered.contains("crate::messaging::ServiceMessenger::poll("),
        enable_camera_rendered,
        "expected poll invocation for `enable_camera`"
    );
    assert_rendered!(
        enable_camera_rendered.contains("capnp::serialize::read_message"),
        enable_camera_rendered,
        "expected response deserialization for `enable_camera`"
    );
    assert_rendered!(
        enable_camera_rendered.contains("Ok(OnEnableCameraResponse {"),
        enable_camera_rendered,
        "expected response construction for `enable_camera`"
    );

    assert_rendered!(
        get_camera_info_rendered.contains("pub struct OnGetCameraInfoResponse"),
        get_camera_info_rendered,
        "expected response struct for `get_camera_info`"
    );
    assert_rendered!(
        get_camera_info_rendered.contains("card_type: String"),
        get_camera_info_rendered,
        "expected response field for `card_type`"
    );
    assert_rendered!(
        get_camera_info_rendered.contains("interval: String"),
        get_camera_info_rendered,
        "expected response field for `interval`"
    );
    assert_rendered!(
        get_camera_info_rendered.contains("size: String"),
        get_camera_info_rendered,
        "expected response field for `size`"
    );
    assert_rendered!(
        get_camera_info_rendered.contains("pub struct GetCameraInfoServicePoll;"),
        get_camera_info_rendered,
        "expected service poll struct for `get_camera_info`"
    );
    assert_rendered!(
        get_camera_info_rendered.contains(
            "pub async fn get_camera_info(\n        messenger: &crate::Messenger,\n    )"
        ),
        get_camera_info_rendered,
        "expected requestless poll helper signature for `get_camera_info`"
    );
    assert_rendered!(
        get_camera_info_rendered.contains("-> crate::Result<OnGetCameraInfoResponse>"),
        get_camera_info_rendered,
        "expected return type for `get_camera_info` poll helper"
    );
    assert_rendered!(
        get_camera_info_rendered.contains("crate::messaging::ServiceMessenger::poll("),
        get_camera_info_rendered,
        "expected poll invocation for `get_camera_info`"
    );
    assert_rendered!(
        get_camera_info_rendered.contains("capnp::serialize::read_message"),
        get_camera_info_rendered,
        "expected response deserialization for `get_camera_info`"
    );
    assert_rendered!(
        get_camera_info_rendered.contains("Ok(OnGetCameraInfoResponse {"),
        get_camera_info_rendered,
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
        rendered.contains("pub struct GetCameraInfoServicePoll;"),
        rendered,
        "expected poll struct definition for `get_camera_info`"
    );
    assert_rendered!(
        rendered.contains(
            "pub async fn get_camera_info(messenger: &crate::Messenger) -> crate::Result<()> {"
        ),
        rendered,
        "expected requestless poll helper returning unit result"
    );
    assert_rendered!(
        !rendered.contains("OnGetCameraInfoResponse"),
        rendered,
        "expected no response struct when response format is missing"
    );
    assert_rendered!(
        rendered.contains("let _ = crate::messaging::ServiceMessenger::poll("),
        rendered,
        "expected poll invocation that discards response bytes"
    );
}

fn create_lib_with_exposed_services_artifact() {
    todo!("finish")
}
