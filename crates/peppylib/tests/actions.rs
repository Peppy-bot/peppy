mod common;

use common::test_node_target;
use config::node::QoSProfile;
use peppylib::PeppyError;
use peppylib::messaging::{
    ActionFeedbackPublisher, ActionGoalHandle, ActionMessenger, CancelState, ConcurrentAction,
    EmptyPayloadError, MessengerHandle, NonEmptyPayload, ProducerRef, ResultStatus, SenderTarget,
    decode_cancel_ack, encode_cancel_ack, wrap_result_outcome,
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
        test_node_target(node_name),
        action_name,
    )
    .await
    .expect("expose should succeed");

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
                        .declare_from_wire("_", wire)
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
            .publish(NonEmptyPayload::try_new(fb).expect("test feedback payload is non-empty"))
            .await
            .expect("feedback publish should succeed");

        // Handle the result request. This test drives the result service
        // directly (not through `ConcurrentAction`), so it must frame the reply
        // with the engine's result-outcome envelope itself.
        action
            .result_service
            .handle_next_request(|_req| {
                let r = res;
                async move { Ok(wrap_result_outcome(ResultStatus::Completed, r.as_ref())) }
            })
            .await
            .expect("result handler should succeed");
    });

    let mut goal_handle = ActionMessenger::send_goal(
        &client_handle,
        core_node,
        instance_id,
        test_node_target(node_name),
        action_name,
        Some(&ProducerRef::new(core_node, instance_id)),
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

    assert_eq!(result.status, ResultStatus::Completed);
    assert_eq!(result.body, result_payload);

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
        test_node_target(node_name),
        action_name,
    )
    .await
    .expect("expose should succeed");

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
                        .declare_from_wire("_", wire)
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
        test_node_target(node_name),
        action_name,
        Some(&ProducerRef::new(core_node, instance_id)),
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

/// `publish_end()` must surface as `Err(ActionFeedbackChannelClosed)` on the
/// client's drain loop. This is the messaging-layer primitive every codegen
/// relies on; protect it with a direct test.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn action_feedback_publish_end_signals_channel_closed() {
    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (host, port) = (instance.host.clone(), instance.port);

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

    publisher
        .publish(
            NonEmptyPayload::try_new(feedback_payload.clone())
                .expect("test feedback payload is non-empty"),
        )
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

    let (publisher, mut goal_handle, scaffold) = setup_goal_handshake(
        &host,
        port,
        "test_core",
        "test_instance_try",
        "test_node",
        "test_action_try",
    )
    .await;

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

/// Empty feedback payloads are forbidden at the type layer:
/// `ActionFeedbackPublisher::publish` takes [`NonEmptyPayload`], so the
/// only way to construct one is through [`NonEmptyPayload::try_new`],
/// which rejects empty payloads with [`EmptyPayloadError`]. This test pins
/// that constructor contract so a refactor that loosens the check at the
/// type boundary fails immediately, without needing a Zenoh router to
/// reach `publish()`.
#[test]
fn non_empty_payload_rejects_empty_payload() {
    let result = NonEmptyPayload::try_new(Payload::new());
    assert!(
        matches!(result, Err(EmptyPayloadError)),
        "empty payload must be rejected by NonEmptyPayload::try_new",
    );
}

/// The fixed cancel-ack encoder/decoder must round-trip every [`CancelState`].
/// This guards the encoder peppylib's concurrent-action engine sends in reply to
/// every cancel; both Rust and Python generated clients decode it via this same
/// `decode_cancel_ack`, so there is no separate per-action wire to keep in sync.
#[test]
fn cancel_ack_encode_decode_roundtrip() {
    for state in [
        CancelState::Signalled,
        CancelState::AlreadyTerminal,
        CancelState::Unknown,
    ] {
        let encoded = encode_cancel_ack(state).expect("encode cancel state");
        let decoded = decode_cancel_ack(encoded.as_ref()).expect("decode cancel state");
        assert_eq!(decoded, state);
    }
}

