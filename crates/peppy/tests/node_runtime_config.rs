mod helpers;

use std::sync::Arc;

use helpers::TestServeHandle;
use peppy::commands::Command;
use peppy::commands::node::{NodeCommand, NodeCommands, NodeName};
use peppy::context::{AppContext, DaemonState};

#[test]
fn node_runtime_config_command_outputs_valid_config() {
    let _serial_guard = helpers::serve_test_guard();
    let serve = TestServeHandle::with_mock_messenger();

    let daemon_state = DaemonState::read().expect("daemon state should be readable");
    let master_node_name = daemon_state.master_node_name;
    assert!(
        !master_node_name.is_empty(),
        "master_node_name should not be empty"
    );

    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_runtime_config_node";

    let node_ctx = Arc::new(AppContext::with_messenger(
        node_dir.path(),
        serve.messenger(),
    ));

    let log_capture = serve.log_capture().clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    NodeCommand {
        command: NodeCommands::Init {
            node_name: NodeName::new(node_name).expect("valid node name"),
            to_dir: None,
            build_system: config::peppy_config::BuildSystem::Rust,
        },
    }
    .execute(&node_ctx)
    .expect("node init command should succeed");

    let peppy_json5_path = node_dir.path().join(node_name).join("peppy.json5");
    assert!(
        peppy_json5_path.exists(),
        "peppy.json5 should exist at {}",
        peppy_json5_path.display()
    );

    NodeCommand {
        command: NodeCommands::Add {
            peppy_json5: peppy_json5_path,
            start: false,
            args: Vec::new(),
            instance_id: None,
        },
    }
    .execute(&node_ctx)
    .expect("node add command should succeed");

    NodeCommand {
        command: NodeCommands::RuntimeConfig {
            node_name: Some(node_name.to_string()),
            peppy_json5: None,
        },
    }
    .execute(&node_ctx)
    .expect("node runtime-config command should succeed");

    let logs = log_capture.logs();
    let prefix = format!("{}=", config::consts::RUNTIME_CONFIG_VAR_NAME);
    let start = logs
        .find(&prefix)
        .unwrap_or_else(|| panic!("logs should contain runtime config output. Logs:\n{logs}"))
        + prefix.len();
    let end = logs[start..]
        .find('\n')
        .map(|offset| start + offset)
        .unwrap_or_else(|| logs.len());

    let runtime_config_json = logs[start..end].trim();
    let runtime_config: config::runtime::RuntimeConfig =
        serde_json::from_str(runtime_config_json).expect("runtime config JSON should parse");

    assert_eq!(
        runtime_config.messaging_host,
        config::consts::DEFAULT_ZENOH_HOST
    );
    assert_eq!(
        runtime_config.messaging_port,
        config::consts::DEFAULT_ZENOH_PORT
    );
    assert_eq!(runtime_config.node_name, node_name);
    assert_eq!(runtime_config.bound_master_node, master_node_name.as_str());
    assert!(
        runtime_config.deployment_instance.arguments.is_empty(),
        "deployment_instance.arguments should be empty"
    );
    assert!(
        !runtime_config
            .deployment_instance
            .instance_id
            .as_str()
            .is_empty(),
        "deployment_instance.instance_id should not be empty"
    );
}

#[test]
fn node_runtime_config_command_with_peppy_json5_outputs_valid_config() {
    let _serial_guard = helpers::serve_test_guard();
    let serve = TestServeHandle::with_mock_messenger();

    let daemon_state = DaemonState::read().expect("daemon state should be readable");
    let master_node_name = daemon_state.master_node_name;
    assert!(
        !master_node_name.is_empty(),
        "master_node_name should not be empty"
    );

    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_runtime_config_json5_node";

    let node_ctx = Arc::new(AppContext::with_messenger(
        node_dir.path(),
        serve.messenger(),
    ));

    let log_capture = serve.log_capture().clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    NodeCommand {
        command: NodeCommands::Init {
            node_name: NodeName::new(node_name).expect("valid node name"),
            to_dir: None,
            build_system: config::peppy_config::BuildSystem::Rust,
        },
    }
    .execute(&node_ctx)
    .expect("node init command should succeed");

    let peppy_json5_path = node_dir.path().join(node_name).join("peppy.json5");
    assert!(
        peppy_json5_path.exists(),
        "peppy.json5 should exist at {}",
        peppy_json5_path.display()
    );

    NodeCommand {
        command: NodeCommands::Add {
            peppy_json5: peppy_json5_path.clone(),
            start: false,
            args: Vec::new(),
            instance_id: None,
        },
    }
    .execute(&node_ctx)
    .expect("node add command should succeed");

    // Use peppy_json5 instead of node_name
    NodeCommand {
        command: NodeCommands::RuntimeConfig {
            node_name: None,
            peppy_json5: Some(peppy_json5_path),
        },
    }
    .execute(&node_ctx)
    .expect("node runtime-config command with peppy_json5 should succeed");

    let logs = log_capture.logs();
    let prefix = format!("{}=", config::consts::RUNTIME_CONFIG_VAR_NAME);
    let start = logs
        .find(&prefix)
        .unwrap_or_else(|| panic!("logs should contain runtime config output. Logs:\n{logs}"))
        + prefix.len();
    let end = logs[start..]
        .find('\n')
        .map(|offset| start + offset)
        .unwrap_or_else(|| logs.len());

    let runtime_config_json = logs[start..end].trim();
    let runtime_config: config::runtime::RuntimeConfig =
        serde_json::from_str(runtime_config_json).expect("runtime config JSON should parse");

    assert_eq!(
        runtime_config.messaging_host,
        config::consts::DEFAULT_ZENOH_HOST
    );
    assert_eq!(
        runtime_config.messaging_port,
        config::consts::DEFAULT_ZENOH_PORT
    );
    assert_eq!(runtime_config.node_name, node_name);
    assert_eq!(runtime_config.bound_master_node, master_node_name.as_str());
}
