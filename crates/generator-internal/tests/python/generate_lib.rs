use crate::helpers;
use config::{
    consts::{NODE_CONFIG_FILE, PEPPYGEN_OUTPUT_PATH},
    node::{ConsumedAction, ConsumedService, ConsumedTopic, MessageFormat, PeppygenLanguage},
};
use generator::generate_peppygen_lib;
use generator::{ConsumedActionMessage, DeploymentInterface, InterfaceVariant};
use std::fs;
use tempfile::TempDir;

const PEPPY_JSON5_CONFIG: &str = r#"{
  schema_version: 1,
  manifest: { name: "test_node",
    tag: "0.1.0" },
  interfaces: {
    topics: {
      emits: [
        {
          name: "test_topic",
          qos_profile: "sensor_data",
          message_format: {
            value: "u32",
            timestamp: "time"
          }
        }
      ]
    },
    services: {
      exposes: [
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
  },

  execution: { language: "python",
    add_cmd: ["uv", "sync"],
    start_cmd: ["uv", "run", "test_node"]
  },
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

    // Check that emitted topics module was generated
    let emitted_topics_dir = peppygen_dir.join("peppygen/emitted_topics");
    assert!(
        emitted_topics_dir.exists(),
        "emitted_topics directory should exist at {}",
        emitted_topics_dir.display()
    );

    // Check that exposed services module was generated
    let exposed_services_dir = peppygen_dir.join("peppygen/exposed_services");
    assert!(
        exposed_services_dir.exists(),
        "exposed_services directory should exist at {}",
        exposed_services_dir.display()
    );
}

#[test]
fn generate_peppygen_lib_minimal_config() {
    let temp_dir = TempDir::new().expect("failed to create temp directory");
    let node_dir = temp_dir.path();

    // Minimal config with no interfaces
    let minimal_config = r#"{
      schema_version: 1,
      manifest: { name: "minimal_node",
        tag: "0.1.0" },

      execution: { language: "python",
        add_cmd: ["uv", "sync"],
        start_cmd: ["uv", "run", "minimal_node"]
      }
    }"#;

    let config_path = node_dir.join(NODE_CONFIG_FILE);
    fs::write(&config_path, minimal_config).expect("failed to write peppy.json5");

    // Generate should succeed even with no interfaces
    generate_peppygen_lib(
        PeppygenLanguage::Python,
        node_dir,
        Vec::new(),
        "test-hash",
        &helpers::test_peppy_dirs(),
        Default::default(),
        None,
    )
    .expect("failed to generate library for minimal config");

    // Verify the generated library exists
    let peppygen_dir = node_dir.join(PEPPYGEN_OUTPUT_PATH);
    assert!(peppygen_dir.exists(), "peppygen directory should exist");
}

#[test]
fn generate_peppygen_lib_missing_config() {
    let temp_dir = TempDir::new().expect("failed to create temp directory");
    let node_dir = temp_dir.path();

    // Try to generate without a peppy.json5 - should fail
    let result = generate_peppygen_lib(
        PeppygenLanguage::Python,
        node_dir,
        Vec::new(),
        "test-hash",
        &helpers::test_peppy_dirs(),
        Default::default(),
        None,
    );
    assert!(result.is_err(), "should fail when peppy.json5 is missing");
}