/// Two goals fired at one [`ConcurrentAction`] server must run concurrently
/// with fully independent feedback streams and results: goal A's feedback never
/// lands on goal B's stream, and each `get_result` is routed back to its own
/// goal by `goal_id`. Each goal echoes its request payload into its feedback
/// and result so a cross-stream leak would fail the assertions.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_action_two_goals_independent() {
    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (host, port) = (instance.host.clone(), instance.port);

    let core_node = "test_core";
    let instance_id = "exposer";
    let node_name = "brain";
    let action_name = "move_arm";

    let server_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("server handle");
    let client_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("client handle");

    let mut action = ConcurrentAction::expose(
        &server_handle,
        core_node,
        instance_id,
        test_node_target(node_name),
        action_name,
        true,
    )
    .await
    .expect("expose should succeed");

    let server = tokio::spawn(async move {
        while let Ok(Some(pending)) = action.recv_next_goal().await {
            let request = pending.request_bytes().to_vec();
            let Ok(ctx) = pending.accept(Payload::from_static(b"accepted")).await else {
                continue;
            };
            tokio::spawn(async move {
                let mut feedback = b"fb:".to_vec();
                feedback.extend_from_slice(&request);
                let _ = ctx
                    .publish_feedback(
                        NonEmptyPayload::try_new(Payload::from(feedback))
                            .expect("feedback is non-empty"),
                    )
                    .await;
                let mut result = b"result:".to_vec();
                result.extend_from_slice(&request);
                let _ = ctx.complete(Payload::from(result)).await;
            });
        }
    });

    let target = ProducerRef::new(core_node, instance_id);
    let send = |payload: &'static [u8]| {
        ActionMessenger::send_goal(
            &client_handle,
            core_node,
            instance_id,
            test_node_target(node_name),
            action_name,
            Some(&target),
            Payload::from_static(payload),
            QoSProfile::Reliable,
            Duration::from_secs(2),
        )
    };
    let mut goal_a = send(b"A").await.expect("send goal A");
    let mut goal_b = send(b"B").await.expect("send goal B");

    assert_ne!(
        goal_a.goal_id(),
        goal_b.goal_id(),
        "each goal must get a distinct goal_id",
    );

    // Feedback isolation: each handle receives only its own goal's feedback.
    let fb_a = tokio::time::timeout(Duration::from_secs(2), goal_a.on_next_feedback())
        .await
        .expect("A feedback within timeout")
        .expect("A feedback ok");
    assert_eq!(fb_a.payload().as_ref(), b"fb:A");
    let fb_b = tokio::time::timeout(Duration::from_secs(2), goal_b.on_next_feedback())
        .await
        .expect("B feedback within timeout")
        .expect("B feedback ok");
    assert_eq!(fb_b.payload().as_ref(), b"fb:B");

    // Result routing by goal_id: each handle gets its own goal's result.
    let res_a = ActionMessenger::request_result(&client_handle, &goal_a, Duration::from_secs(2))
        .await
        .expect("result A");
    assert_eq!(res_a.status, ResultStatus::Completed);
    assert_eq!(res_a.body.as_ref(), b"result:A");
    let res_b = ActionMessenger::request_result(&client_handle, &goal_b, Duration::from_secs(2))
        .await
        .expect("result B");
    assert_eq!(res_b.status, ResultStatus::Completed);
    assert_eq!(res_b.body.as_ref(), b"result:B");

    server.abort();
    drop(server_handle);
}

/// A cancel must fire only the targeted goal's signal. Goal A is cancelled
/// immediately after `fire_goal` returns — exercising the register-before-respond
/// ordering, since the slot must already exist for the cancel to match — and
/// reports its cancelled result, while goal B finishes normally, untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_action_cancel_targets_one_goal() {
    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (host, port) = (instance.host.clone(), instance.port);

    let core_node = "test_core";
    let instance_id = "exposer";
    let node_name = "brain";
    let action_name = "move_arm";

    let server_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("server handle");
    let client_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("client handle");

    let mut action = ConcurrentAction::expose(
        &server_handle,
        core_node,
        instance_id,
        test_node_target(node_name),
        action_name,
        true,
    )
    .await
    .expect("expose should succeed");

    let server = tokio::spawn(async move {
        while let Ok(Some(pending)) = action.recv_next_goal().await {
            let request = pending.request_bytes().to_vec();
            let Ok(ctx) = pending.accept(Payload::from_static(b"accepted")).await else {
                continue;
            };
            tokio::spawn(async move {
                tokio::select! {
                    _ = ctx.cancel_signal() => {
                        let mut result = b"cancelled:".to_vec();
                        result.extend_from_slice(&request);
                        let _ = ctx.complete_cancelled(Payload::from(result)).await;
                    }
                    _ = tokio::time::sleep(Duration::from_millis(400)) => {
                        let mut result = b"done:".to_vec();
                        result.extend_from_slice(&request);
                        let _ = ctx.complete(Payload::from(result)).await;
                    }
                }
            });
        }
    });

    let target = ProducerRef::new(core_node, instance_id);
    let send = |payload: &'static [u8]| {
        ActionMessenger::send_goal(
            &client_handle,
            core_node,
            instance_id,
            test_node_target(node_name),
            action_name,
            Some(&target),
            Payload::from_static(payload),
            QoSProfile::Reliable,
            Duration::from_secs(2),
        )
    };
    let goal_a = send(b"A").await.expect("send goal A");
    let goal_b = send(b"B").await.expect("send goal B");

    // Cancel A immediately; the slot must already be registered.
    let cancel_ack = ActionMessenger::cancel_goal(&client_handle, &goal_a, Duration::from_secs(2))
        .await
        .expect("cancel A");
    let state = decode_cancel_ack(cancel_ack.payload().as_ref()).expect("decode cancel ack");
    assert_eq!(
        state,
        CancelState::Signalled,
        "cancelling a live goal must signal it"
    );

    // A reports its cancelled result; B is unaffected and finishes normally.
    let res_a = ActionMessenger::request_result(&client_handle, &goal_a, Duration::from_secs(2))
        .await
        .expect("result A");
    assert_eq!(res_a.status, ResultStatus::Cancelled);
    assert_eq!(res_a.body.as_ref(), b"cancelled:A");
    let res_b = ActionMessenger::request_result(&client_handle, &goal_b, Duration::from_secs(3))
        .await
        .expect("result B");
    assert_eq!(res_b.status, ResultStatus::Completed);
    assert_eq!(res_b.body.as_ref(), b"done:B");

    server.abort();
    drop(server_handle);
}

