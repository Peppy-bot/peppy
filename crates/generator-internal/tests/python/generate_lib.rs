use crate::helpers;
use config::{
    consts::{NODE_CONFIG_FILE, PEPPYGEN_OUTPUT_PATH},
    node::{MessageFormat, PeppygenLanguage, SubscribedAction, SubscribedService, SubscribedTopic},
};
use generator::generate_peppygen_lib;
use generator::{DeploymentInterface, InterfaceVariant, SubscribedActionMessage};
use std::fs;
use tempfile::TempDir;

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

    // Check that exposed topics module was generated
    let exposed_topics_dir = peppygen_dir.join("peppygen/exposed_topics");
    assert!(
        exposed_topics_dir.exists(),
        "exposed_topics directory should exist at {}",
        exposed_topics_dir.display()
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
      manifest: {
        name: "minimal_node",
        tag: "0.1.0",
        language: "python",
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
    );
    assert!(result.is_err(), "should fail when peppy.json5 is missing");
}

#[test]
fn generate_peppygen_python_lib_exposed_and_subscribed_topics() {
    const EXPOSED_NODE_NAME: &str = "topic_exposer";
    const SUBSCRIBER_NODE_NAME: &str = "topic_subscriber";

    let exposed_dir = TempDir::new().expect("failed to create temp directory");
    let exposed_node_dir = exposed_dir.path();

    let exposed_config = format!(
        r#"{{
          schema_version: 1,
          manifest: {{
            name: "{EXPOSED_NODE_NAME}",
            tag: "0.1.0",
            language: "python",
            add_cmd: ["uv", "sync"],
            start_cmd: ["uv", "run", "{EXPOSED_NODE_NAME}"]
          }},
          interfaces: {{
            exposes: {{
              topics: [
                {{
                  name: "test_topic",
                  qos_profile: "sensor_data",
                  message_format: {{
                    value: "u32"
                  }}
                }}
              ]
            }}
          }}
        }}"#
    );

    fs::write(exposed_node_dir.join(NODE_CONFIG_FILE), exposed_config)
        .expect("failed to write exposed peppy.json5");

    generate_peppygen_lib(
        PeppygenLanguage::Python,
        exposed_node_dir,
        Vec::new(),
        "test-hash",
        &helpers::test_peppy_dirs(),
    )
    .expect("failed to generate peppygen lib for exposed node");

    let exposed_peppygen_dir = exposed_node_dir.join(PEPPYGEN_OUTPUT_PATH);
    assert!(
        exposed_peppygen_dir
            .join("peppygen/exposed_topics/test_topic.py")
            .exists(),
        "exposed topic module should exist in peppygen package at {}",
        exposed_peppygen_dir.display()
    );

    let subscriber_dir = TempDir::new().expect("failed to create temp directory");
    let subscriber_node_dir = subscriber_dir.path();

    let subscriber_config = format!(
        r#"{{
          schema_version: 1,
          manifest: {{
            name: "{SUBSCRIBER_NODE_NAME}",
            tag: "0.1.0",
            language: "python",
            add_cmd: ["uv", "sync"],
            start_cmd: ["uv", "run", "{SUBSCRIBER_NODE_NAME}"]
          }}
        }}"#
    );
    fs::write(
        subscriber_node_dir.join(NODE_CONFIG_FILE),
        subscriber_config,
    )
    .expect("failed to write subscriber peppy.json5");

    let subscribed_topic: SubscribedTopic = serde_json5::from_str(&format!(
        r#"{{
          id: "test_topic_sub",
          node: "{EXPOSED_NODE_NAME}",
          name: "test_topic",
          tag: "0.1.0"
        }}"#
    ))
    .expect("failed to parse subscribed topic");

    let subscribed_format: MessageFormat =
        serde_json5::from_str(r#"{ value: "u32" }"#).expect("failed to parse topic format");

    let subscribed_interfaces = vec![DeploymentInterface::new(InterfaceVariant::SubscribedTopic(
        subscribed_topic,
        subscribed_format,
    ))];

    generate_peppygen_lib(
        PeppygenLanguage::Python,
        subscriber_node_dir,
        subscribed_interfaces,
        "test-hash",
        &helpers::test_peppy_dirs(),
    )
    .expect("failed to generate peppygen lib for subscriber node");

    let subscriber_peppygen_dir = subscriber_node_dir.join(PEPPYGEN_OUTPUT_PATH);
    assert!(
        subscriber_peppygen_dir
            .join("peppygen/subscribed_topics/topic_exposer_test_topic.py")
            .exists(),
        "subscribed topic module should exist in peppygen package at {}",
        subscriber_peppygen_dir.display()
    );
}