#[test]
fn generate_peppygen_python_lib_emitted_and_consumed_topics() {
    const EXPOSED_NODE_NAME: &str = "topic_exposer";
    const CONSUMER_NODE_NAME: &str = "topic_consumer";

    let exposed_dir = TempDir::new().expect("failed to create temp directory");
    let exposed_node_dir = exposed_dir.path();

    let exposed_config = r#"{
          schema_version: 1,
          manifest: {
            name: "{EXPOSED_NODE_NAME}",
            tag: "0.1.0",
          },
          interfaces: {
            topics: {
              emits: [
                {
                  name: "test_topic",
                  qos_profile: "sensor_data",
                  message_format: {
                    value: "u32"
                  }
                }
              ]
            }
          },
          execution: {
            language: "python",
            add_cmd: ["uv", "sync"],
            start_cmd: ["uv", "run", "{EXPOSED_NODE_NAME}"]
          },
        }"#
    .replace("{EXPOSED_NODE_NAME}", EXPOSED_NODE_NAME);

    fs::write(exposed_node_dir.join(NODE_CONFIG_FILE), exposed_config)
        .expect("failed to write exposed peppy.json5");

    generate_peppygen_lib(
        PeppygenLanguage::Python,
        exposed_node_dir,
        Vec::new(),
        "test-hash",
        &helpers::test_peppy_dirs(),
        Default::default(),
        None,
    )
    .expect("failed to generate peppygen lib for exposed node");

    let exposed_peppygen_dir = exposed_node_dir.join(PEPPYGEN_OUTPUT_PATH);
    assert!(
        exposed_peppygen_dir
            .join("peppygen/emitted_topics/test_topic.py")
            .exists(),
        "emitted topic module should exist in peppygen package at {}",
        exposed_peppygen_dir.display()
    );

    let consumer_dir = TempDir::new().expect("failed to create temp directory");
    let consumer_node_dir = consumer_dir.path();

    let consumer_config = r#"{
          schema_version: 1,
          manifest: {
            name: "{CONSUMER_NODE_NAME}",
            tag: "0.1.0",
          },
          execution: {
            language: "python",
            add_cmd: ["uv", "sync"],
            start_cmd: ["uv", "run", "{CONSUMER_NODE_NAME}"]
          }
        }"#
    .replace("{CONSUMER_NODE_NAME}", CONSUMER_NODE_NAME);
    fs::write(consumer_node_dir.join(NODE_CONFIG_FILE), consumer_config)
        .expect("failed to write consumer peppy.json5");

    let consumed_topic: ConsumedTopic = serde_json5::from_str(&format!(
        r#"{{
          local_node_id: "{EXPOSED_NODE_NAME}",
          name: "test_topic",
        }}"#
    ))
    .expect("failed to parse consumed topic");

    let consumed_format: MessageFormat =
        serde_json5::from_str(r#"{ value: "u32" }"#).expect("failed to parse topic format");

    let expected_interfaces = vec![DeploymentInterface::new(InterfaceVariant::ConsumedTopic {
        topic: consumed_topic,
        message_format: consumed_format,
        dependency_node_name: String::from(EXPOSED_NODE_NAME),
    })];

    generate_peppygen_lib(
        PeppygenLanguage::Python,
        consumer_node_dir,
        expected_interfaces,
        "test-hash",
        &helpers::test_peppy_dirs(),
        Default::default(),
        None,
    )
    .expect("failed to generate peppygen lib for consumer node");

    let consumer_peppygen_dir = consumer_node_dir.join(PEPPYGEN_OUTPUT_PATH);
    assert!(
        consumer_peppygen_dir
            .join("peppygen/consumed_topics/topic_exposer_test_topic.py")
            .exists(),
        "consumed topic module should exist in peppygen package at {}",
        consumer_peppygen_dir.display()
    );
}

