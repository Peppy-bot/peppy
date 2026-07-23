use config::{
    consts::{NODE_CONFIG_FILE, PEPPYGEN_OUTPUT_PATH},
    node::{ConsumedAction, ConsumedService, ConsumedTopic, MessageFormat, PeppygenLanguage},
};
use daemon_config::consts::PEPPYLIB_OUTPUT_PATH;
use generator::generate_peppygen_lib;
use generator::{ConsumedActionMessage, DeploymentInterface, InterfaceVariant};
use std::fs;
use tempfile::TempDir;

use crate::helpers;

const PEPPY_JSON5_CONFIG: &str = r#"{
  peppy_schema: "node/v1",
  manifest: {
    name: "test_node",
    tag: "v1"
  },
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

  execution: {
    language: "rust",
    run_cmd: ["./target/release/test_node"]
  },
}"#;

#[test]
fn generate_peppygen_lib_minimal_config() {
    let temp_dir =
        TempDir::new_in(crate::helpers::test_tmp_root()).expect("failed to create temp directory");
    let node_dir = temp_dir.path();

    // Minimal config with no interfaces
    let minimal_config = r#"{
      peppy_schema: "node/v1",
      manifest: {
        name: "minimal_node",
        tag: "v1"
      },
      execution: {
        language: "rust",
        run_cmd: ["./target/debug/minimal_node"]
      }
    }"#;

    let config_path = node_dir.join(config::consts::NODE_CONFIG_FILE);
    fs::write(&config_path, minimal_config).expect("failed to write peppy.json5");

    // Generate should succeed even with no interfaces
    generate_peppygen_lib(
        PeppygenLanguage::Rust,
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

/// `CrateDeployMode::Copy` (used for container builds, where symlinks to host
/// paths break) must deploy the vendored crates as **real directories**, not
/// symlinks into the shared cache, the property container builds rely on.
/// Complements `generate_peppygen_lib_cargo`, which asserts the default
/// `Symlink` mode produces symlinks.
#[test]
fn generate_peppygen_lib_copy_mode_deploys_real_dirs() {
    let temp_dir =
        TempDir::new_in(crate::helpers::test_tmp_root()).expect("failed to create temp directory");
    let node_dir = temp_dir.path();

    let minimal_config = r#"{
      peppy_schema: "node/v1",
      manifest: {
        name: "copy_node",
        tag: "v1"
      },
      execution: {
        language: "rust",
        run_cmd: ["./target/debug/copy_node"]
      }
    }"#;
    let config_path = node_dir.join(config::consts::NODE_CONFIG_FILE);
    fs::write(&config_path, minimal_config).expect("failed to write peppy.json5");

    generate_peppygen_lib(
        PeppygenLanguage::Rust,
        node_dir,
        Vec::new(),
        "test-hash",
        &helpers::test_peppy_dirs(),
        generator::CrateDeployMode::Copy,
        None,
    )
    .expect("failed to generate library in Copy mode");

    let libs_dir = std::path::Path::new(PEPPYLIB_OUTPUT_PATH)
        .parent()
        .expect("PEPPYLIB_OUTPUT_PATH should have a parent directory");
    for crate_name in [
        "peppylib",
        "peppy-messaging-interface",
        "peppy-config-model",
    ] {
        let dest = node_dir.join(libs_dir).join(crate_name);
        let meta = dest.symlink_metadata().unwrap_or_else(|e| {
            panic!("{crate_name} should be deployed at {}: {e}", dest.display())
        });
        assert!(
            !meta.file_type().is_symlink(),
            "{crate_name} should be a real directory in Copy mode, not a symlink"
        );
        assert!(
            dest.join("Cargo.toml").is_file(),
            "{crate_name} should contain copied sources (Cargo.toml) in Copy mode"
        );
    }
}

