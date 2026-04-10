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
    language: "rust",
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
    run_cmd: [
      "cargo",
      "run",
      "--release"
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
    language: "rust",
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
    run_cmd: [
      "cargo",
      "run",
      "--release"
    ],
  }
}
"#;

const NESTED_STRUCT_COLLISION_NODE_EXAMPLE: &str = r#"
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
    language: "rust",
    parameters: {
      control: {
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
      }
    },
    run_cmd: [
      "cargo",
      "run",
      "--release"
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
    language: "rust",
    parameters: {
      device: {
        enabled: true
      }
    },
    run_cmd: [
      "cargo",
      "run",
      "--release"
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
    language: "rust",
    parameters: {
      device: "uuid"
    },
    run_cmd: [
      "cargo",
      "run",
      "--release"
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
    language: "rust",
    parameters: {
      enabled: true
    },
    run_cmd: [
      "cargo",
      "run",
      "--release"
    ],
  }
}
"#;

#[test]
fn generate_parameters_struct() {
    let temp_dir = TempDir::new().unwrap();
    let node_config: NodeConfig =
        serde_json5::from_str(NODE_EXAMPLE).expect("failed to parse NODE_EXAMPLE into NodeConfig");

    let (mut generator, output_dir, _, _) = init_test_env::<RustGenerator>(&temp_dir);
    generator.set_parameters(node_config.execution.parameters);
    generator
        .build(
            &output_dir,
            &config::consts::PeppyDirs::default(),
            Default::default(),
        )
        .unwrap();

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
            "pub resolution: VideoResolution",
            "pub encoding: String",
        ],
    );

    // Verify nested VideoResolution struct
    assert_contains_all(
        &generated,
        &[
            "pub struct VideoResolution",
            "pub width: u16",
            "pub height: u16",
        ],
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
    generator
        .build(
            &output_dir,
            &config::consts::PeppyDirs::default(),
            Default::default(),
        )
        .unwrap();

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
fn generate_parameters_struct_avoids_nested_struct_name_collisions() {
    let temp_dir = TempDir::new().unwrap();
    let node_config: NodeConfig = serde_json5::from_str(NESTED_STRUCT_COLLISION_NODE_EXAMPLE)
        .expect("failed to parse NESTED_STRUCT_COLLISION_NODE_EXAMPLE into NodeConfig");

    let (mut generator, output_dir, _, _) = init_test_env::<RustGenerator>(&temp_dir);
    generator.set_parameters(node_config.execution.parameters);
    generator
        .build(
            &output_dir,
            &config::consts::PeppyDirs::default(),
            Default::default(),
        )
        .unwrap();

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
            "pub control: control::Control",
            "pub mod control",
            "pub struct Control",
            "pub left: ControlLeft",
            "pub right: ControlRight",
            "pub struct ControlLeft",
            "pub config: ControlLeftConfig",
            "pub struct ControlRight",
            "pub config: ControlRightConfig",
            "pub struct ControlLeftConfig",
            "pub threshold: u16",
            "pub struct ControlRightConfig",
            "pub enabled: bool",
        ],
    );

    assert_rendered!(
        !generated.contains("pub struct Config"),
        &generated,
        "expected no ambiguous shared nested struct name"
    );
}

#[test]
fn reject_parameters_with_invalid_field_names() {
    use crate::error::Error;
    use crate::generator::rust::generate_parameters_struct;
    use config::consts::ALLOWED_CONFIG_CHARS;

    let node_config: NodeConfig = serde_json5::from_str(INVALID_PARAMETERS_NODE_EXAMPLE)
        .expect("failed to parse INVALID_PARAMETERS_NODE_EXAMPLE into NodeConfig");

    let result = generate_parameters_struct(&node_config.execution.parameters);

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
fn reject_parameters_with_unsupported_spec_type() {
    use crate::error::Error;
    use crate::generator::rust::generate_parameters_struct;

    let node_config: NodeConfig =
        serde_json5::from_str(UNSUPPORTED_PARAMETERS_VARIANT_NODE_EXAMPLE)
            .expect("failed to parse UNSUPPORTED_PARAMETERS_VARIANT_NODE_EXAMPLE into NodeConfig");

    let err = generate_parameters_struct(&node_config.execution.parameters).unwrap_err();
    match err {
        Error::UnsupportedParameterSpecType { path, kind } => {
            assert_eq!(path, "device.enabled");
            assert_eq!(kind, "bool");
        }
        other => panic!("Expected UnsupportedParameterSpecType error, got: {other:?}"),
    }
}

#[test]
fn reject_parameters_with_top_level_unsupported_spec_type() {
    use crate::error::Error;
    use crate::generator::rust::generate_parameters_struct;

    let node_config: NodeConfig = serde_json5::from_str(
        UNSUPPORTED_TOP_LEVEL_PARAMETER_VARIANT_NODE_EXAMPLE,
    )
    .expect("failed to parse UNSUPPORTED_TOP_LEVEL_PARAMETER_VARIANT_NODE_EXAMPLE into NodeConfig");

    let err = generate_parameters_struct(&node_config.execution.parameters).unwrap_err();
    match err {
        Error::UnsupportedParameterSpecType { path, kind } => {
            assert_eq!(path, "enabled");
            assert_eq!(kind, "bool");
        }
        other => panic!("Expected UnsupportedParameterSpecType error, got: {other:?}"),
    }
}

#[test]
fn reject_parameters_with_unknown_type_name() {
    use crate::error::Error;
    use crate::generator::rust::generate_parameters_struct;

    let node_config: NodeConfig = serde_json5::from_str(UNKNOWN_PARAMETER_TYPE_NODE_EXAMPLE)
        .expect("failed to parse UNKNOWN_PARAMETER_TYPE_NODE_EXAMPLE into NodeConfig");

    let err = generate_parameters_struct(&node_config.execution.parameters).unwrap_err();
    match err {
        Error::UnsupportedParameterTypeName { path, type_name } => {
            assert_eq!(path, "device");
            assert_eq!(type_name, "uuid");
        }
        other => panic!("Expected UnsupportedParameterTypeName error, got: {other:?}"),
    }
}
