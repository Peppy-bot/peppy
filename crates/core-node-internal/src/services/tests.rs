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

#[tokio::test]
async fn core_node_execution_has_no_start_cmd_and_no_container() {
    let messenger = create_mock_messenger().await;
    let peppy_dirs = PeppyDirs::new(std::env::temp_dir());
    let node_arguments = CoreNodeArguments {
        node_startup_timeout: Duration::from_secs(5),
        node_start_health_timeout: Duration::from_secs(5),
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
        execution.start_cmd.is_none(),
        "core node must not have a start_cmd (it runs in-process, not as a spawned process)"
    );
    assert!(
        execution.container.is_none(),
        "core node must not have a container config"
    );
}
