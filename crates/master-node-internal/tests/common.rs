use master_node::MasterNode;
use peppy_core::AppContext;
use peppylib::messaging::MessengerHandle;
use pmi::{Messenger, MessengerAdapter, MessengerBackend, MockAdapter};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

pub struct TestMasterNode {
    pub master_node_name: String,
    pub instance_id: String,
    pub caller_handle: MessengerHandle,
    pub task: JoinHandle<master_node::Result<()>>,
}

impl Drop for TestMasterNode {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub const CALLER_INSTANCE_ID: &str = "caller_instance";

pub async fn setup_test_master_node() -> TestMasterNode {
    let shared_messenger = create_mock_messenger().await;

    let app_ctx = Arc::new(AppContext::default());

    let master_node = MasterNode::new(
        &app_ctx,
        Arc::clone(&shared_messenger),
        Some("test_master_node"),
    );
    let master_node_name = master_node.node_name().to_string();
    let instance_id = master_node.instance_id().to_string();

    let task = tokio::spawn(async move { master_node.start().await });

    // Allow the MasterNode services to fully establish their listeners
    tokio::time::sleep(Duration::from_millis(50)).await;

    let caller_handle = MessengerHandle::from_shared(shared_messenger);

    TestMasterNode {
        master_node_name,
        instance_id,
        caller_handle,
        task,
    }
}

async fn create_mock_messenger() -> Arc<Mutex<Messenger>> {
    let adapter = MockAdapter::default();
    let mut messenger = Messenger::new(MessengerAdapter::Mock(adapter));
    messenger
        .start_session()
        .await
        .expect("failed to start mock session");
    Arc::new(Mutex::new(messenger))
}
