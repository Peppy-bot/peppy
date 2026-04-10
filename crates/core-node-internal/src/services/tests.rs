use super::*;
use config::consts::PeppyDirs;
use pmi::{Messenger, MessengerAdapter, MessengerBackend, MockAdapter};
use std::sync::Arc;
use tokio::sync::Mutex;

async fn create_mock_messenger() -> Arc<Mutex<Messenger>> {
    let adapter = MockAdapter::default();
    let mut messenger = Messenger::new(MessengerAdapter::Mock(adapter));
    messenger
        .start_session()
        .await
        .expect("failed to start mock session");
    Arc::new(Mutex::new(messenger))
}

/// Verifies that the core node is configured to run in-process, not as a spawned
/// subprocess or inside a container.
#[tokio::test]
async fn core_node_execution_has_no_run_cmd_and_no_container() {
    let messenger = create_mock_messenger().await;
    let peppy_dirs = PeppyDirs::new(std::env::temp_dir());
    let node_arguments = CoreNodeArguments {
        node_startup_timeout: Duration::from_secs(5),
        node_start_health_timeout: Duration::from_secs(5),
        health_monitor_interval: Duration::from_secs(5),
        health_monitor_timeout: Duration::from_secs(3),
        health_monitor_max_failures: 3,
    };
    let core_node = CoreNode::new(
        messenger,
        Some("test_core_node"),
        node_arguments,
        std::env::temp_dir(),
        peppy_dirs,
    );

    let execution = &core_node.node_config().execution;
    assert!(
        execution.run_cmd.is_none(),
        "core node must not have a run_cmd (it runs in-process, not as a spawned process)"
    );
    assert!(
        execution.container.is_none(),
        "core node must not have a container config"
    );
}

/// Verifies that, with no explicit name, the core node derives a deterministic
/// machine-uid based name with the `core-node-` prefix. Two instances built on
/// the same machine must produce the same name.
#[tokio::test]
async fn core_node_default_name_is_deterministic_and_machine_uid_based() {
    let peppy_dirs = PeppyDirs::new(std::env::temp_dir());
    let mk = || CoreNodeArguments {
        node_startup_timeout: Duration::from_secs(5),
        node_start_health_timeout: Duration::from_secs(5),
        health_monitor_interval: Duration::from_secs(5),
        health_monitor_timeout: Duration::from_secs(3),
        health_monitor_max_failures: 3,
    };

    let a = CoreNode::new(
        create_mock_messenger().await,
        None,
        mk(),
        std::env::temp_dir(),
        peppy_dirs.clone(),
    );
    let b = CoreNode::new(
        create_mock_messenger().await,
        None,
        mk(),
        std::env::temp_dir(),
        peppy_dirs,
    );

    let name_a = a.node_config().manifest.name.as_str();
    let name_b = b.node_config().manifest.name.as_str();
    assert_eq!(
        name_a, name_b,
        "default core node name must be deterministic across instances on the same machine"
    );
    assert!(
        name_a.starts_with("core-node-"),
        "default core node name must use the `core-node-` prefix, got `{name_a}`"
    );
    assert!(
        name_a.len() > "core-node-".len(),
        "default core node name must include a machine-uid suffix, got `{name_a}`"
    );
}

/// Verifies the explicit `node_name` override still wins over the machine-uid default.
#[tokio::test]
async fn core_node_explicit_name_overrides_machine_uid() {
    let messenger = create_mock_messenger().await;
    let peppy_dirs = PeppyDirs::new(std::env::temp_dir());
    let node_arguments = CoreNodeArguments {
        node_startup_timeout: Duration::from_secs(5),
        node_start_health_timeout: Duration::from_secs(5),
        health_monitor_interval: Duration::from_secs(5),
        health_monitor_timeout: Duration::from_secs(3),
        health_monitor_max_failures: 3,
    };
    let core_node = CoreNode::new(
        messenger,
        Some("custom_name"),
        node_arguments,
        std::env::temp_dir(),
        peppy_dirs,
    );
    assert_eq!(
        core_node.node_config().manifest.name.as_str(),
        "custom_name"
    );
}
