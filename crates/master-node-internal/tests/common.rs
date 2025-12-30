use master_node::{MasterNode, MasterNodeArguments};
use node_stack::NodeStack;
use peppylib::messaging::MessengerHandle;
use pmi::{Messenger, MessengerAdapter, MessengerBackend, MockAdapter};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

const DEFAULT_HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(15);

/// Client for sending requests to a MasterNode.
pub struct MasterNodeClient {
    pub caller_handle: MessengerHandle,
    pub master_node_name: String,
    #[allow(dead_code)]
    pub instance_id: String,
}

/// Server-side handle to a running MasterNode, providing access to its state.
pub struct MasterNodeServer {
    #[allow(dead_code)]
    pub node_stack: NodeStack,
    #[allow(dead_code)]
    pub shared_messenger: Arc<Mutex<Messenger>>,
    task: JoinHandle<master_node::Result<()>>,
}

impl Drop for MasterNodeServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub const CALLER_INSTANCE_ID: &str = "caller_instance";

pub async fn setup_test_master_node() -> (MasterNodeClient, MasterNodeServer) {
    setup_test_master_node_with_timeout(DEFAULT_HEALTH_CHECK_TIMEOUT).await
}

pub async fn setup_test_master_node_with_timeout(
    node_start_health_timeout: Duration,
) -> (MasterNodeClient, MasterNodeServer) {
    let shared_messenger = create_mock_messenger().await;

    let node_arguments = MasterNodeArguments {
        node_start_health_timeout,
    };
    let master_node = MasterNode::new(
        Arc::clone(&shared_messenger),
        Some("test_master_node"),
        node_arguments,
    );
    let master_node_name = master_node.node_name().to_string();
    let instance_id = master_node.instance_id().to_string();
    let node_stack = master_node.node_stack().clone();

    let task = tokio::spawn(async move { master_node.start().await });

    // Allow the MasterNode services to fully establish their listeners
    tokio::time::sleep(Duration::from_millis(50)).await;

    let caller_handle = MessengerHandle::from_shared(Arc::clone(&shared_messenger));

    let client = MasterNodeClient {
        caller_handle,
        master_node_name,
        instance_id,
    };

    let server = MasterNodeServer {
        node_stack,
        shared_messenger,
        task,
    };

    (client, server)
}

pub async fn create_mock_messenger() -> Arc<Mutex<Messenger>> {
    let adapter = MockAdapter::default();
    let mut messenger = Messenger::new(MessengerAdapter::Mock(adapter));
    messenger
        .start_session()
        .await
        .expect("failed to start mock session");
    Arc::new(Mutex::new(messenger))
}
