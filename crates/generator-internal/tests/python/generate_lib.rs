use crate::helpers;
use config::{
    consts::{NODE_CONFIG_FILE, PEPPYGEN_OUTPUT_PATH},
    node::{ConsumedAction, ConsumedService, ConsumedTopic, MessageFormat, PeppygenLanguage},
};
use daemon_config::consts::PEPPYLIB_OUTPUT_PATH;
use generator::generate_peppygen_lib;
use generator::{ConsumedActionMessage, DeploymentInterface, InterfaceVariant};
use std::fs;
use tempfile::TempDir;

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

  execution: { language: "python",
    build_cmd: ["uv", "sync"],
    run_cmd: ["uv", "run", "test_node"]
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

/// The clock helper is part of the generated library: `peppygen.clock`
/// must be a real module exposing both `init` and `now_ns`, and
/// `peppygen/__init__.py` must import it so user code can call
/// `peppygen.clock.*` directly.
#[test]
fn generate_peppygen_lib_emits_clock_module() {
    let (_temp_dir, peppygen_dir) =
        helpers::run_generate_peppygen_lib_test(PeppygenLanguage::Python, PEPPY_JSON5_CONFIG);

    let clock_py = peppygen_dir.join("peppygen/clock.py");
    assert!(
        clock_py.exists(),
        "peppygen/clock.py should be emitted at {}",
        clock_py.display()
    );
    let clock_contents = fs::read_to_string(&clock_py).expect("failed to read clock.py");
    assert!(
        clock_contents.contains("async def init"),
        "clock.py must expose `init`"
    );
    assert!(
        clock_contents.contains("def now_ns"),
        "clock.py must expose `now_ns`"
    );

    let init_py = peppygen_dir.join("peppygen/__init__.py");
    let init_contents = fs::read_to_string(&init_py).expect("failed to read __init__.py");
    assert!(
        init_contents.contains("from . import clock"),
        "__init__.py must import the clock module"
    );
}

/// Expected version of the deployed peppylib Python distribution: a PEP 440
/// local version that public indexes can never serve, so peppygen's exact pin
/// is only satisfiable from the `.peppy/libs/peppylib` path source.
const PEPPYLIB_DIST_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+peppy");

/// peppylib must be deployed as a standalone installable project at
/// `.peppy/libs/peppylib` (sibling of peppygen, mirroring the Rust layout),
/// and no longer nested inside the peppygen project.
#[test]
fn generate_peppygen_lib_deploys_standalone_peppylib_project() {
    let (_temp_dir, peppygen_dir) =
        helpers::run_generate_peppygen_lib_test(PeppygenLanguage::Python, PEPPY_JSON5_CONFIG);
    let libs_dir = peppygen_dir.parent().expect("peppygen dir has a parent");
    let project_dir = libs_dir.join("peppylib");

    let pyproject = fs::read_to_string(project_dir.join("pyproject.toml"))
        .expect("standalone peppylib pyproject.toml should exist");
    assert!(
        pyproject.contains(r#"name = "peppylib""#),
        "peppylib project must be named so the uv path source matches:\n{pyproject}"
    );
    assert!(
        pyproject.contains(&format!(r#"version = "{PEPPYLIB_DIST_VERSION}""#)),
        "peppylib project must carry the local version sentinel:\n{pyproject}"
    );

    assert!(
        project_dir.join("peppylib/__init__.py").exists(),
        "peppylib package wrappers should be deployed"
    );
    assert!(
        project_dir.join("peppylib/_peppylib.abi3.so").exists(),
        "canonical native extension should be deployed"
    );
    assert!(
        !project_dir.join("peppylib/.complete").exists(),
        "cache completeness marker must not be deployed"
    );

    assert!(
        !peppygen_dir.join("peppylib").exists(),
        "peppylib must no longer be nested inside the peppygen project"
    );

    let peppygen_pyproject = fs::read_to_string(peppygen_dir.join("pyproject.toml"))
        .expect("peppygen pyproject.toml should exist");
    assert!(
        peppygen_pyproject.contains(&format!("peppylib=={PEPPYLIB_DIST_VERSION}")),
        "peppygen must pin the deployed peppylib version:\n{peppygen_pyproject}"
    );
    assert!(
        !peppygen_pyproject.contains("peppylib*"),
        "peppygen must not package the peppylib package anymore:\n{peppygen_pyproject}"
    );
    assert!(
        !peppygen_pyproject.contains(r#"exclude = ["peppylib"]"#),
        "the ruff exclude for the nested peppylib copy should be gone:\n{peppygen_pyproject}"
    );
}

/// Re-generating into the same node directory must succeed and produce the
/// same layout (the deploy step removes and re-creates the standalone
/// peppylib project each time).
#[test]
fn generate_peppygen_lib_python_repeat_generation_is_idempotent() {
    let temp_dir =
        TempDir::new_in(crate::helpers::test_tmp_root()).expect("failed to create temp directory");
    let node_dir = temp_dir.path();
    fs::write(node_dir.join(NODE_CONFIG_FILE), PEPPY_JSON5_CONFIG)
        .expect("failed to write peppy.json5");

    for run in 1..=2 {
        generate_peppygen_lib(
            PeppygenLanguage::Python,
            node_dir,
            Vec::new(),
            "test-hash",
            &helpers::test_peppy_dirs(),
            Default::default(),
            None,
        )
        .unwrap_or_else(|e| panic!("generation run {run} failed: {e}"));
    }

    let peppygen_dir = node_dir.join(PEPPYGEN_OUTPUT_PATH);
    let project_dir = node_dir.join(PEPPYLIB_OUTPUT_PATH);
    assert!(project_dir.join("pyproject.toml").exists());
    assert!(project_dir.join("peppylib/_peppylib.abi3.so").exists());
    assert!(!peppygen_dir.join("peppylib").exists());
}

/// End-to-end: `uv sync` in a Python node project must install peppylib from
/// the deployed path source: `import peppylib` works directly (the raw
/// messaging API, without going through peppygen) and the installed
/// distribution is the locally deployed one, never a PyPI release.
#[test]
fn python_node_venv_installs_local_peppylib() {
    let temp_dir =
        TempDir::new_in(crate::helpers::test_tmp_root()).expect("failed to create temp directory");
    let node_dir = temp_dir.path().join("user_node");
    helpers::init_python_user_node(&node_dir);
    fs::write(
        node_dir.join(NODE_CONFIG_FILE),
        helpers::STUB_PYTHON_NODE_CONFIG,
    )
    .expect("failed to write peppy.json5");

    generate_peppygen_lib(
        PeppygenLanguage::Python,
        &node_dir,
        Vec::new(),
        "test-hash",
        &helpers::test_peppy_dirs(),
        Default::default(),
        None,
    )
    .expect("failed to generate peppygen lib");

    helpers::init_python_project_venv(&node_dir);

    let output = std::process::Command::new(node_dir.join(".venv/bin/python"))
        .args([
            "-c",
            "import importlib.metadata\n\
             import peppylib\n\
             from peppylib import QoSProfile\n\
             print(peppylib.__file__)\n\
             print(importlib.metadata.version('peppylib'))",
        ])
        .current_dir(&node_dir)
        .output()
        .expect("failed to run venv python");
    assert!(
        output.status.success(),
        "importing peppylib from the venv failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines();
    let module_path = lines.next().expect("missing peppylib.__file__ line");
    let dist_version = lines.next().expect("missing version line");
    assert!(
        module_path.contains(".venv") && module_path.contains("site-packages"),
        "peppylib must be installed into the project venv, got: {module_path}"
    );
    assert_eq!(
        dist_version, PEPPYLIB_DIST_VERSION,
        "installed peppylib must be the locally deployed distribution, not a PyPI release"
    );
}

#[test]
fn generate_peppygen_lib_minimal_config() {
    let temp_dir =
        TempDir::new_in(crate::helpers::test_tmp_root()).expect("failed to create temp directory");
    let node_dir = temp_dir.path();

    // Minimal config with no interfaces
    let minimal_config = r#"{
      peppy_schema: "node/v1",
      manifest: { name: "minimal_node",
        tag: "v1" },
      execution: { language: "python",
        build_cmd: ["uv", "sync"],
        run_cmd: ["uv", "run", "minimal_node"]
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
    let temp_dir =
        TempDir::new_in(crate::helpers::test_tmp_root()).expect("failed to create temp directory");
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
            language: "python",
            build_cmd: ["uv", "sync"],
            run_cmd: ["uv", "run", "{EXPOSED_NODE_NAME}"]
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
            language: "python",
            build_cmd: ["uv", "sync"],
            run_cmd: ["uv", "run", "{CONSUMER_NODE_NAME}"]
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
            language: "python",
            build_cmd: ["uv", "sync"],
            run_cmd: ["uv", "run", "{EXPOSED_NODE_NAME}"]
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
            language: "python",
            build_cmd: ["uv", "sync"],
            run_cmd: ["uv", "run", "{CONSUMER_NODE_NAME}"]
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
            language: "python",
            build_cmd: ["uv", "sync"],
            run_cmd: ["uv", "run", "{EXPOSED_NODE_NAME}"]
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
            language: "python",
            build_cmd: ["uv", "sync"],
            run_cmd: ["uv", "run", "{CONSUMER_NODE_NAME}"]
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