#[test]
fn generate_peppygen_lib_cargo() {
    let (temp_dir, peppygen_dir) =
        helpers::run_generate_peppygen_lib_test(PeppygenLanguage::Rust, PEPPY_JSON5_CONFIG);
    let node_dir = temp_dir.path();

    // Check that Cargo.toml was generated
    let cargo_toml = peppygen_dir.join("Cargo.toml");
    assert!(
        cargo_toml.exists(),
        "Cargo.toml should exist at {}",
        cargo_toml.display()
    );

    // Check that src/lib.rs was generated
    let lib_rs = peppygen_dir.join("src/lib.rs");
    assert!(
        lib_rs.exists(),
        "src/lib.rs should exist at {}",
        lib_rs.display()
    );

    // Check that emitted topics module was generated
    let emitted_topics_dir = peppygen_dir.join("src/emitted_topics");
    assert!(
        emitted_topics_dir.exists(),
        "emitted_topics directory should exist at {}",
        emitted_topics_dir.display()
    );

    // Check that exposed services module was generated
    let exposed_services_dir = peppygen_dir.join("src/exposed_services");
    assert!(
        exposed_services_dir.exists(),
        "exposed_services directory should exist at {}",
        exposed_services_dir.display()
    );

    // Check that the Cargo.toml in node_dir has peppygen as dependency and points to the peppygen_dir path
    let node_cargo_toml = node_dir.join("Cargo.toml");
    assert!(
        node_cargo_toml.exists(),
        "Cargo.toml should be created in node_dir at {}",
        node_cargo_toml.display()
    );

    let cargo_contents = fs::read_to_string(&node_cargo_toml).expect("failed to read Cargo.toml");
    let cargo_doc: toml::Value =
        toml::from_str(&cargo_contents).expect("Cargo.toml should be valid TOML");

    // Verify package name matches the node name from config
    let package_name = cargo_doc
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .expect("Cargo.toml should have package.name");
    assert_eq!(
        package_name, "test_node",
        "package name should match node name from config"
    );

    // Verify peppygen dependency exists and points to the correct path
    let peppygen_dep = cargo_doc
        .get("dependencies")
        .and_then(|d| d.get("peppygen"))
        .expect("Cargo.toml should have peppygen dependency");

    let peppygen_path = peppygen_dep
        .get("path")
        .and_then(|p| p.as_str())
        .expect("peppygen dependency should have a path");
    assert_eq!(
        peppygen_path, PEPPYGEN_OUTPUT_PATH,
        "peppygen dependency path should point to {PEPPYGEN_OUTPUT_PATH}"
    );

    // Verify peppylib dependency exists and points to the correct path
    let peppylib_dep = cargo_doc
        .get("dependencies")
        .and_then(|d| d.get("peppylib"))
        .expect("Cargo.toml should have peppylib dependency");

    let peppylib_path = peppylib_dep
        .get("path")
        .and_then(|p| p.as_str())
        .expect("peppylib dependency should have a path");
    assert_eq!(
        peppylib_path, PEPPYLIB_OUTPUT_PATH,
        "peppylib dependency path should point to {PEPPYLIB_OUTPUT_PATH}"
    );

    // Verify the Rust crate symlinks exist.
    let libs_dir = std::path::Path::new(PEPPYLIB_OUTPUT_PATH)
        .parent()
        .expect("PEPPYLIB_OUTPUT_PATH should have a parent directory");
    for crate_name in [
        "peppylib",
        "peppy-messaging-interface",
        "peppy-config-model",
    ] {
        let link = node_dir.join(libs_dir).join(crate_name);
        let meta = link.symlink_metadata().unwrap_or_else(|e| {
            panic!(
                "{crate_name} symlink should exist at {}: {e}",
                link.display()
            )
        });
        assert!(
            meta.file_type().is_symlink(),
            "{crate_name} should be a symlink"
        );
    }

    // Verify the peppygen Cargo.toml uses the ../peppylib relative path
    let peppygen_cargo =
        fs::read_to_string(&cargo_toml).expect("failed to read peppygen Cargo.toml");
    assert!(
        peppygen_cargo.contains("../peppylib"),
        "peppygen Cargo.toml should reference peppylib via ../peppylib path"
    );
}

/// The clock helper is part of the generated library: `peppygen::clock`
/// must be a real module exposing both `init` and `now_ns`, and the
/// crate root must declare it so user code can call `peppygen::clock::*`.
#[test]
fn generate_peppygen_lib_emits_clock_module() {
    let (_temp_dir, peppygen_dir) =
        helpers::run_generate_peppygen_lib_test(PeppygenLanguage::Rust, PEPPY_JSON5_CONFIG);

    let clock_rs = peppygen_dir.join("src/clock.rs");
    assert!(
        clock_rs.exists(),
        "src/clock.rs should be emitted at {}",
        clock_rs.display()
    );
    let clock_contents = fs::read_to_string(&clock_rs).expect("failed to read clock.rs");
    assert!(
        clock_contents.contains("pub async fn init"),
        "clock.rs must expose `init`"
    );
    assert!(
        clock_contents.contains("pub fn now_ns"),
        "clock.rs must expose `now_ns`"
    );

    let lib_rs = peppygen_dir.join("src/lib.rs");
    let lib_contents = fs::read_to_string(&lib_rs).expect("failed to read lib.rs");
    assert!(
        lib_contents.contains("pub mod clock"),
        "lib.rs must declare the clock module"
    );
}