/// A rejected goal is answered (the client still gets its goal response) without
/// creating a `GoalContext`, and the server goes on to accept a later goal. This
/// is the engine primitive the generated `handle_goal_next_request` relies on to
/// skip rejections internally.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_action_reject_then_accept() {
    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (host, port) = (instance.host.clone(), instance.port);

    let core_node = "test_core";
    let instance_id = "exposer";
    let node_name = "brain";
    let action_name = "move_arm";

    let server_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("server handle");
    let client_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("client handle");

    let mut action = ConcurrentAction::expose(
        &server_handle,
        core_node,
        instance_id,
        test_node_target(node_name),
        action_name,
        true,
    )
    .await
    .expect("expose should succeed");

    // Reject goals whose payload is "reject"; accept the rest and echo the
    // request into the result. The loop keeps serving after a rejection.
    let server = tokio::spawn(async move {
        while let Ok(Some(pending)) = action.recv_next_goal().await {
            let request = pending.request_bytes().to_vec();
            if request == b"reject" {
                let _ = pending.reject(Payload::from_static(b"rejected")).await;
                continue;
            }
            let Ok(ctx) = pending.accept(Payload::from_static(b"accepted")).await else {
                continue;
            };
            let mut result = b"result:".to_vec();
            result.extend_from_slice(&request);
            let _ = ctx.complete(Payload::from(result)).await;
        }
    });

    let target = ProducerRef::new(core_node, instance_id);
    let send = |payload: &'static [u8]| {
        ActionMessenger::send_goal(
            &client_handle,
            core_node,
            instance_id,
            test_node_target(node_name),
            action_name,
            Some(&target),
            Payload::from_static(payload),
            QoSProfile::Reliable,
            Duration::from_secs(2),
        )
    };

    // First goal is rejected: the client still gets the goal response.
    let goal_a = send(b"reject").await.expect("send rejected goal");
    assert_eq!(goal_a.goal_response().payload().as_ref(), b"rejected");

    // After the rejection the server keeps serving, so the next goal is
    // accepted and its result is routed back by goal_id.
    let goal_b = send(b"B").await.expect("send accepted goal");
    assert_eq!(goal_b.goal_response().payload().as_ref(), b"accepted");
    let res_b = ActionMessenger::request_result(&client_handle, &goal_b, Duration::from_secs(2))
        .await
        .expect("result B");
    assert_eq!(res_b.status, ResultStatus::Completed);
    assert_eq!(res_b.body.as_ref(), b"result:B");

    server.abort();
    drop(server_handle);
}

/// Dropping a `GoalContext` without completing the goal (the worker returned
/// early, panicked, or otherwise abandoned it) must close the goal's feedback
/// stream and transition the goal to a typed `Abandoned` terminal state. A
/// client draining feedback breaks out with `ActionFeedbackChannelClosed`
/// instead of hanging, and a prompt `get_result` resolves to
/// `ResultStatus::Abandoned` (parking first if it raced the transition) — never
/// a bare "no active goal" error, never a hang.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_action_abandoned_goal_yields_typed_abandoned() {
    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (host, port) = (instance.host.clone(), instance.port);

    let core_node = "test_core";
    let instance_id = "exposer";
    let node_name = "brain";
    let action_name = "move_arm";

    let server_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("server handle");
    let client_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("client handle");

    let mut action = ConcurrentAction::expose(
        &server_handle,
        core_node,
        instance_id,
        test_node_target(node_name),
        action_name,
        true,
    )
    .await
    .expect("expose should succeed");

    // The server accepts the goal and then abandons it: the context is dropped
    // without ever calling `complete`.
    let server = tokio::spawn(async move {
        while let Ok(Some(pending)) = action.recv_next_goal().await {
            let Ok(ctx) = pending.accept(Payload::from_static(b"accepted")).await else {
                continue;
            };
            drop(ctx);
        }
    });

    let mut goal = ActionMessenger::send_goal(
        &client_handle,
        core_node,
        instance_id,
        test_node_target(node_name),
        action_name,
        Some(&ProducerRef::new(core_node, instance_id)),
        Payload::from_static(b"X"),
        QoSProfile::Reliable,
        Duration::from_secs(2),
    )
    .await
    .expect("send goal");

    // Feedback must resolve to a clean close, not hang.
    let closed = tokio::time::timeout(Duration::from_secs(2), goal.on_next_feedback())
        .await
        .expect("abandoned goal must close feedback, not hang");
    match closed {
        Err(PeppyError::ActionFeedbackChannelClosed) => {}
        other => panic!("expected ActionFeedbackChannelClosed, got {other:?}"),
    }

    // The goal was abandoned, so the result resolves to a typed `Abandoned`
    // outcome (empty body) instead of erroring or hanging.
    let result = ActionMessenger::request_result(&client_handle, &goal, Duration::from_secs(2))
        .await
        .expect("abandoned goal must resolve to a typed outcome, not error");
    assert_eq!(result.status, ResultStatus::Abandoned);
    assert!(
        result.body.as_ref().is_empty(),
        "abandoned outcome carries no result body"
    );

    server.abort();
    drop(server_handle);
}

