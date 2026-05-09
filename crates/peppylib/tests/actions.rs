use config::node::QoSProfile;
use peppylib::PeppyError;
use peppylib::messaging::{
    ActionFeedbackPublisher, ActionGoalHandle, ActionMessenger, MessengerHandle,
};
use peppylib::types::Payload;
use pmi::ZenohAdapter;
use std::sync::Arc;
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
        let publisher_tx = std::sync::Arc::new(std::sync::Mutex::new(Some(publisher_tx)));
        action
            .goal_service
            .handle_next_request(move |req_ctx| {
                let resp = goal_resp.clone();
                let factory = factory.clone();
                let publisher_tx = publisher_tx.clone();
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

    let mut goal_handle = ActionMessenger::send_goal(
        &client_handle,
        core_node,
        instance_id,
        node_name,
        action_name,
        Some(core_node),
        Some(instance_id),
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

/// Scaffolding kept alive across a test. Holds both server and client
/// `MessengerHandle`s so their underlying Zenoh sessions don't tear down
/// while the test is still publishing or draining feedback (subscription
/// background task fails the moment the session that produced the
/// subscriber drops). `shutdown_tx` ends the goal-handler task at cleanup.
struct ServerScaffold {
    _server_handle: MessengerHandle,
    _client_handle: MessengerHandle,
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
    _join: tokio::task::JoinHandle<()>,
}

/// Drives the goal request/response handshake and hands the test back the
/// per-goal `ActionFeedbackPublisher` (server side) plus the client's
/// `ActionGoalHandle`. The returned scaffold must outlive the test's
/// publishes.
async fn setup_goal_handshake(
    host: &str,
    port: u16,
    core_node: &str,
    instance_id: &str,
    node_name: &str,
    action_name: &str,
) -> (ActionFeedbackPublisher, ActionGoalHandle, ServerScaffold) {
    let server_handle = MessengerHandle::from_host_port(host, port)
        .await
        .expect("failed to create server handle");
    let client_handle = MessengerHandle::from_host_port(host, port)
        .await
        .expect("failed to create client handle");

    let mut action = ActionMessenger::expose(
        &server_handle,
        core_node,
        instance_id,
        node_name,
        action_name,
    )
    .await
    .expect("expose should succeed");

    tokio::time::sleep(Duration::from_millis(50)).await;

    let (publisher_tx, publisher_rx) = tokio::sync::oneshot::channel::<ActionFeedbackPublisher>();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let factory = action.feedback_publisher_factory.clone();
    let join = tokio::spawn(async move {
        let publisher_tx = Arc::new(std::sync::Mutex::new(Some(publisher_tx)));
        action
            .goal_service
            .handle_next_request(move |req_ctx| {
                let factory = factory.clone();
                let publisher_tx = publisher_tx.clone();
                async move {
                    let wire = req_ctx.message().payload().into_inner();
                    let declared = factory
                        .declare_from_wire(wire)
                        .await
                        .expect("declare from wire");
                    if let Some(tx) = publisher_tx.lock().unwrap().take() {
                        let _ = tx.send(declared.publisher);
                    }
                    Ok(Payload::from_static(b"accepted"))
                }
            })
            .await
            .expect("goal handler should succeed");
        // Hold `action` (and thus the per-goal publisher's session) alive
        // until the test signals completion.
        let _ = shutdown_rx.await;
        drop(action);
    });

    let goal_handle = ActionMessenger::send_goal(
        &client_handle,
        core_node,
        instance_id,
        node_name,
        action_name,
        Some(core_node),
        Some(instance_id),
        Payload::from_static(b"goal data"),
        QoSProfile::Reliable,
        Duration::from_secs(2),
    )
    .await
    .expect("send_goal should succeed");

    let publisher = publisher_rx
        .await
        .expect("server should have captured publisher");

    (
        publisher,
        goal_handle,
        ServerScaffold {
            _server_handle: server_handle,
            _client_handle: client_handle,
            shutdown_tx,
            _join: join,
        },
    )
}

/// Drive `publisher.publish` with a short retry loop until the client
/// confirms delivery via `on_next_feedback`. The per-goal publisher uses
/// `PublisherQoS::Standard` (drop-on-no-match), and matching between the
/// just-declared publisher and the client's pre-existing subscription is
/// eventually-consistent through the Zenoh router. Without this warmup,
/// the first publish can be silently dropped.
async fn warm_up_feedback(
    publisher: &ActionFeedbackPublisher,
    goal_handle: &mut ActionGoalHandle,
    warmup_payload: &Payload,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        publisher
            .publish(warmup_payload.clone())
            .await
            .expect("warmup publish should succeed");
        match tokio::time::timeout(Duration::from_millis(100), goal_handle.on_next_feedback()).await
        {
            Ok(Ok(msg)) => {
                assert_eq!(msg.payload(), warmup_payload);
                return;
            }
            Ok(Err(err)) => panic!("unexpected feedback error during warmup: {err:?}"),
            Err(_elapsed) => {
                if tokio::time::Instant::now() >= deadline {
                    panic!("publisher → subscriber matching never settled within timeout");
                }
            }
        }
    }
}

/// `publish_end()` must surface as `Err(ActionFeedbackChannelClosed)` on the
/// client's drain loop. This is the messaging-layer primitive every codegen
/// relies on; protect it with a direct test.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn action_feedback_publish_end_signals_channel_closed() {
    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (host, port) = (instance.host.clone(), instance.port);

    let warmup_payload = Payload::from_static(b"warmup");
    let feedback_payload = Payload::from_static(b"50% done");
    let (publisher, mut goal_handle, scaffold) = setup_goal_handshake(
        &host,
        port,
        "test_core",
        "test_instance",
        "test_node",
        "test_action",
    )
    .await;

    warm_up_feedback(&publisher, &mut goal_handle, &warmup_payload).await;

    // With matching settled, the regular publish + publish_end pair must
    // arrive in order on the client side.
    publisher
        .publish(feedback_payload.clone())
        .await
        .expect("regular feedback publish should succeed");
    publisher
        .publish_end()
        .await
        .expect("publish_end should succeed");

    let received = tokio::time::timeout(Duration::from_secs(2), goal_handle.on_next_feedback())
        .await
        .expect("regular feedback should arrive within timeout")
        .expect("regular feedback should be Ok");
    assert_eq!(received.payload(), &feedback_payload);

    let closed = tokio::time::timeout(Duration::from_secs(2), goal_handle.on_next_feedback())
        .await
        .expect("close signal should arrive within timeout");
    match closed {
        Err(PeppyError::ActionFeedbackChannelClosed) => {}
        other => panic!("expected ActionFeedbackChannelClosed, got {other:?}"),
    }

    let _ = scaffold.shutdown_tx.send(());
}

/// Same end-of-stream contract as above, but exercises the non-blocking
/// `try_next_feedback` path — it has its own `is_end_sentinel` branch in
/// actions.rs that's easy to miss in a refactor.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn action_feedback_publish_end_signals_channel_closed_via_try_next() {
    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (host, port) = (instance.host.clone(), instance.port);

    let warmup_payload = Payload::from_static(b"warmup");
    let (publisher, mut goal_handle, scaffold) = setup_goal_handshake(
        &host,
        port,
        "test_core",
        "test_instance_try",
        "test_node",
        "test_action_try",
    )
    .await;

    warm_up_feedback(&publisher, &mut goal_handle, &warmup_payload).await;

    publisher
        .publish_end()
        .await
        .expect("publish_end should succeed");

    // Scaffold lives until the loop below confirms the close signal — keep
    // the binding alive past the loop with `let _ = scaffold...`.
    let _scaffold = scaffold;

    // Poll until the sentinel reaches the client. Bound the wait so a
    // regression that drops the sentinel fails the test instead of hanging.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        match goal_handle.try_next_feedback() {
            Err(PeppyError::ActionFeedbackChannelClosed) => break,
            Ok(None) => {
                if tokio::time::Instant::now() >= deadline {
                    panic!("close signal did not arrive via try_next_feedback within timeout");
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok(Some(msg)) => panic!("expected close signal, got message: {msg:?}"),
            Err(other) => panic!("expected ActionFeedbackChannelClosed, got {other:?}"),
        }
    }
}

/// `ActionFeedbackPublisher::publish(empty_payload)` is a runtime error — empty
/// is reserved for the end-of-stream sentinel. This was a `debug_assert!`
/// originally and easy to regress back to one without breaking debug-build
/// tests.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn action_feedback_publish_rejects_empty_payload() {
    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (host, port) = (instance.host.clone(), instance.port);

    let (publisher, _goal_handle, _scaffold) = setup_goal_handshake(
        &host,
        port,
        "test_core",
        "test_instance_empty",
        "test_node",
        "test_action_empty",
    )
    .await;

    match publisher.publish(Payload::new()).await {
        Err(PeppyError::InternalEncodingError { identifier, .. }) => {
            assert_eq!(identifier, "action_feedback_publish");
        }
        Ok(()) => panic!("expected error, got Ok — empty payload must be rejected"),
        Err(other) => panic!("expected InternalEncodingError, got {other:?}"),
    }
}