#[test]
fn generate_peppygen_lib_missing_config() {
    let temp_dir =
        TempDir::new_in(crate::helpers::test_tmp_root()).expect("failed to create temp directory");
    let node_dir = temp_dir.path();

    // Try to generate without a peppy.json5 - should fail
    let result = generate_peppygen_lib(
        PeppygenLanguage::Rust,
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
fn generate_peppygen_rust_lib_emitted_and_consumed_topics() {
    const EXPOSED_NODE_NAME: &str = "topic_exposer";
    const CONSUMER_NODE_NAME: &str = "topic_consumer";

    let exposed_dir =
        TempDir::new_in(crate::helpers::test_tmp_root()).expect("failed to create temp directory");
    let exposed_node_dir = exposed_dir.path();

    let exposed_config = r#"{
          peppy_schema: "node/v1",
          manifest: {
            name: "{EXPOSED_NODE_NAME}",
            tag: "v1",
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
            language: "rust",
            run_cmd: ["./target/debug/{EXPOSED_NODE_NAME}"]
          },
        }"#
    .replace("{EXPOSED_NODE_NAME}", EXPOSED_NODE_NAME);

    fs::write(exposed_node_dir.join(NODE_CONFIG_FILE), exposed_config)
        .expect("failed to write exposed peppy.json5");

    generate_peppygen_lib(
        PeppygenLanguage::Rust,
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
            .join("src/emitted_topics/test_topic.rs")
            .exists(),
        "emitted topic module should exist in peppygen crate at {}",
        exposed_peppygen_dir.display()
    );

    let consumer_dir =
        TempDir::new_in(crate::helpers::test_tmp_root()).expect("failed to create temp directory");
    let consumer_node_dir = consumer_dir.path();

    let consumer_config = r#"{
          peppy_schema: "node/v1",
          manifest: {
            name: "{CONSUMER_NODE_NAME}",
            tag: "v1",
          },
          execution: {
            language: "rust",
            run_cmd: ["./target/debug/{CONSUMER_NODE_NAME}"]
          }
        }"#
    .replace("{CONSUMER_NODE_NAME}", CONSUMER_NODE_NAME);
    fs::write(consumer_node_dir.join(NODE_CONFIG_FILE), consumer_config)
        .expect("failed to write consumer peppy.json5");

    let consumed_topic: ConsumedTopic = serde_json5::from_str(&format!(
        r#"{{
          link_id: "{EXPOSED_NODE_NAME}",
          name: "test_topic",
        }}"#
    ))
    .expect("failed to parse consumed topic");

    let consumed_format: MessageFormat =
        serde_json5::from_str(r#"{ value: "u32" }"#).expect("failed to parse topic format");

    let expected_interfaces = vec![DeploymentInterface::new(InterfaceVariant::ConsumedTopic {
        topic: consumed_topic,
        message_format: consumed_format,
        dependency: helpers::native_dep(EXPOSED_NODE_NAME, "v1", EXPOSED_NODE_NAME),
    })];

    generate_peppygen_lib(
        PeppygenLanguage::Rust,
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
            .join("src/consumed_topics/topic_exposer/test_topic.rs")
            .exists(),
        "consumed topic module should exist in peppygen crate at {}",
        consumer_peppygen_dir.display()
    );
}

#[test]
fn generate_peppygen_rust_lib_exposed_and_consumed_services() {
    const EXPOSED_NODE_NAME: &str = "service_exposer";
    const CONSUMER_NODE_NAME: &str = "service_consumer";

    let exposed_dir =
        TempDir::new_in(crate::helpers::test_tmp_root()).expect("failed to create temp directory");
    let exposed_node_dir = exposed_dir.path();

    let exposed_config = r#"{
          peppy_schema: "node/v1",
          manifest: {
            name: "{EXPOSED_NODE_NAME}",
            tag: "v1",
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
            language: "rust",
            run_cmd: ["./target/debug/{EXPOSED_NODE_NAME}"]
          },
        }"#
    .replace("{EXPOSED_NODE_NAME}", EXPOSED_NODE_NAME);

    fs::write(exposed_node_dir.join(NODE_CONFIG_FILE), exposed_config)
        .expect("failed to write exposed peppy.json5");

    generate_peppygen_lib(
        PeppygenLanguage::Rust,
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
            .join("src/exposed_services/test_service.rs")
            .exists(),
        "exposed service module should exist in peppygen crate at {}",
        exposed_peppygen_dir.display()
    );

    let consumer_dir =
        TempDir::new_in(crate::helpers::test_tmp_root()).expect("failed to create temp directory");
    let consumer_node_dir = consumer_dir.path();

    let consumer_config = r#"{
          peppy_schema: "node/v1",
          manifest: {
            name: "{CONSUMER_NODE_NAME}",
            tag: "v1",
          },
          execution: {
            language: "rust",
            run_cmd: ["./target/debug/{CONSUMER_NODE_NAME}"]
          }
        }"#
    .replace("{CONSUMER_NODE_NAME}", CONSUMER_NODE_NAME);
    fs::write(consumer_node_dir.join(NODE_CONFIG_FILE), consumer_config)
        .expect("failed to write consumer peppy.json5");

    let consumed_service: ConsumedService = serde_json5::from_str(&format!(
        r#"{{
          link_id: "{EXPOSED_NODE_NAME}",
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
            dependency: helpers::native_dep(EXPOSED_NODE_NAME, "v1", EXPOSED_NODE_NAME),
        },
    )];

    generate_peppygen_lib(
        PeppygenLanguage::Rust,
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
        consumer_peppygen_dir.join("src/consumed_services/service_exposer/test_service.rs");
    assert!(
        consumed_service_module_path.exists(),
        "consumed service module should exist in peppygen crate at {}",
        consumer_peppygen_dir.display()
    );

    let consumed_service_code = fs::read_to_string(&consumed_service_module_path)
        .expect("failed to read consumed service module");
    assert!(
        consumed_service_code.contains("pub async fn poll("),
        "consumed service module should define a poll() function"
    );
}

