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
    language: "python",
    labels: [
      "uvc",
      "camera",
      "usb",
    ],
    start_cmd: [
      "python",
      "-m",
      "uvc_camera"
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
    language: "python",
    labels: [
      "uvc",
      "camera",
      "usb",
    ],
    start_cmd: [
      "python",
      "-m",
      "uvc_camera"
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

    let output_dir = temp_dir.path().join("output");
    fs::create_dir_all(&output_dir).unwrap();

    let mut generator = PythonGenerator::new();
    generator.set_parameters(node_config.parameters);
    generator.build(&output_dir).unwrap();

    let parameters_file = output_dir.join("peppygen").join("parameters.py");
    assert!(
        parameters_file.exists(),
        "Expected parameters.py to be generated"
    );

    let generated = fs::read_to_string(&parameters_file).expect("failed to read parameters.py");

    assert_contains_all(
        &generated,
        &[
            "@dataclass",
            "class Parameters:",
            "device: Device",
            "video: Video",
        ],
    );

    // Verify the Device dataclass
    assert_contains_all(
        &generated,
        &[
            "class Device:",
            "physical: str",
            "sim: str",
            "priority: str",
        ],
    );

    // Verify the Video dataclass with nested Resolution
    assert_contains_all(
        &generated,
        &[
            "class Video:",
            "frame_rate: int",
            "resolution: Resolution",
            "encoding: str",
        ],
    );

    // Verify nested Resolution dataclass
    assert_contains_all(
        &generated,
        &["class Resolution:", "width: int", "height: int"],
    );
}

#[test]
fn generate_empty_parameters_struct() {
    let temp_dir = TempDir::new().unwrap();

    let output_dir = temp_dir.path().join("output");
    fs::create_dir_all(&output_dir).unwrap();

    let generator = PythonGenerator::new();
    // Don't set any parameters - use the default empty parameters
    generator.build(&output_dir).unwrap();

    let parameters_file = output_dir.join("peppygen").join("parameters.py");
    assert!(
        parameters_file.exists(),
        "Expected parameters.py to be generated"
    );

    let generated = fs::read_to_string(&parameters_file).expect("failed to read parameters.py");

    // Even with no parameters, we should have a valid Parameters dataclass
    assert_contains_all(&generated, &["@dataclass", "class Parameters:"]);
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