/// Hard producer death mid-goal: the producer's session is torn down while a
/// goal is in flight, with its `GoalContext` still alive — so the
/// end-of-stream sentinel is never published (the exact race a SIGKILL /
/// OOM / runtime teardown loses). The consumer's feedback drain must fail
/// over to the producer's liveliness token disappearing:
/// `on_next_feedback` (and `try_next_feedback`) resolve to a typed
/// `ActionFeedbackProducerGone` instead of blocking forever, and
/// `get_result` resolves to `ResultStatus::Abandoned` — both via the goal
/// handle's confirmed-gone fast path and via the sender-only probe path the
/// Python binding uses.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_action_producer_death_unblocks_feedback_and_yields_abandoned() {
    use pmi::{Messenger, MessengerAdapter, MessengerBackend, ZenohNetProtocol};

    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (host, port) = (instance.host.clone(), instance.port);

    let core_node = "test_core";
    let instance_id = "exposer";
    let node_name = "brain";
    let action_name = "move_arm";

    // The producer's messenger is built by hand (instead of
    // `from_host_port`) so the test retains the `Arc` and can close the
    // session deterministically mid-goal — `stop_session` is the in-process
    // stand-in for hard process death: liveliness tokens are removed
    // identically on session close and on transport loss.
    let producer_adapter =
        ZenohAdapter::connect_to(ZenohNetProtocol::Tcp, &host, port).expect("producer adapter");
    let mut producer_messenger = Messenger::new(MessengerAdapter::Zenoh(producer_adapter));
    producer_messenger
        .start_session()
        .await
        .expect("producer start_session");
    let producer_messenger = Arc::new(tokio::sync::Mutex::new(producer_messenger));
    let server_handle = MessengerHandle::from_shared(Arc::clone(&producer_messenger));

    let client_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("client handle");

    let mut action = ConcurrentAction::expose(
        &server_handle,
        core_node,
        instance_id,
        test_node_target(node_name),
        action_name,
        true,
    )
    .await
    .expect("expose should succeed");

    // The server accepts the goal, emits one feedback, then hands the live
    // `GoalContext` out to the test body — it is deliberately NOT dropped
    // before the session dies, so the graceful abandon-on-drop path can
    // never publish the sentinel.
    let (ctx_tx, ctx_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        if let Ok(Some(pending)) = action.recv_next_goal().await
            && let Ok(ctx) = pending.accept(Payload::from_static(b"accepted")).await
        {
            ctx.publish_feedback(
                NonEmptyPayload::try_new(Payload::from_static(b"working"))
                    .expect("test feedback payload is non-empty"),
            )
            .await
            .expect("feedback publish should succeed");
            let _ = ctx_tx.send(ctx);
        }
        // Keep `action` — and with it the liveliness token — alive until the
        // session teardown below removes the token out from under it.
        std::future::pending::<()>().await;
    });

    let mut goal = ActionMessenger::send_goal(
        &client_handle,
        core_node,
        instance_id,
        test_node_target(node_name),
        action_name,
        Some(&ProducerRef::new(core_node, instance_id)),
        Payload::from_static(b"X"),
        QoSProfile::Reliable,
        Duration::from_secs(2),
    )
    .await
    .expect("send goal");

    // The goal is live: first feedback arrives normally.
    let first = tokio::time::timeout(Duration::from_secs(2), goal.on_next_feedback())
        .await
        .expect("live goal must deliver feedback")
        .expect("feedback should arrive before the producer dies");
    assert_eq!(first.payload().as_ref(), b"working");

    let ctx = ctx_rx.await.expect("server should hand out the context");

    // Kill the producer. The context is still alive, so no sentinel is ever
    // published — only the liveliness token disappearing tells the consumer.
    producer_messenger
        .lock()
        .await
        .stop_session()
        .await
        .expect("producer stop_session");

    // The drain must fail over to the typed producer-gone error (Gone event
    // → confirmation probes), never hang and never report a clean close.
    let gone = tokio::time::timeout(Duration::from_secs(10), goal.on_next_feedback())
        .await
        .expect("producer death must unblock the feedback drain, not hang");
    match gone {
        Err(PeppyError::ActionFeedbackProducerGone {
            instance_id: gone_instance,
            action_name: gone_action,
        }) => {
            assert_eq!(gone_instance.as_deref(), Some(instance_id));
            assert_eq!(gone_action, action_name);
        }
        other => panic!("expected ActionFeedbackProducerGone, got {other:?}"),
    }

    // The non-blocking variant observes the same latched state.
    match goal.try_next_feedback() {
        Err(PeppyError::ActionFeedbackProducerGone { .. }) => {}
        other => {
            panic!("expected ActionFeedbackProducerGone from try_next_feedback, got {other:?}")
        }
    }

    // get_result resolves to a typed Abandoned outcome via the goal handle's
    // confirmed-gone fast path (no poll against the dead queryable).
    let result = ActionMessenger::request_result(&client_handle, &goal, Duration::from_secs(2))
        .await
        .expect("dead producer must resolve to a typed outcome, not error");
    assert_eq!(result.status, ResultStatus::Abandoned);
    assert!(
        result.body.as_ref().is_empty(),
        "abandoned outcome carries no result body"
    );
    assert_eq!(result.instance_id, instance_id);

    // The sender-only path (what the Python binding drives) has no goal
    // handle to consult: the result poll fails against the dead producer and
    // the follow-up liveliness probe converts it to the same Abandoned reply.
    let result = ActionMessenger::request_result_with_sender(
        &client_handle,
        goal.sender(),
        goal.goal_id(),
        Duration::from_secs(2),
    )
    .await
    .expect("sender-only path must also resolve to a typed outcome");
    assert_eq!(result.status, ResultStatus::Abandoned);

    drop(ctx);
    server.abort();
    drop(server_handle);
}

