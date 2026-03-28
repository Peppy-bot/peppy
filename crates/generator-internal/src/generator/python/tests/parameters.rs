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
    labels: [
      "uvc",
      "camera",
      "usb",
    ],
  },
  interfaces: {},
  execution: {
    language: "python",
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
    start_cmd: [
      "python",
      "-m",
      "uvc_camera"
    ],
  }
}
"#;

const INVALID_PARAMETERS_NODE_EXAMPLE: &str = r#"
{
  schema_version: 1,
  manifest: {
    name: "uvc_camera",
    tag: "0.1.0",
    labels: [
      "uvc",
      "camera",
      "usb",
    ],
  },
  interfaces: {},
  execution: {
    language: "python",
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
    start_cmd: [
      "python",
      "-m",
      "uvc_camera"
    ],
  }
}
"#;

const NESTED_CLASS_COLLISION_NODE_EXAMPLE: &str = r#"
{
  schema_version: 1,
  manifest: {
    name: "uvc_camera",
    tag: "0.1.0",
    labels: [
      "uvc",
      "camera",
      "usb",
    ],
  },
  interfaces: {},
  execution: {
    language: "python",
    parameters: {
      left: {
        config: {
          threshold: "u16"
        }
      },
      right: {
        config: {
          enabled: "bool"
        }
      }
    },
    start_cmd: [
      "python",
      "-m",
      "uvc_camera"
    ],
  }
}
"#;

const UNSUPPORTED_PARAMETERS_VARIANT_NODE_EXAMPLE: &str = r#"
{
  schema_version: 1,
  manifest: {
    name: "uvc_camera",
    tag: "0.1.0",
    labels: [
      "uvc",
      "camera",
      "usb",
    ],
  },
  interfaces: {},
  execution: {
    language: "python",
    parameters: {
      device: {
        enabled: true
      }
    },
    start_cmd: [
      "python",
      "-m",
      "uvc_camera"
    ],
  }
}
"#;

const UNKNOWN_PARAMETER_TYPE_NODE_EXAMPLE: &str = r#"
{
  schema_version: 1,
  manifest: {
    name: "uvc_camera",
    tag: "0.1.0",
    labels: [
      "uvc",
      "camera",
      "usb",
    ],
  },
  interfaces: {},
  execution: {
    language: "python",
    parameters: {
      device: "uuid"
    },
    start_cmd: [
      "python",
      "-m",
      "uvc_camera"
    ],
  }
}
"#;

const UNSUPPORTED_TOP_LEVEL_PARAMETER_VARIANT_NODE_EXAMPLE: &str = r#"
{
  schema_version: 1,
  manifest: {
    name: "uvc_camera",
    tag: "0.1.0",
    labels: [
      "uvc",
      "camera",
      "usb",
    ],
  },
  interfaces: {},
  execution: {
    language: "python",
    parameters: {
      enabled: true
    },
    start_cmd: [
      "python",
      "-m",
      "uvc_camera"
    ],
  }
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
    generator.set_parameters(node_config.execution.unwrap().parameters);
    generator
        .build(
            &output_dir,
            &config::consts::PeppyDirs::default(),
            Default::default(),
        )
        .unwrap();

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
            "resolution: VideoResolution",
            "encoding: str",
        ],
    );

    // Verify nested VideoResolution dataclass
    assert_contains_all(
        &generated,
        &["class VideoResolution:", "width: int", "height: int"],
    );

    // Verify from_dict classmethods for dict-to-dataclass hydration
    assert_contains_all(
        &generated,
        &[
            "def from_dict(cls, data: dict) -> \"Parameters\":",
            "device=Device.from_dict(data[\"device\"])",
            "video=Video.from_dict(data[\"video\"])",
            "def from_dict(cls, data: dict) -> \"Device\":",
            "physical=data[\"physical\"]",
            "def from_dict(cls, data: dict) -> \"Video\":",
            "resolution=VideoResolution.from_dict(data[\"resolution\"])",
            "def from_dict(cls, data: dict) -> \"VideoResolution\":",
            "width=data[\"width\"]",
        ],
    );
}

#[test]
fn generate_parameters_struct_avoids_nested_class_name_collisions() {
    let temp_dir = TempDir::new().unwrap();
    let node_config: NodeConfig = serde_json5::from_str(NESTED_CLASS_COLLISION_NODE_EXAMPLE)
        .expect("failed to parse NESTED_CLASS_COLLISION_NODE_EXAMPLE into NodeConfig");

    let output_dir = temp_dir.path().join("output");
    fs::create_dir_all(&output_dir).unwrap();

    let mut generator = PythonGenerator::new();
    generator.set_parameters(node_config.execution.unwrap().parameters);
    generator
        .build(
            &output_dir,
            &config::consts::PeppyDirs::default(),
            Default::default(),
        )
        .unwrap();

    let parameters_file = output_dir.join("peppygen").join("parameters.py");
    assert!(
        parameters_file.exists(),
        "Expected parameters.py to be generated"
    );

    let generated = fs::read_to_string(&parameters_file).expect("failed to read parameters.py");

    assert_contains_all(
        &generated,
        &[
            "class Left:",
            "config: LeftConfig",
            "class Right:",
            "config: RightConfig",
            "class LeftConfig:",
            "threshold: int",
            "class RightConfig:",
            "enabled: bool",
        ],
    );

    assert_rendered!(
        !generated.contains("class Config:"),
        &generated,
        "expected no ambiguous shared nested class name"
    );
}

