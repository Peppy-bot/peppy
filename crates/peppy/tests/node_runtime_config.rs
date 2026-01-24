use peppy::test_support::{LogCapture, ServeCommandEmulation};
use std::sync::Arc;

use peppy::commands::Command;
use peppy::commands::node::{NodeCommand, NodeCommands, NodeName};
use peppy::context::AppContext;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_runtime_config_command_outputs_valid_config() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let master_node_name = serve.master_node_name().to_string();
    assert!(
        !master_node_name.is_empty(),
        "master_node_name should not be empty"
    );

    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_runtime_config_node";

    let node_ctx = Arc::new(
        AppContext::with_messenger(node_dir.path(), shared_messenger.clone())
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    let log_capture = LogCapture::new();
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

    let node_path = node_dir.path().join(node_name);
    let peppy_json5_path = node_path.join("peppy.json5");
    assert!(
        peppy_json5_path.exists(),
        "peppy.json5 should exist at {}",
        peppy_json5_path.display()
    );

    peppy::test_support::disable_add_cmd(&peppy_json5_path);

    NodeCommand {
        command: NodeCommands::Add {
            node_dir: node_path,
            start: false,
            args: Vec::new(),
            instance_id: None,
            timeout: 60,
        },
    }
    .execute(&node_ctx)
    .expect("node add command should succeed");

    NodeCommand {
        command: NodeCommands::RuntimeConfig {
            node_name: Some(node_name.to_string()),
            node_dir: None,
            args: Vec::new(),
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
        config::consts::DEFAULT_MESSAGING_HOST
    );
    assert_eq!(
        runtime_config.messaging_port,
        config::consts::DEFAULT_MESSAGING_PORT
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_runtime_config_command_with_peppy_json5_outputs_valid_config() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let master_node_name = serve.master_node_name().to_string();
    assert!(
        !master_node_name.is_empty(),
        "master_node_name should not be empty"
    );

    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_runtime_config_json5_node";

    let node_ctx = Arc::new(
        AppContext::with_messenger(node_dir.path(), shared_messenger.clone())
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    let log_capture = LogCapture::new();
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

    let node_path = node_dir.path().join(node_name);
    let peppy_json5_path = node_path.join("peppy.json5");
    assert!(
        peppy_json5_path.exists(),
        "peppy.json5 should exist at {}",
        peppy_json5_path.display()
    );

    peppy::test_support::disable_add_cmd(&peppy_json5_path);

    NodeCommand {
        command: NodeCommands::Add {
            node_dir: node_path.clone(),
            start: false,
            args: Vec::new(),
            instance_id: None,
            timeout: 60,
        },
    }
    .execute(&node_ctx)
    .expect("node add command should succeed");

    // Use node_dir instead of node_name
    NodeCommand {
        command: NodeCommands::RuntimeConfig {
            node_name: None,
            node_dir: Some(node_path),
            args: Vec::new(),
        },
    }
    .execute(&node_ctx)
    .expect("node runtime-config command with node_dir should succeed");

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
        config::consts::DEFAULT_MESSAGING_HOST
    );
    assert_eq!(
        runtime_config.messaging_port,
        config::consts::DEFAULT_MESSAGING_PORT
    );
    assert_eq!(runtime_config.node_name, node_name);
    assert_eq!(runtime_config.bound_master_node, master_node_name.as_str());
}