/// A `get_result` issued before the worker completes must PARK and then resolve
/// to the typed outcome — never error with "no active goal". This is the core
/// of the fix: a prompt poll on a live goal waits for a definitive answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_action_result_parks_until_complete() {
    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (host, port) = (instance.host.clone(), instance.port);

    let core_node = "test_core";
    let instance_id = "exposer";
    let node_name = "brain";
    let action_name = "move_arm";

    let server_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("server handle");
    let client_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("client handle");

    let mut action = ConcurrentAction::expose(
        &server_handle,
        core_node,
        instance_id,
        test_node_target(node_name),
        action_name,
        false,
    )
    .await
    .expect("expose should succeed");

    // Accept, wait, then complete — so the client's prompt poll parks first.
    let server = tokio::spawn(async move {
        while let Ok(Some(pending)) = action.recv_next_goal().await {
            let request = pending.request_bytes().to_vec();
            let Ok(ctx) = pending.accept(Payload::from_static(b"accepted")).await else {
                continue;
            };
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                let mut result = b"result:".to_vec();
                result.extend_from_slice(&request);
                let _ = ctx.complete(Payload::from(result)).await;
            });
        }
    });

    let goal = ActionMessenger::send_goal(
        &client_handle,
        core_node,
        instance_id,
        test_node_target(node_name),
        action_name,
        Some(&ProducerRef::new(core_node, instance_id)),
        Payload::from_static(b"A"),
        QoSProfile::Reliable,
        Duration::from_secs(2),
    )
    .await
    .expect("send goal");

    // Polled immediately, well before the 300ms completion: must park, then
    // resolve to the completed result.
    let res = ActionMessenger::request_result(&client_handle, &goal, Duration::from_secs(3))
        .await
        .expect("a prompt poll on a live goal must resolve, not error");
    assert_eq!(res.status, ResultStatus::Completed);
    assert_eq!(res.body.as_ref(), b"result:A");

    server.abort();
    drop(server_handle);
}

/// Several concurrent `get_result` polls for the same goal must all resolve once
/// the goal completes — the slot parks a `Vec` of responders, not just one, so a
/// relay that retries its poll never loses an earlier waiter.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_action_multiple_polls_one_goal_all_resolve() {
    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (host, port) = (instance.host.clone(), instance.port);

    let core_node = "test_core";
    let instance_id = "exposer";
    let node_name = "brain";
    let action_name = "move_arm";

    let server_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("server handle");
    let client_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("client handle");

    let mut action = ConcurrentAction::expose(
        &server_handle,
        core_node,
        instance_id,
        test_node_target(node_name),
        action_name,
        false,
    )
    .await
    .expect("expose should succeed");

    let server = tokio::spawn(async move {
        while let Ok(Some(pending)) = action.recv_next_goal().await {
            let Ok(ctx) = pending.accept(Payload::from_static(b"accepted")).await else {
                continue;
            };
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                let _ = ctx.complete(Payload::from_static(b"result:A")).await;
            });
        }
    });

    let goal = ActionMessenger::send_goal(
        &client_handle,
        core_node,
        instance_id,
        test_node_target(node_name),
        action_name,
        Some(&ProducerRef::new(core_node, instance_id)),
        Payload::from_static(b"A"),
        QoSProfile::Reliable,
        Duration::from_secs(2),
    )
    .await
    .expect("send goal");

    // Three polls issued concurrently, all before completion: each parks in the
    // slot's waiter Vec and all must resolve to the same completed result.
    let timeout = Duration::from_secs(3);
    let (r1, r2, r3) = tokio::join!(
        ActionMessenger::request_result(&client_handle, &goal, timeout),
        ActionMessenger::request_result(&client_handle, &goal, timeout),
        ActionMessenger::request_result(&client_handle, &goal, timeout),
    );
    for res in [
        r1.expect("poll 1 resolves"),
        r2.expect("poll 2 resolves"),
        r3.expect("poll 3 resolves"),
    ] {
        assert_eq!(res.status, ResultStatus::Completed);
        assert_eq!(res.body.as_ref(), b"result:A");
    }

    server.abort();
    drop(server_handle);
}

