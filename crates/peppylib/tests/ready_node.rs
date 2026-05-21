mod common;

use common::{
    CALLER_INSTANCE_ID, TEST_CORE_NODE_NAME, TEST_INSTANCE_ID, TEST_NODE_NAME, get_client_server,
    test_node_target,
};
use peppylib::{
    messaging::{MessengerHandle, ServiceMessenger},
    services::ready::listen_for_node_ready,
    types::Payload,
};
use std::sync::Arc;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ready_node() {
    let (client, shared_messenger) = get_client_server().await;

    // Set up the ready service on the server side
    let server_handle = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    let ready_task = listen_for_node_ready(
        &server_handle,
        TEST_CORE_NODE_NAME,
        TEST_INSTANCE_ID,
        test_node_target(TEST_NODE_NAME),
        &[],
    )
    .await
    .expect("failed to start ready service");

    // Allow the service to fully establish its listeners
    tokio::time::sleep(Duration::from_millis(50)).await;

    let request_payload = Payload::from_static(b"ready");

    // The ready service should accept all valid targeting modes:
    // - specific core node + specific instance
    // - specific core node + broadcast instance
    // - broadcast core node + specific instance
    // - full broadcast (core node + instance)
    let to_combinations = [
        (
            Some(client.core_node_name.as_str()),
            Some(client.instance_id.as_str()),
        ),
        (Some(client.core_node_name.as_str()), None),
        (None, Some(client.instance_id.as_str())),
        (None, None),
    ];

    for (target_core_node, target_instance_id) in to_combinations {
        let response = ServiceMessenger::poll(
            &client.caller_handle,
            &client.core_node_name,
            CALLER_INSTANCE_ID,
            test_node_target(TEST_NODE_NAME),
            None,
            peppylib::messaging::NODE_READY_SERVICE,
            target_core_node,
            target_instance_id,
            request_payload.clone(),
            Duration::from_secs(2),
        )
        .await
        .expect("caller should receive response");

        assert_eq!(response.payload(), &request_payload);
        assert_eq!(response.core_node(), client.core_node_name);
        assert_eq!(response.instance_id(), client.instance_id);
    }

    // The ready task should still be running (it handles multiple requests)
    assert!(!ready_task.is_finished());
}
