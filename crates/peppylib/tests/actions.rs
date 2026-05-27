mod common;

use common::test_node_target;
use config::node::QoSProfile;
use peppylib::PeppyError;
use peppylib::messaging::{
    ActionGoalHandle, ActionMessenger, ActionServer, EmptyPayloadError, GoalContext,
    MessengerHandle, NonEmptyPayload, SenderTarget, decode_cancel_ack,
};
use peppylib::types::Payload;
use pmi::ZenohAdapter;
use std::future::Future;
use std::time::Duration;

// Shared identity for the pinned single-producer tests. Wildcard / discovery
// tests below use their own targets.
const CORE: &str = "test_core";
const INST: &str = "test_instance";
const NODE: &str = "test_node";
const ACTION: &str = "test_action";

fn target() -> SenderTarget {
    test_node_target(NODE)
}

async fn router() -> pmi::ZenohdInstance {
    ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test")
}

async fn handle(host: &str, port: u16) -> MessengerHandle {
    MessengerHandle::from_host_port(host, port)
        .await
        .expect("failed to create messenger handle")
}

async fn expose_server(server_handle: &MessengerHandle) -> ActionServer {
    ActionMessenger::expose(server_handle, CORE, INST, target(), ACTION)
        .await
        .expect("expose should succeed")
}

/// Fire a goal pinned to the standard test producer.
async fn fire(client: &MessengerHandle, payload: &'static [u8]) -> ActionGoalHandle {
    ActionMessenger::send_goal(
        client,
        CORE,
        INST,
        target(),
        ACTION,
        Some(CORE),
        Some(INST),
        Payload::from_static(payload),
        QoSProfile::Reliable,
        Duration::from_secs(2),
    )
    .await
    .expect("send_goal should succeed")
}

/// Spawn an accept loop that responds `accepted` to every goal and runs
/// `worker(GoalContext)` on its own task, so goals progress concurrently.
/// The returned task owns the `ActionServer` (and thus the cancel/result
/// pumps); it ends when the goal service closes.
fn spawn_accept_loop<F, Fut>(mut server: ActionServer, worker: F) -> tokio::task::JoinHandle<()>
where
    F: Fn(GoalContext) -> Fut + Send + Clone + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        while let Ok(Some((req_ctx, responder))) = server.recv_next_goal().await {
            // Register before responding so a fast cancel/result can't miss
            // the slot.
            let Ok(goal_ctx) = server.register_goal(&req_ctx).await else {
                continue;
            };
            if responder
                .respond(Payload::from_static(b"accepted"))
                .await
                .is_err()
            {
                continue;
            }
            let worker = worker.clone();
            tokio::spawn(async move { worker(goal_ctx).await });
        }
    })
}

/// A single accepted goal handed back to the test: the server-side
/// `GoalContext` plus the client's goal handle, with the server, both
/// messenger handles, and the router kept alive for the test's lifetime.
struct OneGoal {
    server_ctx: GoalContext,
    goal_handle: ActionGoalHandle,
    client: MessengerHandle,
    _server: ActionServer,
    _server_handle: MessengerHandle,
    _client_keepalive: MessengerHandle,
    _router: pmi::ZenohdInstance,
}

/// Stand up a server, fire one goal, accept it, and return the wired-up
/// `OneGoal`. The server is NOT placed in an accept loop, so the test fully
/// controls completion/cancellation of this single goal.
async fn one_goal() -> OneGoal {
    let router = router().await;
    let server_handle = handle(&router.host, router.port).await;
    let client = handle(&router.host, router.port).await;

    let mut server = expose_server(&server_handle).await;
    // Allow the goal subscription to propagate before firing.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let fire_client = client.clone();
    let fire_task = tokio::spawn(async move { fire(&fire_client, b"goal data").await });

    let (req_ctx, responder) = server
        .recv_next_goal()
        .await
        .expect("recv goal")
        .expect("a goal request");
    let server_ctx = server.register_goal(&req_ctx).await.expect("register goal");
    responder
        .respond(Payload::from_static(b"accepted"))
        .await
        .expect("respond accepted");

    let goal_handle = fire_task.await.expect("fire task");

    OneGoal {
        server_ctx,
        goal_handle,
        client,
        _server: server,
        _server_handle: server_handle,
        _client_keepalive: handle(&router.host, router.port).await,
        _router: router,
    }
}