/// A completed goal's result survives the worker dropping its context (a
/// slightly late `get_result` still resolves to the typed result) and can be
/// fetched more than once within the retention window (relay-retry safe — there
/// is no deliver-once). A result never fetched within the window is evicted and
/// a later poll resolves to a typed `Expired` outcome — never a bare error,
/// never a leaked slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_action_completed_result_expires_after_grace() {
    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (host, port) = (instance.host.clone(), instance.port);

    let core_node = "test_core";
    let instance_id = "exposer";
    let node_name = "brain";
    let action_name = "move_arm";
    let grace = Duration::from_millis(500);

    let server_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("server handle");
    let client_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("client handle");

    let mut action = ConcurrentAction::expose(
        &server_handle,
        core_node,
        instance_id,
        test_node_target(node_name),
        action_name,
        false,
    )
    .await
    .expect("expose should succeed")
    .with_result_retention_grace(grace);

    // The server accepts each goal, completes it immediately, and drops the
    // context (end of the loop body) without waiting for the client to fetch.
    let server = tokio::spawn(async move {
        while let Ok(Some(pending)) = action.recv_next_goal().await {
            let request = pending.request_bytes().to_vec();
            let Ok(ctx) = pending.accept(Payload::from_static(b"accepted")).await else {
                continue;
            };
            let mut result = b"result:".to_vec();
            result.extend_from_slice(&request);
            let _ = ctx.complete(Payload::from(result)).await;
        }
    });

    let target = ProducerRef::new(core_node, instance_id);
    let send = |payload: &'static [u8]| {
        ActionMessenger::send_goal(
            &client_handle,
            core_node,
            instance_id,
            test_node_target(node_name),
            action_name,
            Some(&target),
            Payload::from_static(payload),
            QoSProfile::Reliable,
            Duration::from_secs(2),
        )
    };

    // Fetched within the grace: the result survives the context drop, and a
    // second fetch within the window returns the same result (no deliver-once).
    let goal_fast = send(b"A").await.expect("send goal A");
    let res = ActionMessenger::request_result(&client_handle, &goal_fast, Duration::from_secs(2))
        .await
        .expect("result A within grace");
    assert_eq!(res.status, ResultStatus::Completed);
    assert_eq!(res.body.as_ref(), b"result:A");
    let res_again =
        ActionMessenger::request_result(&client_handle, &goal_fast, Duration::from_secs(2))
            .await
            .expect("second result A within grace");
    assert_eq!(res_again.status, ResultStatus::Completed);
    assert_eq!(res_again.body.as_ref(), b"result:A");

    // Never fetched in time: once the grace window elapses the sweeper evicts
    // the slot and a late poll resolves to a typed `Expired` outcome. Poll until
    // eviction (bounded by 10x the grace) rather than sleeping a fixed multiple
    // of it: the test resolves as soon as the slot is gone, with no fixed dead
    // wait and no single-shot race against the 250ms sweeper interval.
    let goal_slow = send(b"B").await.expect("send goal B");
    let mut expired = None;
    for _ in 0..100 {
        let reply =
            ActionMessenger::request_result(&client_handle, &goal_slow, Duration::from_secs(2))
                .await
                .expect("a late poll must resolve to a typed outcome, not error");
        if reply.status == ResultStatus::Expired {
            expired = Some(reply);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let expired = expired.expect("slot should be evicted to Expired within 10x the grace window");
    assert_eq!(expired.status, ResultStatus::Expired);

    server.abort();
    drop(server_handle);
}

/// Cancelling a goal that has already reached a terminal state returns the typed
/// `AlreadyTerminal` (not a bare "no active goal") while its result is retained.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_action_cancel_after_terminal_is_already_terminal() {
    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (host, port) = (instance.host.clone(), instance.port);

    let core_node = "test_core";
    let instance_id = "exposer";
    let node_name = "brain";
    let action_name = "move_arm";

    let server_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("server handle");
    let client_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("client handle");

    // Default retention grace (long) so the terminal slot is still present when
    // the cancel arrives.
    let mut action = ConcurrentAction::expose(
        &server_handle,
        core_node,
        instance_id,
        test_node_target(node_name),
        action_name,
        false,
    )
    .await
    .expect("expose should succeed");

    let server = tokio::spawn(async move {
        while let Ok(Some(pending)) = action.recv_next_goal().await {
            let Ok(ctx) = pending.accept(Payload::from_static(b"accepted")).await else {
                continue;
            };
            let _ = ctx.complete(Payload::from_static(b"done")).await;
        }
    });

    let goal = ActionMessenger::send_goal(
        &client_handle,
        core_node,
        instance_id,
        test_node_target(node_name),
        action_name,
        Some(&ProducerRef::new(core_node, instance_id)),
        Payload::from_static(b"A"),
        QoSProfile::Reliable,
        Duration::from_secs(2),
    )
    .await
    .expect("send goal");

    // Drive the goal to a terminal state first.
    let res = ActionMessenger::request_result(&client_handle, &goal, Duration::from_secs(2))
        .await
        .expect("result");
    assert_eq!(res.status, ResultStatus::Completed);

    // Now cancel the already-finished goal: typed AlreadyTerminal, not an error.
    let ack = ActionMessenger::cancel_goal(&client_handle, &goal, Duration::from_secs(2))
        .await
        .expect("cancel a terminal goal still gets a reply");
    let state = decode_cancel_ack(ack.payload().as_ref()).expect("decode cancel ack");
    assert_eq!(state, CancelState::AlreadyTerminal);

    server.abort();
    drop(server_handle);
}

