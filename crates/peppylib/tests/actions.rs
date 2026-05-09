use config::node::QoSProfile;
use peppylib::messaging::{ActionMessenger, MessengerHandle};
use peppylib::types::Payload;
use pmi::ZenohAdapter;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn action_messenger_communication() {
    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (host, port) = (instance.host.clone(), instance.port);

    let core_node = "test_core";
    let instance_id = "test_instance";
    let node_name = "test_node";
    let action_name = "test_action";
    let goal_payload = Payload::from_static(b"goal data");
    let goal_response_payload = Payload::from_static(b"goal accepted");
    let feedback_payload = Payload::from_static(b"50% done");
    let result_payload = Payload::from_static(b"action result");

    let server_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("failed to create server handle");
    let client_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("failed to create client handle");

    // Expose the action server
    let mut action = ActionMessenger::expose(
        &server_handle,
        core_node,
        instance_id,
        node_name,
        action_name,
    )
    .await
    .expect("expose should succeed");

    // Allow subscriptions to propagate
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Run the server side in a spawned task
    let goal_resp = goal_response_payload.clone();
    let fb = feedback_payload.clone();
    let res = result_payload.clone();
    // Server uses declare_from_wire to unwrap the envelope + declare the
    // per-goal feedback publisher in one call, matching the goal_id the
    // client emits below.
    let (publisher_tx, publisher_rx) =
        tokio::sync::oneshot::channel::<peppylib::messaging::ActionFeedbackPublisher>();
    let factory = action.feedback_publisher_factory.clone();
    let server = tokio::spawn(async move {
        let publisher_tx = std::sync::Mutex::new(Some(publisher_tx));
        action
            .goal_service
            .handle_next_request(move |req_ctx| {
                let resp = goal_resp.clone();
                let factory = factory.clone();
                let publisher_tx = std::sync::Mutex::new(publisher_tx.lock().unwrap().take());
                async move {
                    let wire = req_ctx.message().payload().into_inner();
                    let declared = factory
                        .declare_from_wire(wire)
                        .await
                        .expect("declare from wire");
                    if let Some(tx) = publisher_tx.lock().unwrap().take() {
                        let _ = tx.send(declared.publisher);
                    }
                    Ok(resp)
                }
            })
            .await
            .expect("goal handler should succeed");

        let feedback_publisher = publisher_rx
            .await
            .expect("server should have captured publisher");
        feedback_publisher
            .publish(fb)
            .await
            .expect("feedback publish should succeed");

        // Handle the result request
        action
            .result_service
            .handle_next_request(|_req| {
                let r = res;
                async move { Ok(r) }
            })
            .await
            .expect("result handler should succeed");
    });

    // Client: wrap the user payload with a fresh goal_id and send.
    let goal_id = peppylib::messaging::generate_goal_id();
    let goal_payload =
        peppylib::messaging::wrap_goal_payload(&goal_id, goal_payload.as_ref()).expect("wrap goal");
    let mut goal_handle = ActionMessenger::send_goal(
        &client_handle,
        core_node,
        instance_id,
        node_name,
        action_name,
        Some(core_node),
        Some(instance_id),
        &goal_id,
        goal_payload,
        QoSProfile::Reliable,
        Duration::from_secs(2),
    )
    .await
    .expect("send_goal should succeed");

    assert_eq!(
        goal_handle.goal_response().payload(),
        &goal_response_payload
    );

    // Client: receive feedback
    let feedback = tokio::time::timeout(Duration::from_secs(2), goal_handle.on_next_feedback())
        .await
        .expect("should receive feedback within timeout")
        .expect("feedback should not be an error");

    assert_eq!(feedback.payload(), &feedback_payload);

    // Client: request result
    let result =
        ActionMessenger::request_result(&client_handle, &goal_handle, Duration::from_secs(2))
            .await
            .expect("request_result should succeed");

    assert_eq!(result.payload(), &result_payload);

    server.await.expect("server task should not panic");
}