/// Full goal -> feedback -> result round trip on the concurrent API.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn action_messenger_communication() {
    let router = router().await;
    let server_handle = handle(&router.host, router.port).await;
    let client = handle(&router.host, router.port).await;

    let mut server = expose_server(&server_handle).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let server_task = tokio::spawn(async move {
        let (req_ctx, responder) = server
            .recv_next_goal()
            .await
            .expect("recv goal")
            .expect("a goal request");
        let ctx = server.register_goal(&req_ctx).await.expect("register");
        responder
            .respond(Payload::from_static(b"goal accepted"))
            .await
            .expect("respond accepted");
        ctx.publish_feedback(
            NonEmptyPayload::try_new(Payload::from_static(b"50% done")).expect("non-empty"),
        )
        .await
        .expect("publish feedback");
        ctx.complete(Payload::from_static(b"action result"))
            .await
            .expect("complete");
        // Hold the context until the test has fetched the result.
        tokio::time::sleep(Duration::from_secs(1)).await;
    });

    let mut goal_handle = fire(&client, b"goal data").await;
    assert_eq!(
        goal_handle.goal_response().payload(),
        &Payload::from_static(b"goal accepted"),
    );

    let feedback = tokio::time::timeout(Duration::from_secs(2), goal_handle.on_next_feedback())
        .await
        .expect("feedback within timeout")
        .expect("feedback not an error");
    assert_eq!(feedback.payload(), &Payload::from_static(b"50% done"));

    let result = ActionMessenger::request_result(&client, &goal_handle, Duration::from_secs(2))
        .await
        .expect("request_result should succeed");
    assert_eq!(result.payload(), &Payload::from_static(b"action result"));

    server_task.await.expect("server task should not panic");
}

/// Two goals fired at one server make progress in parallel: the fast goal's
/// result returns well before the slow goal finishes. This is the fix for the
/// goal-queue starvation symptom.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_goals_make_progress_in_parallel() {
    let router = router().await;
    let server_handle = handle(&router.host, router.port).await;
    let client = handle(&router.host, router.port).await;

    let server = expose_server(&server_handle).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let _accept = spawn_accept_loop(server, |ctx| async move {
        if ctx.request_bytes().starts_with(b"slow") {
            tokio::time::sleep(Duration::from_millis(600)).await;
            ctx.complete(Payload::from_static(b"slow-done")).await.ok();
        } else {
            ctx.complete(Payload::from_static(b"fast-done")).await.ok();
        }
        // Keep the context (and its retained result) alive for the fetch.
        tokio::time::sleep(Duration::from_secs(1)).await;
    });

    let slow = fire(&client, b"slow").await;
    let fast = fire(&client, b"fast").await;

    // The fast goal's result must arrive long before the slow goal's 600ms
    // sleep elapses; bound the wait to prove there's no head-of-line blocking.
    let fast_result = tokio::time::timeout(
        Duration::from_millis(250),
        ActionMessenger::request_result(&client, &fast, Duration::from_secs(2)),
    )
    .await
    .expect("fast result must not be blocked by the slow goal")
    .expect("fast result ok");
    assert_eq!(fast_result.payload(), &Payload::from_static(b"fast-done"));

    let slow_result = ActionMessenger::request_result(&client, &slow, Duration::from_secs(3))
        .await
        .expect("slow result ok");
    assert_eq!(slow_result.payload(), &Payload::from_static(b"slow-done"));
}

