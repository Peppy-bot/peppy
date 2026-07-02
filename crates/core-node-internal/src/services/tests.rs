use super::*;
use daemon_config::consts::PeppyDirs;
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

fn test_node_arguments() -> CoreNodeArguments {
    CoreNodeArguments {
        node_startup_timeout: Duration::from_secs(5),
        node_start_health_timeout: Duration::from_secs(5),
        health_monitor_interval: Duration::from_secs(5),
        health_monitor_timeout: Duration::from_secs(3),
        clock_publish_interval: Duration::from_millis(100),
        heartbeat_interval: Duration::from_secs(5),
        daemon_use_sim_time: false,
    }
}

/// Builds a `CoreNodeConfig` for the constructor tests; the cases differ only in
/// the messenger, the explicit name, and the dirs.
fn test_core_node_config(
    messenger: Arc<Mutex<Messenger>>,
    node_name: Option<&str>,
    peppy_dirs: PeppyDirs,
) -> CoreNodeConfig {
    CoreNodeConfig {
        messenger,
        node_name: node_name.map(str::to_string),
        arguments: test_node_arguments(),
        root_dir: std::env::temp_dir(),
        peppy_dirs,
        peppy_config: daemon_config::peppy_config::PeppyConfig::default(),
        organization_namespace: "local".to_string(),
        shutdown_token: tokio_util::sync::CancellationToken::new(),
    }
}

/// Verifies that the core node is configured to run in-process, not as a spawned
/// subprocess or inside a container.
#[tokio::test]
async fn core_node_execution_has_no_run_cmd_and_no_container() {
    let messenger = create_mock_messenger().await;
    let peppy_dirs = PeppyDirs::new(std::env::temp_dir());
    let core_node = CoreNode::new(test_core_node_config(
        messenger,
        Some("test_core_node"),
        peppy_dirs,
    ));

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

    let a = CoreNode::new(test_core_node_config(
        create_mock_messenger().await,
        None,
        peppy_dirs.clone(),
    ));
    let b = CoreNode::new(test_core_node_config(
        create_mock_messenger().await,
        None,
        peppy_dirs,
    ));

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
    let core_node = CoreNode::new(test_core_node_config(
        messenger,
        Some("custom_name"),
        peppy_dirs,
    ));
    assert_eq!(
        core_node.node_config().manifest.name.as_str(),
        "custom_name"
    );
}

/// A second `start_with_ready` on the same instance is rejected rather than
/// re-running the destructive setup and double-registering listeners.
#[tokio::test]
async fn start_with_ready_rejects_a_second_start() {
    let core_node = Arc::new(CoreNode::new(test_core_node_config(
        create_mock_messenger().await,
        Some("dup_start_node"),
        PeppyDirs::new(std::env::temp_dir()),
    )));

    // Drive the first start on a task: it registers listeners then serves until
    // the session closes. The ready signal is a deterministic barrier — once it
    // fires, the `started` flag is set.
    let first = Arc::clone(&core_node);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let first_task = tokio::spawn(async move { first.start_with_ready(Some(ready_tx)).await });
    ready_rx
        .await
        .expect("first start should reach the ready signal");

    let err = core_node
        .start_with_ready(None)
        .await
        .expect_err("a second start must be rejected");
    assert!(matches!(err, crate::Error::AlreadyStarted));

    first_task.abort();
}
