use super::*;
use config::node::NodeConfig;
use std::fs;
use tempfile::TempDir;

const NODE_EXAMPLE: &str = r#"
{
  schema_version: 1,
  manifest: {
    name: "uvc_camera",
    tag: "0.1.0",
    language: "rust",
    labels: [
      "uvc",
      "camera",
      "usb",
    ],
    start_cmd: [
      "cargo",
      "run",
      "--release"
    ]
  },
  parameters: {
    device: {
      physical: "string",
      sim: "string",
      priority: "string"
    },
    video: {
      frame_rate: "u16",
      resolution: {
        width: "u16",
        height: "u16",
      },
      encoding: "string",
    },
  },
  interfaces: {}
}
"#;

const INVALID_PARAMETERS_NODE_EXAMPLE: &str = r#"
{
  schema_version: 1,
  manifest: {
    name: "uvc_camera",
    tag: "0.1.0",
    language: "rust",
    labels: [
      "uvc",
      "camera",
      "usb",
    ],
    start_cmd: [
      "cargo",
      "run",
      "--release"
    ]
  },
  parameters: {
    device: {
      $type: "object",
      physical: "string",
      sim: "string",
      priority: "string"
    },
    video: {
      "*type": "object",
      frame_rate: "u16",
      resolution: {
        "%type": "object",
        width: "u16",
        height: "u16",
      },
      encoding: "string",
    },
  },
  interfaces: {}
}
"#;

#[test]
fn generate_parameters_struct() {
    let temp_dir = TempDir::new().unwrap();
    let node_config: NodeConfig =
        serde_json5::from_str(NODE_EXAMPLE).expect("failed to parse NODE_EXAMPLE into NodeConfig");

    let (mut generator, output_dir, _, _) = init_test_env::<RustGenerator>(&temp_dir);
    generator.set_parameters(node_config.parameters);
    generator.build(&output_dir).unwrap();

    let parameters_file = output_dir.join("src/parameters.rs");
    assert!(
        parameters_file.exists(),
        "Expected parameters.rs to be generated"
    );

    let generated = fs::read_to_string(&parameters_file).expect("failed to read parameters.rs");
    assert_contains_all(
        &generated,
        &[
            "pub struct Parameters",
            "pub device: device::Device",
            "pub video: video::Video",
        ],
    );

    // Verify the device module with Device struct
    assert_contains_all(
        &generated,
        &[
            "pub mod device",
            "pub struct Device",
            "pub physical: String",
            "pub sim: String",
            "pub priority: String",
        ],
    );

    // Verify the video module with Video struct and nested Resolution
    assert_contains_all(
        &generated,
        &[
            "pub mod video",
            "pub struct Video",
            "pub frame_rate: u16",
            "pub resolution: Resolution",
            "pub encoding: String",
        ],
    );

    // Verify nested Resolution struct (simple name, no prefix)
    assert_contains_all(
        &generated,
        &["pub struct Resolution", "pub width: u16", "pub height: u16"],
    );

    // Verify derive attributes
    assert_contains_all(
        &generated,
        &["#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]"],
    );
}

#[test]
fn generate_empty_parameters_struct() {
    let temp_dir = TempDir::new().unwrap();

    let (generator, output_dir, _, _) = init_test_env::<RustGenerator>(&temp_dir);
    // Don't set any parameters - use the default empty parameters
    generator.build(&output_dir).unwrap();

    let parameters_file = output_dir.join("src/parameters.rs");
    assert!(
        parameters_file.exists(),
        "Expected parameters.rs to be generated"
    );

    let generated = fs::read_to_string(&parameters_file).expect("failed to read parameters.rs");

    // Even with no parameters, we should have a valid Parameters struct with derives
    assert_contains_all(
        &generated,
        &[
            "#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]",
            "pub struct Parameters",
        ],
    );
}

#[test]
fn reject_parameters_with_invalid_field_names() {
    use crate::error::Error;
    use crate::generator::rust::generate_parameters_struct;
    use config::consts::ALLOWED_CONFIG_CHARS;

    let node_config: NodeConfig = serde_json5::from_str(INVALID_PARAMETERS_NODE_EXAMPLE)
        .expect("failed to parse INVALID_PARAMETERS_NODE_EXAMPLE into NodeConfig");

    let result = generate_parameters_struct(&node_config.parameters);

    assert!(
        result.is_err(),
        "Expected error for field names with invalid characters"
    );

    let err = result.unwrap_err();
    match err {
        Error::InvalidParameterFieldName { name, allowed } => {
            assert!(
                name.chars().any(|c| !ALLOWED_CONFIG_CHARS.contains(c)),
                "Expected field name with invalid characters, got: {}",
                name
            );
            assert_eq!(allowed, ALLOWED_CONFIG_CHARS);
        }
        _ => panic!("Expected InvalidParameterFieldName error, got: {:?}", err),
    }
}