/// Feedback for one goal never lands on another goal's stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn feedback_isolation_between_goals() {
    let router = router().await;
    let server_handle = handle(&router.host, router.port).await;
    let client = handle(&router.host, router.port).await;

    let server = expose_server(&server_handle).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Each worker echoes its own request bytes as a feedback message, then
    // completes. If the shared-slot bug regressed, a client would see the
    // other goal's tag.
    let _accept = spawn_accept_loop(server, |ctx| async move {
        let tag = ctx.request_bytes().to_vec();
        ctx.publish_feedback(
            NonEmptyPayload::try_new(Payload::from(tag.clone())).expect("non-empty"),
        )
        .await
        .ok();
        ctx.complete(Payload::from(tag)).await.ok();
        tokio::time::sleep(Duration::from_secs(1)).await;
    });

    let mut goal_a = fire(&client, b"AAAA").await;
    let mut goal_b = fire(&client, b"BBBB").await;

    let fb_a = tokio::time::timeout(Duration::from_secs(2), goal_a.on_next_feedback())
        .await
        .expect("A feedback within timeout")
        .expect("A feedback ok");
    let fb_b = tokio::time::timeout(Duration::from_secs(2), goal_b.on_next_feedback())
        .await
        .expect("B feedback within timeout")
        .expect("B feedback ok");

    assert_eq!(fb_a.payload(), &Payload::from_static(b"AAAA"));
    assert_eq!(fb_b.payload(), &Payload::from_static(b"BBBB"));
}

/// A cancel routes to the named goal only: the cancelled goal observes its
/// signal (and completes "cancelled"), the other runs to normal completion.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_routes_to_correct_goal() {
    let router = router().await;
    let server_handle = handle(&router.host, router.port).await;
    let client = handle(&router.host, router.port).await;

    let server = expose_server(&server_handle).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Each worker races its cancel signal against a timer: cancel -> report
    // "cancelled", timer -> report "finished".
    let _accept = spawn_accept_loop(server, |ctx| async move {
        let outcome = tokio::select! {
            _ = ctx.cancel_signal() => Payload::from_static(b"cancelled"),
            _ = tokio::time::sleep(Duration::from_millis(700)) => Payload::from_static(b"finished"),
        };
        ctx.complete(outcome).await.ok();
        tokio::time::sleep(Duration::from_secs(1)).await;
    });

    let goal_a = fire(&client, b"a").await;
    let goal_b = fire(&client, b"b").await;

    let cancel_resp = ActionMessenger::cancel_goal(&client, &goal_a, Duration::from_secs(1))
        .await
        .expect("cancel_goal ok");
    assert!(
        decode_cancel_ack(cancel_resp.payload().as_ref()).expect("decode ack"),
        "cancel of an in-flight goal must be accepted",
    );

    let result_a = ActionMessenger::request_result(&client, &goal_a, Duration::from_secs(2))
        .await
        .expect("A result ok");
    assert_eq!(result_a.payload(), &Payload::from_static(b"cancelled"));

    let result_b = ActionMessenger::request_result(&client, &goal_b, Duration::from_secs(2))
        .await
        .expect("B result ok");
    assert_eq!(
        result_b.payload(),
        &Payload::from_static(b"finished"),
        "the un-cancelled goal must finish normally",
    );
}

/// Result rendezvous when `complete` runs before the client's `get_result`:
/// the buffered result is delivered to the later request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn result_rendezvous_complete_before_request() {
    let g = one_goal().await;
    g.server_ctx
        .complete(Payload::from_static(b"R"))
        .await
        .expect("complete");

    let result = ActionMessenger::request_result(&g.client, &g.goal_handle, Duration::from_secs(2))
        .await
        .expect("request_result ok");
    assert_eq!(result.payload(), &Payload::from_static(b"R"));
}