/// A single node exposes the *same* action name under two distinct iface
/// scopes (native + a conformed interface). Their goal services must wire to
/// distinct paths, so a `send_goal` targeting one scope must only ever hit
/// the matching server.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn action_iface_scoped_native_and_conformed_do_not_collide() {
    use peppylib::messaging::ActionFeedbackPublisherFactory;
    use tokio::sync::oneshot;

    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (host, port) = (instance.host.clone(), instance.port);

    let core_node = "test_core";
    let instance_id = "test_instance";
    let node_name = "test_node";
    let action_name = "move";
    let iface_name = "arm";
    let iface_tag = "v1";

    let native_response = Payload::from_static(b"native_ack");
    let iface_response = Payload::from_static(b"iface_ack");

    let native_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("native handle");
    let iface_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("iface handle");
    let caller_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("caller handle");

    // Expose under both scopes.
    let native_action = ActionMessenger::expose(
        &native_handle,
        core_node,
        instance_id,
        test_node_target(node_name),
        action_name,
    )
    .await
    .expect("native expose");
    let iface_action = ActionMessenger::expose(
        &iface_handle,
        core_node,
        instance_id,
        SenderTarget::interface(iface_name, iface_tag).expect("test target"),
        action_name,
    )
    .await
    .expect("iface expose");

    fn run_goal_handler(
        mut action: peppylib::messaging::ActionCreation,
        response: Payload,
    ) -> (tokio::task::JoinHandle<()>, oneshot::Receiver<()>) {
        let (ready_tx, ready_rx) = oneshot::channel();
        let factory: ActionFeedbackPublisherFactory = action.feedback_publisher_factory.clone();
        let handle = tokio::spawn(async move {
            let ready_tx = std::sync::Mutex::new(Some(ready_tx));
            let _publisher_keepalive: Arc<std::sync::Mutex<Option<ActionFeedbackPublisher>>> =
                Arc::new(std::sync::Mutex::new(None));
            let kept = Arc::clone(&_publisher_keepalive);
            let _ = action
                .goal_service
                .handle_next_request(|req| {
                    let factory = factory.clone();
                    let response = response.clone();
                    let kept = Arc::clone(&kept);
                    async move {
                        let declared = factory
                            .declare_from_wire("_", req.message().payload().into_inner())
                            .await
                            .expect("declare_from_wire");
                        kept.lock().unwrap().replace(declared.publisher);
                        Ok(response)
                    }
                })
                .await
                .expect("goal handler");
            if let Some(tx) = ready_tx.lock().unwrap().take() {
                let _ = tx.send(());
            }
        });
        (handle, ready_rx)
    }

    let (native_task, native_done) = run_goal_handler(native_action, native_response.clone());
    let (iface_task, iface_done) = run_goal_handler(iface_action, iface_response.clone());

    let native_goal = ActionMessenger::send_goal(
        &caller_handle,
        core_node,
        instance_id,
        test_node_target(node_name),
        action_name,
        Some(&ProducerRef::new(core_node, instance_id)),
        Payload::from_static(b"native_goal"),
        QoSProfile::Reliable,
        Duration::from_secs(2),
    )
    .await
    .expect("native send_goal");
    assert_eq!(
        native_goal.goal_response().payload(),
        &native_response,
        "native send_goal must hit the native goal handler",
    );

    let iface_goal = ActionMessenger::send_goal(
        &caller_handle,
        core_node,
        instance_id,
        SenderTarget::interface(iface_name, iface_tag).expect("test target"),
        action_name,
        Some(&ProducerRef::new(core_node, instance_id)),
        Payload::from_static(b"iface_goal"),
        QoSProfile::Reliable,
        Duration::from_secs(2),
    )
    .await
    .expect("iface send_goal");
    assert_eq!(
        iface_goal.goal_response().payload(),
        &iface_response,
        "iface send_goal must hit the iface goal handler",
    );

    native_done.await.expect("native handler signaled ready");
    iface_done.await.expect("iface handler signaled ready");
    native_task.await.expect("native task");
    iface_task.await.expect("iface task");
}

