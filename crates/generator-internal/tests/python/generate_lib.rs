use crate::helpers;
use config::node::PeppygenLanguage;

const PEPPY_JSON5_CONFIG: &str = r#"{
  schema_version: 1,
  manifest: {
    name: "test_node",
    tag: "0.1.0",
    language: "python",
    add_cmd: ["uv", "sync"],
    start_cmd: ["uv", "run", "test_node"]
  },
  interfaces: {
    exposes: {
      topics: [
        {
          name: "test_topic",
          qos_profile: "sensor_data",
          message_format: {
            value: "u32",
            timestamp: "time"
          }
        }
      ],
      services: [
        {
          name: "test_service",
          request_message_format: {
            input: "string"
          },
          response_message_format: {
            output: "string",
            success: "bool"
          }
        }
      ]
    }
  }
}"#;

#[test]
fn generate_peppygen_lib_uv() {
    let (_temp_dir, peppygen_dir) =
        helpers::run_generate_peppygen_lib_test(PeppygenLanguage::Python, PEPPY_JSON5_CONFIG);

    // Check that pyproject.toml was generated
    let pyproject_toml = peppygen_dir.join("pyproject.toml");
    assert!(
        pyproject_toml.exists(),
        "pyproject.toml should exist at {}",
        pyproject_toml.display()
    );

    // Check that peppygen/__init__.py was generated
    let init_py = peppygen_dir.join("peppygen/__init__.py");
    assert!(
        init_py.exists(),
        "peppygen/__init__.py should exist at {}",
        init_py.display()
    );
}