/// Result rendezvous when the client's `get_result` arrives before `complete`:
/// the request parks server-side and resolves when the worker completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn result_rendezvous_request_before_complete() {
    let OneGoal {
        server_ctx,
        goal_handle,
        client,
        _server,
        _server_handle,
        _client_keepalive,
        _router,
    } = one_goal().await;

    // Complete after a delay, on its own task.
    let completer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        server_ctx
            .complete(Payload::from_static(b"R"))
            .await
            .expect("complete");
        // Hold the context past the client's fetch.
        tokio::time::sleep(Duration::from_secs(1)).await;
    });

    // The request arrives first and must park, then resolve once complete fires.
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        ActionMessenger::request_result(&client, &goal_handle, Duration::from_secs(2)),
    )
    .await
    .expect("parked result must resolve after complete")
    .expect("request_result ok");
    assert_eq!(result.payload(), &Payload::from_static(b"R"));

    completer.await.expect("completer task");
}

/// A second `complete` is a no-op; the first result wins.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_complete_is_noop() {
    let g = one_goal().await;
    g.server_ctx
        .complete(Payload::from_static(b"first"))
        .await
        .expect("first complete");
    g.server_ctx
        .complete(Payload::from_static(b"second"))
        .await
        .expect("second complete is a no-op");

    let result = ActionMessenger::request_result(&g.client, &g.goal_handle, Duration::from_secs(2))
        .await
        .expect("request_result ok");
    assert_eq!(result.payload(), &Payload::from_static(b"first"));
}

/// A completed result is retained while the context is alive, so a client
/// retry (or a duplicate request) gets the same answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn result_retained_until_context_drop() {
    let g = one_goal().await;
    g.server_ctx
        .complete(Payload::from_static(b"R"))
        .await
        .expect("complete");

    let first = ActionMessenger::request_result(&g.client, &g.goal_handle, Duration::from_secs(2))
        .await
        .expect("first request ok");
    let second = ActionMessenger::request_result(&g.client, &g.goal_handle, Duration::from_secs(2))
        .await
        .expect("retry ok");
    assert_eq!(first.payload(), &Payload::from_static(b"R"));
    assert_eq!(second.payload(), &Payload::from_static(b"R"));
}

/// Cancelling a goal that is no longer in flight (its context was dropped)
/// returns `accepted: false` rather than firing anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_goal_id_cancel_returns_not_accepted() {
    let g = one_goal().await;
    drop(g.server_ctx); // evicts the slot
    tokio::time::sleep(Duration::from_millis(50)).await;

    let cancel_resp =
        ActionMessenger::cancel_goal(&g.client, &g.goal_handle, Duration::from_secs(1))
            .await
            .expect("cancel_goal ok");
    assert!(
        !decode_cancel_ack(cancel_resp.payload().as_ref()).expect("decode ack"),
        "cancel of an unknown goal must not be accepted",
    );
}

/// Requesting the result of a goal that is no longer in flight surfaces a
/// definitive error instead of hanging until timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_goal_id_result_errors() {
    let g = one_goal().await;
    drop(g.server_ctx); // evicts the slot
    tokio::time::sleep(Duration::from_millis(50)).await;

    let result =
        ActionMessenger::request_result(&g.client, &g.goal_handle, Duration::from_secs(1)).await;
    match result {
        Err(PeppyError::ServiceError { reason, .. }) => {
            assert!(
                reason.contains("no in-flight goal"),
                "expected a no-such-goal reason, got {reason:?}",
            );
        }
        other => panic!("expected a ServiceError for an unknown goal, got {other:?}"),
    }
}