/// Discover-then-pin safety: when a consumer issues a wildcard
/// `ActionMessenger::send_goal` against two producers exposing the same
/// `(name, tag)`, only the discovered producer must run its goal handler.
/// The other receives only the discovery probe (filtered server-side) and
/// never executes the user goal handler.
///
/// This is the action analog of `service_from_any_poll_runs_handler_on_winner_only`
/// in `tests/services.rs`. It exists because actions are state-changing and
/// long-running: without discovery, every matching producer would run its
/// handler concurrently (e.g. two manipulators both moving to pick up the
/// same object).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn action_from_any_send_goal_runs_handler_on_winner_only() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::oneshot;

    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (host, port) = (instance.host.clone(), instance.port);

    let action_target = SenderTarget::interface("manipulator", "v1").expect("iface target");
    let action_name = "pick_up";
    let producer_a_core = "producer_a_core";
    let producer_a_inst = "producer_a";
    let producer_b_core = "producer_b_core";
    let producer_b_inst = "producer_b";

    struct ProducerSpec {
        core: &'static str,
        inst: &'static str,
        target: SenderTarget,
        action_name: &'static str,
    }

    async fn spawn_producer(
        host: String,
        port: u16,
        spec: ProducerSpec,
        goal_count: Arc<AtomicUsize>,
        ready: oneshot::Sender<()>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> tokio::task::JoinHandle<()> {
        let handle = MessengerHandle::from_host_port(&host, port)
            .await
            .expect("connect");
        tokio::spawn(async move {
            let action = ActionMessenger::expose(
                &handle,
                spec.core,
                spec.inst,
                spec.target,
                spec.action_name,
            )
            .await
            .expect("expose should succeed");
            let mut goal_service = action.goal_service;
            ready.send(()).expect("ready signal");

            // The winner receives the goal and responds; the loser never does
            // (the caller pins to one producer) and is released by `shutdown`
            // once the winner has been serviced. Using an explicit signal rather
            // than a fixed timeout keeps the test deterministic regardless of
            // how long peer-mode discovery takes to settle.
            tokio::select! {
                res = goal_service.recv_next_request() => {
                    if let Ok(Some((_ctx, responder))) = res {
                        goal_count.fetch_add(1, Ordering::SeqCst);
                        responder
                            .respond(Payload::from(spec.inst.as_bytes().to_vec()))
                            .await
                            .expect("goal respond");
                    }
                }
                _ = shutdown.changed() => {
                    // Loser of the discovery race — released without a goal.
                }
            }
        })
    }

    let goal_a = Arc::new(AtomicUsize::new(0));
    let goal_b = Arc::new(AtomicUsize::new(0));
    let (ready_a_tx, ready_a_rx) = oneshot::channel();
    let (ready_b_tx, ready_b_rx) = oneshot::channel();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let task_a = spawn_producer(
        host.clone(),
        port,
        ProducerSpec {
            core: producer_a_core,
            inst: producer_a_inst,
            target: action_target.clone(),
            action_name,
        },
        Arc::clone(&goal_a),
        ready_a_tx,
        shutdown_rx.clone(),
    )
    .await;
    let task_b = spawn_producer(
        host.clone(),
        port,
        ProducerSpec {
            core: producer_b_core,
            inst: producer_b_inst,
            target: action_target.clone(),
            action_name,
        },
        Arc::clone(&goal_b),
        ready_b_tx,
        shutdown_rx,
    )
    .await;

    ready_a_rx.await.expect("producer A ready");
    ready_b_rx.await.expect("producer B ready");

    let caller_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("caller connect");

    // send_goal performs a from_any discover-then-pin internally. In peer mode
    // discover_producer re-probes within its budget until the producers'
    // queryables propagate to this freshly-connected caller, so no external
    // readiness gate is needed — this exercises that cold-start retry directly.
    let goal_handle = ActionMessenger::send_goal(
        &caller_handle,
        "caller_core",
        "caller_inst",
        action_target,
        action_name,
        None,
        Payload::from_static(b"go"),
        QoSProfile::Reliable,
        Duration::from_secs(5),
    )
    .await
    .expect("send_goal should succeed");

    // Winner has been serviced; release the loser from its `recv_next_request`.
    shutdown_tx.send(true).expect("signal producers to stop");

    let winner_inst = goal_handle.goal_response().instance_id().to_string();
    assert!(
        winner_inst == producer_a_inst || winner_inst == producer_b_inst,
        "goal response identity must come from one of the producers, got {winner_inst:?}",
    );

    task_a.await.expect("producer A task panicked");
    task_b.await.expect("producer B task panicked");

    let (winner_goal, loser_goal) = if winner_inst == producer_a_inst {
        (goal_a.load(Ordering::SeqCst), goal_b.load(Ordering::SeqCst))
    } else {
        (goal_b.load(Ordering::SeqCst), goal_a.load(Ordering::SeqCst))
    };
    assert_eq!(
        winner_goal, 1,
        "winning producer ({winner_inst}) should run its goal handler exactly once",
    );
    assert_eq!(
        loser_goal, 0,
        "losing producer must NOT run its goal handler — discovery pins to the winner first",
    );
}
