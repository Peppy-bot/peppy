#![allow(dead_code)]

use peppylib::messaging::{MessengerHandle, SenderTarget};
use pmi::{Messenger, MessengerAdapter, MessengerBackend, MockAdapter};
use std::sync::Arc;
use tokio::sync::Mutex;

pub const CALLER_INSTANCE_ID: &str = "caller_instance";

pub const TEST_CORE_NODE_NAME: &str = "test_core_node";
pub const TEST_NODE_NAME: &str = "test_node";
pub const TEST_INSTANCE_ID: &str = "test_instance";
pub const TEST_NODE_TAG: &str = "v1";

/// Builds a node-shaped [`SenderTarget`] with the standard test tag. Panics on
/// invalid names — tests use known-good values only.
pub fn test_node_target(name: &str) -> SenderTarget {
    SenderTarget::node(name, TEST_NODE_TAG).expect("test node target")
}

/// Client for sending requests to a test node.
pub struct CoreNodeClient {
    pub caller_handle: MessengerHandle,
    pub core_node_name: String,
    pub instance_id: String,
}

/// Creates a shared mock messenger and returns a client with a MessengerHandle.
pub async fn get_client_server() -> (CoreNodeClient, Arc<Mutex<Messenger>>) {
    let shared_messenger = create_mock_messenger().await;

    let caller_handle = MessengerHandle::from_shared(Arc::clone(&shared_messenger));

    let client = CoreNodeClient {
        caller_handle,
        core_node_name: TEST_CORE_NODE_NAME.to_string(),
        instance_id: TEST_INSTANCE_ID.to_string(),
    };

    (client, shared_messenger)
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