#[test]
fn generate_peppygen_python_lib_exposed_and_consumed_services() {
    const EXPOSED_NODE_NAME: &str = "service_exposer";
    const CONSUMER_NODE_NAME: &str = "service_consumer";

    let exposed_dir = TempDir::new().expect("failed to create temp directory");
    let exposed_node_dir = exposed_dir.path();

    let exposed_config = r#"{
          schema_version: 1,
          manifest: {
            name: "{EXPOSED_NODE_NAME}",
            tag: "0.1.0",
          },
          interfaces: {
            services: {
              exposes: [
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
          },
          execution: {
            language: "python",
            add_cmd: ["uv", "sync"],
            start_cmd: ["uv", "run", "{EXPOSED_NODE_NAME}"]
          },
        }"#
    .replace("{EXPOSED_NODE_NAME}", EXPOSED_NODE_NAME);

    fs::write(exposed_node_dir.join(NODE_CONFIG_FILE), exposed_config)
        .expect("failed to write exposed peppy.json5");

    generate_peppygen_lib(
        PeppygenLanguage::Python,
        exposed_node_dir,
        Vec::new(),
        "test-hash",
        &helpers::test_peppy_dirs(),
        Default::default(),
        None,
    )
    .expect("failed to generate peppygen lib for exposed node");

    let exposed_peppygen_dir = exposed_node_dir.join(PEPPYGEN_OUTPUT_PATH);
    assert!(
        exposed_peppygen_dir
            .join("peppygen/exposed_services/test_service.py")
            .exists(),
        "exposed service module should exist in peppygen package at {}",
        exposed_peppygen_dir.display()
    );

    let consumer_dir = TempDir::new().expect("failed to create temp directory");
    let consumer_node_dir = consumer_dir.path();

    let consumer_config = r#"{
          schema_version: 1,
          manifest: {
            name: "{CONSUMER_NODE_NAME}",
            tag: "0.1.0",
          },
          execution: {
            language: "python",
            add_cmd: ["uv", "sync"],
            start_cmd: ["uv", "run", "{CONSUMER_NODE_NAME}"]
          }
        }"#
    .replace("{CONSUMER_NODE_NAME}", CONSUMER_NODE_NAME);
    fs::write(consumer_node_dir.join(NODE_CONFIG_FILE), consumer_config)
        .expect("failed to write consumer peppy.json5");

    let consumed_service: ConsumedService = serde_json5::from_str(&format!(
        r#"{{
          local_node_id: "{EXPOSED_NODE_NAME}",
          name: "test_service",
        }}"#
    ))
    .expect("failed to parse consumed service");

    let request_format: MessageFormat =
        serde_json5::from_str(r#"{ input: "string" }"#).expect("failed to parse request format");
    let response_format: MessageFormat =
        serde_json5::from_str(r#"{ output: "string", success: "bool" }"#)
            .expect("failed to parse response format");

    let consumed_interfaces = vec![DeploymentInterface::new(
        InterfaceVariant::ConsumedService {
            service: consumed_service,
            request_format,
            response_format,
            dependency_node_name: String::from(EXPOSED_NODE_NAME),
        },
    )];

    generate_peppygen_lib(
        PeppygenLanguage::Python,
        consumer_node_dir,
        consumed_interfaces,
        "test-hash",
        &helpers::test_peppy_dirs(),
        Default::default(),
        None,
    )
    .expect("failed to generate peppygen lib for consumer node");

    let consumer_peppygen_dir = consumer_node_dir.join(PEPPYGEN_OUTPUT_PATH);
    let consumed_service_module_path =
        consumer_peppygen_dir.join("peppygen/consumed_services/service_exposer_test_service.py");
    assert!(
        consumed_service_module_path.exists(),
        "consumed service module should exist in peppygen package at {}",
        consumer_peppygen_dir.display()
    );

    let consumed_service_code = fs::read_to_string(&consumed_service_module_path)
        .expect("failed to read consumed service module");
    assert!(
        consumed_service_code.contains("async def poll("),
        "consumed service module should define a poll() function"
    );
}

#[test]
fn generate_peppygen_python_lib_exposed_and_consumed_actions() {
    const EXPOSED_NODE_NAME: &str = "action_exposer";
    const CONSUMER_NODE_NAME: &str = "action_consumer";

    let exposed_dir = TempDir::new().expect("failed to create temp directory");
    let exposed_node_dir = exposed_dir.path();

    let exposed_config = r#"{
          schema_version: 1,
          manifest: {
            name: "{EXPOSED_NODE_NAME}",
            tag: "0.1.0",
          },
          interfaces: {
            actions: {
              exposes: [
                {
                  name: "test_action",
                  goal_service: {
                    request_message_format: {
                      value: "u32"
                    },
                    response_message_format: {
                      accepted: "bool"
                    }
                  },
                  feedback_topic: {
                    qos_profile: "sensor_data",
                    message_format: {
                      progress: "u8"
                    }
                  },
                  result_service: {
                    response_message_format: {
                      success: "bool"
                    }
                  }
                }
              ]
            }
          },
          execution: {
            language: "python",
            add_cmd: ["uv", "sync"],
            start_cmd: ["uv", "run", "{EXPOSED_NODE_NAME}"]
          },
        }"#
    .replace("{EXPOSED_NODE_NAME}", EXPOSED_NODE_NAME);

    fs::write(exposed_node_dir.join(NODE_CONFIG_FILE), exposed_config)
        .expect("failed to write exposed peppy.json5");

    generate_peppygen_lib(
        PeppygenLanguage::Python,
        exposed_node_dir,
        Vec::new(),
        "test-hash",
        &helpers::test_peppy_dirs(),
        Default::default(),
        None,
    )
    .expect("failed to generate peppygen lib for exposed node");

    let exposed_peppygen_dir = exposed_node_dir.join(PEPPYGEN_OUTPUT_PATH);
    assert!(
        exposed_peppygen_dir
            .join("peppygen/exposed_actions/test_action.py")
            .exists(),
        "exposed action module should exist in peppygen package at {}",
        exposed_peppygen_dir.display()
    );

    let consumer_dir = TempDir::new().expect("failed to create temp directory");
    let consumer_node_dir = consumer_dir.path();

    let consumer_config = r#"{
          schema_version: 1,
          manifest: {
            name: "{CONSUMER_NODE_NAME}",
            tag: "0.1.0",
          },
          execution: {
            language: "python",
            add_cmd: ["uv", "sync"],
            start_cmd: ["uv", "run", "{CONSUMER_NODE_NAME}"]
          }
        }"#
    .replace("{CONSUMER_NODE_NAME}", CONSUMER_NODE_NAME);
    fs::write(consumer_node_dir.join(NODE_CONFIG_FILE), consumer_config)
        .expect("failed to write consumer peppy.json5");

    let consumed_action: ConsumedAction = serde_json5::from_str(&format!(
        r#"{{
          local_node_id: "{EXPOSED_NODE_NAME}",
          name: "test_action",
        }}"#
    ))
    .expect("failed to parse consumed action");

    let goal_request_format: MessageFormat =
        serde_json5::from_str(r#"{ value: "u32" }"#).expect("failed to parse goal request format");
    let goal_response_format: MessageFormat = serde_json5::from_str(r#"{ accepted: "bool" }"#)
        .expect("failed to parse goal response format");
    let feedback_format: MessageFormat =
        serde_json5::from_str(r#"{ progress: "u8" }"#).expect("failed to parse feedback format");
    let result_response_format: MessageFormat = serde_json5::from_str(r#"{ success: "bool" }"#)
        .expect("failed to parse result response format");

    let action_messages = ConsumedActionMessage {
        goal_request: Some(goal_request_format),
        goal_response: Some(goal_response_format),
        feedback: Some(feedback_format),
        result_request: None,
        result_response: Some(result_response_format),
    };

    let consumed_interfaces = vec![DeploymentInterface::new(InterfaceVariant::ConsumedAction {
        action: consumed_action,
        messages: action_messages,
        dependency_node_name: String::from(EXPOSED_NODE_NAME),
    })];

    generate_peppygen_lib(
        PeppygenLanguage::Python,
        consumer_node_dir,
        consumed_interfaces,
        "test-hash",
        &helpers::test_peppy_dirs(),
        Default::default(),
        None,
    )
    .expect("failed to generate peppygen lib for consumer node");

    let consumer_peppygen_dir = consumer_node_dir.join(PEPPYGEN_OUTPUT_PATH);
    assert!(
        consumer_peppygen_dir
            .join("peppygen/consumed_actions/action_exposer_test_action.py")
            .exists(),
        "consumed action module should exist in peppygen package at {}",
        consumer_peppygen_dir.display()
    );
}