/// `complete()` closes the feedback stream: the client's drain loop sees the
/// regular feedback, then `Err(ActionFeedbackChannelClosed)`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn complete_closes_feedback_stream() {
    let g = one_goal().await;

    g.server_ctx
        .publish_feedback(
            NonEmptyPayload::try_new(Payload::from_static(b"50% done")).expect("non-empty"),
        )
        .await
        .expect("publish feedback");
    g.server_ctx
        .complete(Payload::from_static(b"done"))
        .await
        .expect("complete");

    let mut goal_handle = g.goal_handle;
    let received = tokio::time::timeout(Duration::from_secs(2), goal_handle.on_next_feedback())
        .await
        .expect("feedback within timeout")
        .expect("feedback ok");
    assert_eq!(received.payload(), &Payload::from_static(b"50% done"));

    let closed = tokio::time::timeout(Duration::from_secs(2), goal_handle.on_next_feedback())
        .await
        .expect("close signal within timeout");
    match closed {
        Err(PeppyError::ActionFeedbackChannelClosed) => {}
        other => panic!("expected ActionFeedbackChannelClosed, got {other:?}"),
    }
}

/// Dropping the `GoalContext` without completing closes the feedback stream
/// (so a draining client doesn't hang), exercised via the non-blocking
/// `try_next_feedback` path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drop_context_without_complete_closes_feedback() {
    let OneGoal {
        server_ctx,
        mut goal_handle,
        client: _client,
        _server,
        _server_handle,
        _client_keepalive,
        _router,
    } = one_goal().await;

    drop(server_ctx); // fire-and-forget publish_end on drop

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
/// [`NonEmptyPayload::try_new`] rejects them, so `publish_feedback` can never
/// emit the empty payload reserved for the end-of-stream sentinel.
#[test]
fn non_empty_payload_rejects_empty_payload() {
    let result = NonEmptyPayload::try_new(Payload::new());
    assert!(
        matches!(result, Err(EmptyPayloadError)),
        "empty payload must be rejected by NonEmptyPayload::try_new",
    );
}

/// A single node exposes the *same* action name under two distinct iface
/// scopes (native + a conformed interface). Their goal services must wire to
/// distinct paths, so a `send_goal` targeting one scope must only ever hit the
/// matching server.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn action_iface_scoped_native_and_conformed_do_not_collide() {
    let router = router().await;
    let host = router.host.clone();
    let port = router.port;

    let core_node = "test_core";
    let instance_id = "test_instance";
    let node_name = "test_node";
    let action_name = "move";
    let iface_name = "arm";
    let iface_tag = "v1";

    let native_handle = handle(&host, port).await;
    let iface_handle = handle(&host, port).await;
    let caller_handle = handle(&host, port).await;

    let native_server = ActionMessenger::expose(
        &native_handle,
        core_node,
        instance_id,
        test_node_target(node_name),
        action_name,
    )
    .await
    .expect("native expose");
    let iface_server = ActionMessenger::expose(
        &iface_handle,
        core_node,
        instance_id,
        SenderTarget::interface(iface_name, iface_tag).expect("test target"),
        action_name,
    )
    .await
    .expect("iface expose");

    // Each server accepts one goal and responds with its own tag. We only
    // assert on the goal response routing, so the worker just completes.
    fn run_one(mut server: ActionServer, response: &'static [u8]) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let (req_ctx, responder) = server
                .recv_next_goal()
                .await
                .expect("recv goal")
                .expect("a goal");
            let ctx = server.register_goal(&req_ctx).await.expect("register");
            responder
                .respond(Payload::from_static(response))
                .await
                .expect("respond");
            ctx.complete(Payload::from_static(b"done")).await.ok();
            // Keep server + context alive past the assertions.
            tokio::time::sleep(Duration::from_secs(1)).await;
        })
    }

    let native_task = run_one(native_server, b"native_ack");
    let iface_task = run_one(iface_server, b"iface_ack");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let native_goal = ActionMessenger::send_goal(
        &caller_handle,
        core_node,
        instance_id,
        test_node_target(node_name),
        action_name,
        Some(core_node),
        Some(instance_id),
        Payload::from_static(b"native_goal"),
        QoSProfile::Reliable,
        Duration::from_secs(2),
    )
    .await
    .expect("native send_goal");
    assert_eq!(
        native_goal.goal_response().payload(),
        &Payload::from_static(b"native_ack"),
        "native send_goal must hit the native goal handler",
    );

    let iface_goal = ActionMessenger::send_goal(
        &caller_handle,
        core_node,
        instance_id,
        SenderTarget::interface(iface_name, iface_tag).expect("test target"),
        action_name,
        Some(core_node),
        Some(instance_id),
        Payload::from_static(b"iface_goal"),
        QoSProfile::Reliable,
        Duration::from_secs(2),
    )
    .await
    .expect("iface send_goal");
    assert_eq!(
        iface_goal.goal_response().payload(),
        &Payload::from_static(b"iface_ack"),
        "iface send_goal must hit the iface goal handler",
    );

    native_task.abort();
    iface_task.abort();
}