#[test]
fn generate_empty_parameters_struct() {
    let temp_dir = TempDir::new().unwrap();

    let output_dir = temp_dir.path().join("output");
    fs::create_dir_all(&output_dir).unwrap();

    let generator = PythonGenerator::new();
    // Don't set any parameters - use the default empty parameters
    generator
        .build(
            &output_dir,
            &config::consts::PeppyDirs::default(),
            Default::default(),
        )
        .unwrap();

    let parameters_file = output_dir.join("peppygen").join("parameters.py");
    assert!(
        parameters_file.exists(),
        "Expected parameters.py to be generated"
    );

    let generated = fs::read_to_string(&parameters_file).expect("failed to read parameters.py");

    // Even with no parameters, we should have a valid Parameters dataclass
    // with a from_dict classmethod
    assert_contains_all(
        &generated,
        &[
            "@dataclass",
            "class Parameters:",
            "def from_dict(cls, data: dict) -> \"Parameters\":",
            "return cls()",
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

    let result = generate_parameters_struct(&node_config.execution.unwrap().parameters);

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

#[test]
fn reject_python_parameters_with_unsupported_spec_type() {
    use crate::error::Error;

    let temp_dir = TempDir::new().unwrap();
    let output_dir = temp_dir.path().join("output");
    fs::create_dir_all(&output_dir).unwrap();

    let node_config: NodeConfig =
        serde_json5::from_str(UNSUPPORTED_PARAMETERS_VARIANT_NODE_EXAMPLE)
            .expect("failed to parse UNSUPPORTED_PARAMETERS_VARIANT_NODE_EXAMPLE into NodeConfig");

    let mut generator = PythonGenerator::new();
    generator.set_parameters(node_config.execution.unwrap().parameters);
    let err = generator
        .build(
            &output_dir,
            &config::consts::PeppyDirs::default(),
            Default::default(),
        )
        .unwrap_err();

    match err {
        Error::UnsupportedParameterSpecType { path, kind } => {
            assert_eq!(path, "device.enabled");
            assert_eq!(kind, "bool");
        }
        other => panic!("Expected UnsupportedParameterSpecType error, got: {other:?}"),
    }
}

#[test]
fn reject_python_parameters_with_top_level_unsupported_spec_type() {
    use crate::error::Error;

    let temp_dir = TempDir::new().unwrap();
    let output_dir = temp_dir.path().join("output");
    fs::create_dir_all(&output_dir).unwrap();

    let node_config: NodeConfig = serde_json5::from_str(
        UNSUPPORTED_TOP_LEVEL_PARAMETER_VARIANT_NODE_EXAMPLE,
    )
    .expect("failed to parse UNSUPPORTED_TOP_LEVEL_PARAMETER_VARIANT_NODE_EXAMPLE into NodeConfig");

    let mut generator = PythonGenerator::new();
    generator.set_parameters(node_config.execution.unwrap().parameters);
    let err = generator
        .build(
            &output_dir,
            &config::consts::PeppyDirs::default(),
            Default::default(),
        )
        .unwrap_err();

    match err {
        Error::UnsupportedParameterSpecType { path, kind } => {
            assert_eq!(path, "enabled");
            assert_eq!(kind, "bool");
        }
        other => panic!("Expected UnsupportedParameterSpecType error, got: {other:?}"),
    }
}

#[test]
fn reject_python_parameters_with_unknown_type_name() {
    use crate::error::Error;

    let temp_dir = TempDir::new().unwrap();
    let output_dir = temp_dir.path().join("output");
    fs::create_dir_all(&output_dir).unwrap();

    let node_config: NodeConfig = serde_json5::from_str(UNKNOWN_PARAMETER_TYPE_NODE_EXAMPLE)
        .expect("failed to parse UNKNOWN_PARAMETER_TYPE_NODE_EXAMPLE into NodeConfig");

    let mut generator = PythonGenerator::new();
    generator.set_parameters(node_config.execution.unwrap().parameters);
    let err = generator
        .build(
            &output_dir,
            &config::consts::PeppyDirs::default(),
            Default::default(),
        )
        .unwrap_err();

    match err {
        Error::UnsupportedParameterTypeName { path, type_name } => {
            assert_eq!(path, "device");
            assert_eq!(type_name, "uuid");
        }
        other => panic!("Expected UnsupportedParameterTypeName error, got: {other:?}"),
    }
}
