mod common;

use common::{
    TEST_CORE_NODE_NAME, TEST_INSTANCE_ID, TEST_NODE_NAME, get_client_server, test_node_target,
};
use peppylib::{
    messaging::{MessengerHandle, NODE_READY_SERVICE, ServiceTarget},
    services::ready::listen_for_node_ready,
    types::Payload,
};
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ready_node() {
    let (client, shared_messenger) = get_client_server().await;

    let server_handle = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    let ready_task = listen_for_node_ready(
        &server_handle,
        TEST_CORE_NODE_NAME,
        TEST_INSTANCE_ID,
        test_node_target(TEST_NODE_NAME),
    )
    .await
    .expect("failed to start ready service");

    let request_payload = Payload::from_static(b"ready");

    // The ready service should accept both valid targeting modes:
    // - fully pinned producer (core_node + instance_id)
    // - full broadcast (no target producer)
    let pinned = client.producer();
    let targets = [ServiceTarget::Producer(&pinned), ServiceTarget::Any];

    for target in targets {
        let response = client
            .poll_test_node(NODE_READY_SERVICE, target, request_payload.clone())
            .await
            .expect("caller should receive response");

        assert_eq!(response.payload(), &request_payload);
        assert_eq!(response.core_node(), client.core_node_name);
        assert_eq!(response.instance_id(), client.instance_id);
    }

    // The ready task should still be running (it handles multiple requests)
    assert!(!ready_task.is_finished());
}