/// Discover-then-pin safety: a wildcard `send_goal` against two producers
/// exposing the same `(name, tag)` runs exactly one producer's goal handler.
/// The other receives only the discovery probe (filtered server-side by
/// `recv_next_goal`) and never yields a real goal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn action_from_any_send_goal_runs_handler_on_winner_only() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::oneshot;

    let router = router().await;
    let host = router.host.clone();
    let port = router.port;

    let action_target = SenderTarget::interface("manipulator", "v1").expect("iface target");
    let action_name = "pick_up";

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
    ) -> tokio::task::JoinHandle<()> {
        let messenger = handle(&host, port).await;
        tokio::spawn(async move {
            let mut server = ActionMessenger::expose(
                &messenger,
                spec.core,
                spec.inst,
                spec.target,
                spec.action_name,
            )
            .await
            .expect("expose should succeed");
            ready.send(()).expect("ready signal");

            // The loser must time out here; the winner returns a real goal.
            match tokio::time::timeout(Duration::from_millis(1000), server.recv_next_goal()).await {
                Ok(Ok(Some((_req_ctx, responder)))) => {
                    goal_count.fetch_add(1, Ordering::SeqCst);
                    responder
                        .respond(Payload::from(spec.inst.as_bytes().to_vec()))
                        .await
                        .expect("goal respond");
                }
                _ => {
                    // Loser of the discovery race; expected outcome.
                }
            }
        })
    }

    let goal_a = Arc::new(AtomicUsize::new(0));
    let goal_b = Arc::new(AtomicUsize::new(0));
    let (ready_a_tx, ready_a_rx) = oneshot::channel();
    let (ready_b_tx, ready_b_rx) = oneshot::channel();

    let task_a = spawn_producer(
        host.clone(),
        port,
        ProducerSpec {
            core: "producer_a_core",
            inst: "producer_a",
            target: action_target.clone(),
            action_name,
        },
        Arc::clone(&goal_a),
        ready_a_tx,
    )
    .await;
    let task_b = spawn_producer(
        host.clone(),
        port,
        ProducerSpec {
            core: "producer_b_core",
            inst: "producer_b",
            target: action_target.clone(),
            action_name,
        },
        Arc::clone(&goal_b),
        ready_b_tx,
    )
    .await;

    ready_a_rx.await.expect("producer A ready");
    ready_b_rx.await.expect("producer B ready");
    tokio::time::sleep(Duration::from_millis(100)).await;

    let caller_handle = handle(&host, port).await;
    let goal_handle = ActionMessenger::send_goal(
        &caller_handle,
        "caller_core",
        "caller_inst",
        action_target,
        action_name,
        None,
        None,
        Payload::from_static(b"go"),
        QoSProfile::Reliable,
        Duration::from_secs(2),
    )
    .await
    .expect("send_goal should succeed");

    let winner_inst = goal_handle.goal_response().instance_id().to_string();
    assert!(
        winner_inst == "producer_a" || winner_inst == "producer_b",
        "goal response identity must come from one of the producers, got {winner_inst:?}",
    );

    task_a.await.expect("producer A task panicked");
    task_b.await.expect("producer B task panicked");

    let (winner_goal, loser_goal) = if winner_inst == "producer_a" {
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
        "losing producer must NOT run its goal handler; discovery pins to the winner first",
    );
}