#[test]
fn generate_peppygen_python_lib_exposed_and_subscribed_services() {
    const EXPOSED_NODE_NAME: &str = "service_exposer";
    const SUBSCRIBER_NODE_NAME: &str = "service_subscriber";

    let exposed_dir = TempDir::new().expect("failed to create temp directory");
    let exposed_node_dir = exposed_dir.path();

    let exposed_config = format!(
        r#"{{
          schema_version: 1,
          manifest: {{
            name: "{EXPOSED_NODE_NAME}",
            tag: "0.1.0",
            language: "python",
            add_cmd: ["uv", "sync"],
            start_cmd: ["uv", "run", "{EXPOSED_NODE_NAME}"]
          }},
          interfaces: {{
            exposes: {{
              services: [
                {{
                  name: "test_service",
                  request_message_format: {{
                    input: "string"
                  }},
                  response_message_format: {{
                    output: "string",
                    success: "bool"
                  }}
                }}
              ]
            }}
          }}
        }}"#
    );

    fs::write(exposed_node_dir.join(NODE_CONFIG_FILE), exposed_config)
        .expect("failed to write exposed peppy.json5");

    generate_peppygen_lib(
        PeppygenLanguage::Python,
        exposed_node_dir,
        Vec::new(),
        "test-hash",
        &helpers::test_peppy_dirs(),
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

    let subscriber_dir = TempDir::new().expect("failed to create temp directory");
    let subscriber_node_dir = subscriber_dir.path();

    let subscriber_config = format!(
        r#"{{
          schema_version: 1,
          manifest: {{
            name: "{SUBSCRIBER_NODE_NAME}",
            tag: "0.1.0",
            language: "python",
            add_cmd: ["uv", "sync"],
            start_cmd: ["uv", "run", "{SUBSCRIBER_NODE_NAME}"]
          }}
        }}"#
    );
    fs::write(
        subscriber_node_dir.join(NODE_CONFIG_FILE),
        subscriber_config,
    )
    .expect("failed to write subscriber peppy.json5");

    let subscribed_service: SubscribedService = serde_json5::from_str(&format!(
        r#"{{
          id: "test_service_sub",
          node: "{EXPOSED_NODE_NAME}",
          name: "test_service",
          tag: "0.1.0"
        }}"#
    ))
    .expect("failed to parse subscribed service");

    let request_format: MessageFormat =
        serde_json5::from_str(r#"{ input: "string" }"#).expect("failed to parse request format");
    let response_format: MessageFormat =
        serde_json5::from_str(r#"{ output: "string", success: "bool" }"#)
            .expect("failed to parse response format");

    let subscribed_interfaces = vec![DeploymentInterface::new(
        InterfaceVariant::SubscribedService(subscribed_service, request_format, response_format),
    )];

    generate_peppygen_lib(
        PeppygenLanguage::Python,
        subscriber_node_dir,
        subscribed_interfaces,
        "test-hash",
        &helpers::test_peppy_dirs(),
    )
    .expect("failed to generate peppygen lib for subscriber node");

    let subscriber_peppygen_dir = subscriber_node_dir.join(PEPPYGEN_OUTPUT_PATH);
    let subscribed_service_module_path = subscriber_peppygen_dir
        .join("peppygen/subscribed_services/service_exposer_test_service.py");
    assert!(
        subscribed_service_module_path.exists(),
        "subscribed service module should exist in peppygen package at {}",
        subscriber_peppygen_dir.display()
    );

    let subscribed_service_code = fs::read_to_string(&subscribed_service_module_path)
        .expect("failed to read subscribed service module");
    assert!(
        subscribed_service_code.contains("async def poll("),
        "subscribed service module should define a poll() function"
    );
}

#[test]
fn generate_peppygen_python_lib_exposed_and_subscribed_actions() {
    const EXPOSED_NODE_NAME: &str = "action_exposer";
    const SUBSCRIBER_NODE_NAME: &str = "action_subscriber";

    let exposed_dir = TempDir::new().expect("failed to create temp directory");
    let exposed_node_dir = exposed_dir.path();

    let exposed_config = format!(
        r#"{{
          schema_version: 1,
          manifest: {{
            name: "{EXPOSED_NODE_NAME}",
            tag: "0.1.0",
            language: "python",
            add_cmd: ["uv", "sync"],
            start_cmd: ["uv", "run", "{EXPOSED_NODE_NAME}"]
          }},
          interfaces: {{
            exposes: {{
              actions: [
                {{
                  name: "test_action",
                  goal_service: {{
                    request_message_format: {{
                      value: "u32"
                    }},
                    response_message_format: {{
                      accepted: "bool"
                    }}
                  }},
                  feedback_topic: {{
                    qos_profile: "sensor_data",
                    message_format: {{
                      progress: "u8"
                    }}
                  }},
                  result_service: {{
                    response_message_format: {{
                      success: "bool"
                    }}
                  }}
                }}
              ]
            }}
          }}
        }}"#
    );

    fs::write(exposed_node_dir.join(NODE_CONFIG_FILE), exposed_config)
        .expect("failed to write exposed peppy.json5");

    generate_peppygen_lib(
        PeppygenLanguage::Python,
        exposed_node_dir,
        Vec::new(),
        "test-hash",
        &helpers::test_peppy_dirs(),
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

    let subscriber_dir = TempDir::new().expect("failed to create temp directory");
    let subscriber_node_dir = subscriber_dir.path();

    let subscriber_config = format!(
        r#"{{
          schema_version: 1,
          manifest: {{
            name: "{SUBSCRIBER_NODE_NAME}",
            tag: "0.1.0",
            language: "python",
            add_cmd: ["uv", "sync"],
            start_cmd: ["uv", "run", "{SUBSCRIBER_NODE_NAME}"]
          }}
        }}"#
    );
    fs::write(
        subscriber_node_dir.join(NODE_CONFIG_FILE),
        subscriber_config,
    )
    .expect("failed to write subscriber peppy.json5");

    let subscribed_action: SubscribedAction = serde_json5::from_str(&format!(
        r#"{{
          id: "test_action_sub",
          node: "{EXPOSED_NODE_NAME}",
          name: "test_action",
          tag: "0.1.0"
        }}"#
    ))
    .expect("failed to parse subscribed action");

    let goal_request_format: MessageFormat =
        serde_json5::from_str(r#"{ value: "u32" }"#).expect("failed to parse goal request format");
    let goal_response_format: MessageFormat = serde_json5::from_str(r#"{ accepted: "bool" }"#)
        .expect("failed to parse goal response format");
    let feedback_format: MessageFormat =
        serde_json5::from_str(r#"{ progress: "u8" }"#).expect("failed to parse feedback format");
    let result_response_format: MessageFormat = serde_json5::from_str(r#"{ success: "bool" }"#)
        .expect("failed to parse result response format");

    let action_messages = SubscribedActionMessage {
        goal_request: Some(goal_request_format),
        goal_response: Some(goal_response_format),
        feedback: Some(feedback_format),
        result_request: None,
        result_response: Some(result_response_format),
    };

    let subscribed_interfaces = vec![DeploymentInterface::new(
        InterfaceVariant::SubscribedAction(subscribed_action, action_messages),
    )];

    generate_peppygen_lib(
        PeppygenLanguage::Python,
        subscriber_node_dir,
        subscribed_interfaces,
        "test-hash",
        &helpers::test_peppy_dirs(),
    )
    .expect("failed to generate peppygen lib for subscriber node");

    let subscriber_peppygen_dir = subscriber_node_dir.join(PEPPYGEN_OUTPUT_PATH);
    assert!(
        subscriber_peppygen_dir
            .join("peppygen/subscribed_actions/action_exposer_test_action.py")
            .exists(),
        "subscribed action module should exist in peppygen package at {}",
        subscriber_peppygen_dir.display()
    );
}
