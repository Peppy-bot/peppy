use pmi::{Messenger, MessengerAdapter, MessengerBackend, MockAdapter};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use master_node::{MasterNode, MasterNodeArguments};
use node_stack::NodeStack;
use peppylib::messaging::MessengerHandle;

pub async fn create_mock_messenger() -> Arc<Mutex<Messenger>> {
    let adapter = MockAdapter::default();
    let mut messenger = Messenger::new(MessengerAdapter::Mock(adapter));
    messenger
        .start_session()
        .await
        .expect("failed to start mock session");
    Arc::new(Mutex::new(messenger))
}

#[allow(dead_code)]
pub struct StartedMasterNode {
    pub shared_messenger: Arc<Mutex<Messenger>>,
    pub caller_handle: MessengerHandle,
    pub master_node_name: String,
    pub node_stack: NodeStack,
    pub task: JoinHandle<master_node::Result<()>>,
}

pub async fn start_master_node() -> StartedMasterNode {
    start_master_node_with_timeout(Duration::from_secs(5)).await
}

pub async fn start_master_node_with_timeout(
    node_start_health_timeout: Duration,
) -> StartedMasterNode {
    let shared_messenger = create_mock_messenger().await;
    let caller_handle = MessengerHandle::from_shared(Arc::clone(&shared_messenger));

    let node_arguments = MasterNodeArguments {
        node_start_health_timeout,
    };
    let master_node = MasterNode::new(
        Arc::clone(&shared_messenger),
        Some("test_master_node"),
        node_arguments,
    );
    let master_node_name = master_node.node_name().to_string();
    let node_stack = master_node.node_stack().clone();

    let task = tokio::spawn(async move { master_node.start().await });

    // Allow the MasterNode services to fully establish their listeners
    tokio::time::sleep(Duration::from_millis(50)).await;

    StartedMasterNode {
        shared_messenger,
        caller_handle,
        master_node_name,
        node_stack,
        task,
    }
}