#[test]
fn generate_peppygen_rust_lib_exposed_and_consumed_actions() {
    const EXPOSED_NODE_NAME: &str = "action_exposer";
    const CONSUMER_NODE_NAME: &str = "action_consumer";

    let exposed_dir =
        TempDir::new_in(crate::helpers::test_tmp_root()).expect("failed to create temp directory");
    let exposed_node_dir = exposed_dir.path();

    let exposed_config = r#"{
          peppy_schema: "node/v1",
          manifest: {
            name: "{EXPOSED_NODE_NAME}",
            tag: "v1",
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
            language: "rust",
            run_cmd: ["./target/debug/{EXPOSED_NODE_NAME}"]
          },
        }"#
    .replace("{EXPOSED_NODE_NAME}", EXPOSED_NODE_NAME);

    fs::write(exposed_node_dir.join(NODE_CONFIG_FILE), exposed_config)
        .expect("failed to write exposed peppy.json5");

    generate_peppygen_lib(
        PeppygenLanguage::Rust,
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
            .join("src/exposed_actions/test_action.rs")
            .exists(),
        "exposed action module should exist in peppygen crate at {}",
        exposed_peppygen_dir.display()
    );

    let consumer_dir =
        TempDir::new_in(crate::helpers::test_tmp_root()).expect("failed to create temp directory");
    let consumer_node_dir = consumer_dir.path();

    let consumer_config = r#"{
          peppy_schema: "node/v1",
          manifest: {
            name: "{CONSUMER_NODE_NAME}",
            tag: "v1",
          },
          execution: {
            language: "rust",
            run_cmd: ["./target/debug/{CONSUMER_NODE_NAME}"]
          }
        }"#
    .replace("{CONSUMER_NODE_NAME}", CONSUMER_NODE_NAME);
    fs::write(consumer_node_dir.join(NODE_CONFIG_FILE), consumer_config)
        .expect("failed to write consumer peppy.json5");

    let consumed_action: ConsumedAction = serde_json5::from_str(&format!(
        r#"{{
          link_id: "{EXPOSED_NODE_NAME}",
          name: "test_action",
        }}"#
    ))
    .expect("failed to parse consumed action");

    let goal_request_format: MessageFormat =
        serde_json5::from_str(r#"{ value: "u32" }"#).expect("failed to parse goal request format");
    let feedback_format: MessageFormat =
        serde_json5::from_str(r#"{ progress: "u8" }"#).expect("failed to parse feedback format");
    let result_response_format: MessageFormat = serde_json5::from_str(r#"{ success: "bool" }"#)
        .expect("failed to parse result response format");

    let action_messages = ConsumedActionMessage {
        goal_request: Some(goal_request_format),
        feedback: Some(feedback_format),
        result_response: Some(result_response_format),
    };

    let consumed_interfaces = vec![DeploymentInterface::new(InterfaceVariant::ConsumedAction {
        action: consumed_action,
        messages: action_messages,
        dependency: helpers::native_dep(EXPOSED_NODE_NAME, "v1", EXPOSED_NODE_NAME),
    })];

    generate_peppygen_lib(
        PeppygenLanguage::Rust,
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
            .join("src/consumed_actions/action_exposer/test_action.rs")
            .exists(),
        "consumed action module should exist in peppygen crate at {}",
        consumer_peppygen_dir.display()
    );
}
