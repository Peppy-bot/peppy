use super::{
    MessengerHandle, ProducerRef, SenderTarget, ServiceMessenger, ServiceTarget,
    within_service_deadline,
};
use crate::error::{Error, Result};
use crate::types::{Message, Payload};
use futures::{
    FutureExt, poll,
    task::{ArcWake, waker_ref},
};
use pmi::{
    Messenger, MessengerAdapter, MessengerBackend, MockAdapter, ServiceKind, ServiceQueryKind,
    ServiceQueryable, ServiceWireReceiver, ServiceWireSender,
};
use std::{
    cell::Cell,
    future::{Future, poll_fn, ready},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::{
    sync::{Mutex, Notify, oneshot},
    time::{Duration, Instant, advance, sleep_until},
};

const BUDGET: Duration = Duration::from_secs(1);
const CORE: &str = "deadline_core";
const PRODUCER: &str = "deadline_producer";
const SERVICE: &str = "deadline_service";

fn target() -> SenderTarget {
    SenderTarget::node("deadline_node", "v1").unwrap()
}

fn sender() -> ServiceWireSender {
    ServiceWireSender::new(
        CORE,
        "deadline_caller",
        Some(&ProducerRef::new(CORE, PRODUCER)),
        target(),
        SERVICE,
        ServiceKind::Service,
    )
    .unwrap()
}

async fn mock_handle() -> MessengerHandle {
    let mut messenger = Messenger::new(MessengerAdapter::Mock(MockAdapter::default()));
    messenger.start_session().await.unwrap();
    MessengerHandle::from_shared(Arc::new(Mutex::new(messenger)))
}

async fn listen(handle: &MessengerHandle) -> ServiceQueryable {
    let receiver =
        ServiceWireReceiver::new(CORE, PRODUCER, target(), SERVICE, ServiceKind::Service).unwrap();
    handle
        .messenger
        .lock()
        .await
        .listen_service(&receiver)
        .await
        .unwrap()
}

fn assert_deadline_error(result: Result<Message>, received_ack: bool) {
    match (received_ack, result.unwrap_err()) {
        (
            false,
            Error::ServiceUnreachable {
                instance_id,
                service_name,
            },
        )
        | (
            true,
            Error::ServiceTimeout {
                instance_id,
                service_name,
            },
        ) => {
            assert_eq!(instance_id.as_deref(), Some(PRODUCER));
            assert_eq!(service_name, SERVICE);
        }
        (_, error) => panic!("wrong deadline classification for ACK={received_ack}: {error:?}"),
    }
}

// A dedicated waker gates manual polling on this call's actual channel or timer
// wakeups, without polling unrelated tasks or guessing scheduler yield counts.
#[derive(Default)]
struct CallWake(Notify);

impl ArcWake for CallWake {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.0.notify_one();
    }
}

fn poll_once<F: Future>(future: Pin<&mut F>, wake: &Arc<CallWake>) -> Poll<F::Output> {
    let _ = wake.0.notified().now_or_never();
    future.poll(&mut Context::from_waker(&waker_ref(wake)))
}

async fn assert_no_request(handle: &MessengerHandle, queryable: &ServiceQueryable) {
    // The same queryable processes this probe after every preceding query, so
    // its response fences the forwarder before checking the request channel.
    handle
        .poll_service(&sender(), Payload::new(), ServiceQueryKind::Probe, None)
        .await
        .unwrap();
    assert!(matches!(
        queryable.rx.try_recv(),
        Err(flume::TryRecvError::Empty)
    ));
}

#[tokio::test(start_paused = true)]
async fn a_ready_future_completes_after_the_deadline_has_passed() {
    let deadline = Instant::now();
    advance(BUDGET).await;
    let polled = Cell::new(false);
    let future = poll_fn(|_| {
        polled.set(true);
        Poll::Ready(42)
    });
    assert_eq!(
        within_service_deadline(Some(deadline), future).await,
        Some(42)
    );
    assert!(polled.get());
}

#[tokio::test(start_paused = true)]
async fn pending_issuance_is_cancelled_at_the_deadline() {
    let (tx, rx) = oneshot::channel::<()>();
    let deadline = Instant::now() + BUDGET;
    let mut issuance = Box::pin(within_service_deadline(Some(deadline), rx));
    assert!(poll!(issuance.as_mut()).is_pending());
    sleep_until(deadline).await;
    assert!(matches!(poll!(issuance.as_mut()), Poll::Ready(None)));
    assert!(tx.is_closed(), "expiry must drop the pending future");
}

#[tokio::test(start_paused = true)]
async fn a_result_ready_when_polled_wins_at_and_after_the_deadline() {
    for elapsed in [
        BUDGET - Duration::from_millis(1),
        BUDGET,
        BUDGET + Duration::from_millis(1),
    ] {
        let (tx, rx) = oneshot::channel();
        let mut wait = Box::pin(within_service_deadline(Some(Instant::now() + BUDGET), rx));
        assert!(poll!(wait.as_mut()).is_pending());
        advance(elapsed).await;
        tx.send(42).unwrap();
        assert_eq!(wait.await, Some(Ok(42)), "elapsed {elapsed:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn optional_deadline_preserves_future_results() {
    for deadline in [Some(Instant::now() + BUDGET), None] {
        assert_eq!(
            within_service_deadline(deadline, ready(Err::<(), _>("issuance failed"))).await,
            Some(Err("issuance failed"))
        );
    }
    let (tx, rx) = oneshot::channel();
    let mut unbounded = Box::pin(within_service_deadline(None, rx));
    assert!(poll!(unbounded.as_mut()).is_pending());
    advance(Duration::from_secs(86_400)).await;
    assert!(poll!(unbounded.as_mut()).is_pending());
    tx.send(42).unwrap();
    assert_eq!(unbounded.await, Some(Ok(42)));
}

#[tokio::test(start_paused = true)]
async fn mutex_wait_expires_without_issuing_even_if_released_late() {
    for release_before_poll in [false, true] {
        let handle = mock_handle().await;
        let queryable = listen(&handle).await;
        let sender = sender();
        let mut guard = Some(handle.messenger.lock().await);
        let deadline = Instant::now() + BUDGET;
        let mut call = Box::pin(handle.poll_service(
            &sender,
            Payload::new(),
            ServiceQueryKind::UserRequest,
            BUDGET,
        ));
        assert!(poll!(call.as_mut()).is_pending());
        sleep_until(deadline).await;
        if release_before_poll {
            drop(guard.take());
        }
        let Poll::Ready(result) = poll!(call.as_mut()) else {
            panic!("the mutex wait must finish at the caller deadline");
        };
        assert_deadline_error(result, false);
        drop(guard);
        assert_no_request(&handle, &queryable).await;
    }
}

#[tokio::test(start_paused = true)]
async fn lock_wait_is_subtracted_from_the_transport_budget() {
    let immediate_handle = mock_handle().await;
    let immediate_service = listen(&immediate_handle).await;
    let delayed_handle = mock_handle().await;
    let delayed_service = listen(&delayed_handle).await;
    let guard = delayed_handle.messenger.lock().await;
    let sender = sender();
    let started = Instant::now();
    let mut immediate = Box::pin(immediate_handle.poll_service(
        &sender,
        Payload::new(),
        ServiceQueryKind::UserRequest,
        BUDGET,
    ));
    let mut delayed = Box::pin(delayed_handle.poll_service(
        &sender,
        Payload::new(),
        ServiceQueryKind::UserRequest,
        BUDGET,
    ));
    let immediate_wake = Arc::new(CallWake::default());
    let delayed_wake = Arc::new(CallWake::default());
    assert!(poll_once(immediate.as_mut(), &immediate_wake).is_pending());
    assert!(poll_once(delayed.as_mut(), &delayed_wake).is_pending());
    let immediate_request = immediate_service.rx.recv_async().await.unwrap();
    immediate_request.token.respond_ack().await.unwrap();
    immediate_wake.0.notified().await;
    assert!(poll_once(immediate.as_mut(), &immediate_wake).is_pending());

    let lock_wait = Duration::from_millis(250);
    advance(lock_wait).await;
    drop(guard);
    delayed_wake.0.notified().await;
    assert!(poll_once(delayed.as_mut(), &delayed_wake).is_pending());
    let delayed_request = delayed_service.rx.recv_async().await.unwrap();
    delayed_request.token.respond_ack().await.unwrap();
    delayed_wake.0.notified().await;
    assert!(poll_once(delayed.as_mut(), &delayed_wake).is_pending());

    advance(BUDGET - lock_wait).await;
    tokio::join!(immediate_wake.0.notified(), delayed_wake.0.notified());
    assert_eq!(Instant::now(), started + BUDGET);

    // Leave both callers unpolled after their deadline wakeups. Their native
    // reply pumps then close the pending receivers after the transport grace.
    // Equal caller deadlines must give equal wire lifetimes despite lock waits.
    let (immediate_closed, delayed_closed) = tokio::join!(
        async {
            immediate_wake.0.notified().await;
            Instant::now()
        },
        async {
            delayed_wake.0.notified().await;
            Instant::now()
        },
    );
    assert!(immediate_closed > started + BUDGET);
    assert_eq!(delayed_closed, immediate_closed);
    assert_deadline_error(immediate.await, true);
    assert_deadline_error(delayed.await, true);
}

#[tokio::test(start_paused = true)]
async fn pending_reply_expiry_preserves_ack_classification() {
    for received_ack in [false, true] {
        let handle = mock_handle().await;
        let queryable = listen(&handle).await;
        let sender = sender();
        let deadline = Instant::now() + BUDGET;
        let wake = Arc::new(CallWake::default());
        let mut call = Box::pin(handle.poll_service(
            &sender,
            Payload::new(),
            ServiceQueryKind::UserRequest,
            BUDGET,
        ));
        assert!(poll_once(call.as_mut(), &wake).is_pending());
        let request = queryable.rx.recv_async().await.unwrap();
        if received_ack {
            request.token.respond_ack().await.unwrap();
            wake.0.notified().await;
            assert!(poll_once(call.as_mut(), &wake).is_pending());
        }
        sleep_until(deadline).await;
        let Poll::Ready(result) = poll_once(call.as_mut(), &wake) else {
            panic!("pending reply must expire at the caller deadline");
        };
        assert_deadline_error(result, received_ack);
    }
}

#[tokio::test(start_paused = true)]
async fn a_terminal_reply_in_hand_wins_when_the_caller_resumes_after_expiry() {
    for received_ack in [false, true] {
        for handler_error in [false, true] {
            let handle = mock_handle().await;
            let queryable = listen(&handle).await;
            let sender = sender();
            let wake = Arc::new(CallWake::default());
            let mut call = Box::pin(handle.poll_service(
                &sender,
                Payload::new(),
                ServiceQueryKind::UserRequest,
                BUDGET,
            ));
            assert!(poll_once(call.as_mut(), &wake).is_pending());
            let request = queryable.rx.recv_async().await.unwrap();
            if received_ack {
                request.token.respond_ack().await.unwrap();
                wake.0.notified().await;
                assert!(poll_once(call.as_mut(), &wake).is_pending());
            }
            // Keep the caller parked after its timer fires. The transport
            // still accepts a terminal reply during its independent grace, and
            // the producer has done the work by then.
            advance(BUDGET + Duration::from_millis(1)).await;
            wake.0.notified().await;
            if handler_error {
                request
                    .token
                    .respond_handler_error("handler failed".into())
                    .await
                    .unwrap();
            } else {
                request
                    .token
                    .respond_response(Payload::from_static(b"reply").into_inner().into())
                    .await
                    .unwrap();
            }
            // Both the expired timer and the terminal reply are ready before
            // the caller resumes: the reply in hand is what the caller gets.
            wake.0.notified().await;
            let result = call.await;
            if handler_error {
                match result.unwrap_err() {
                    Error::ServiceError {
                        instance_id,
                        service_name,
                        reason,
                    } => {
                        assert_eq!(instance_id.as_deref(), Some(PRODUCER));
                        assert_eq!(service_name, SERVICE);
                        assert_eq!(reason, "handler failed");
                    }
                    error => panic!("expected the handler error in hand, got {error:?}"),
                }
            } else {
                assert_eq!(
                    result.unwrap().payload(),
                    &Payload::from_static(b"reply"),
                    "ACK={received_ack}"
                );
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn timely_terminal_replies_preserve_payloads_and_handler_errors() {
    for response_timeout in [Some(BUDGET), None] {
        for handler_error in [false, true] {
            let handle = mock_handle().await;
            let queryable = listen(&handle).await;
            let sender = sender();
            let started = Instant::now();
            let mut call = Box::pin(handle.poll_service(
                &sender,
                Payload::new(),
                ServiceQueryKind::UserRequest,
                response_timeout,
            ));
            assert!(poll!(call.as_mut()).is_pending());
            let request = queryable.rx.recv_async().await.unwrap();
            request.token.respond_ack().await.unwrap();
            if handler_error {
                request
                    .token
                    .respond_handler_error("handler failed".into())
                    .await
                    .unwrap();
                match call.await.unwrap_err() {
                    Error::ServiceError {
                        instance_id,
                        service_name,
                        reason,
                    } => {
                        assert_eq!(instance_id.as_deref(), Some(PRODUCER));
                        assert_eq!(service_name, SERVICE);
                        assert_eq!(reason, "handler failed");
                    }
                    error => panic!("expected handler error, got {error:?}"),
                }
            } else {
                request
                    .token
                    .respond_response(Payload::from_static(b"reply").into_inner().into())
                    .await
                    .unwrap();
                let reply = call.await.unwrap();
                assert_eq!(reply.payload(), &Payload::from_static(b"reply"));
                assert_eq!(reply.instance_id(), PRODUCER);
                assert_eq!(reply.core_node(), CORE);
            }
            assert_eq!(Instant::now(), started);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn stream_closure_without_a_retry_preserves_ack_classification() {
    for (response_timeout, received_ack) in [(None, false), (None, true), (Some(BUDGET), true)] {
        let handle = mock_handle().await;
        let queryable = listen(&handle).await;
        let sender = sender();
        let started = Instant::now();
        let wake = Arc::new(CallWake::default());
        let mut call = Box::pin(handle.poll_service(
            &sender,
            Payload::new(),
            ServiceQueryKind::UserRequest,
            response_timeout,
        ));
        assert!(poll_once(call.as_mut(), &wake).is_pending());
        let request = queryable.rx.recv_async().await.unwrap();
        if received_ack {
            request.token.respond_ack().await.unwrap();
            wake.0.notified().await;
            assert!(poll_once(call.as_mut(), &wake).is_pending());
        }
        drop(request);
        wake.0.notified().await;
        let Poll::Ready(result) = poll_once(call.as_mut(), &wake) else {
            panic!("closed reply stream must finish without a retry");
        };
        assert_deadline_error(result, received_ack);
        assert_eq!(Instant::now(), started);
        assert_no_request(&handle, &queryable).await;
    }
}

#[tokio::test(start_paused = true)]
async fn cold_start_retry_reaches_a_new_producer_after_backoff() {
    let handle = mock_handle().await;
    let sender = sender();
    let started = Instant::now();
    let wake = Arc::new(CallWake::default());
    let mut call = Box::pin(handle.poll_service(
        &sender,
        Payload::new(),
        ServiceQueryKind::UserRequest,
        BUDGET,
    ));
    assert!(poll_once(call.as_mut(), &wake).is_pending());
    wake.0.notified().await;
    assert!(poll_once(call.as_mut(), &wake).is_pending());

    let queryable = listen(&handle).await;
    advance(Duration::from_millis(49)).await;
    assert!(poll_once(call.as_mut(), &wake).is_pending());
    assert_no_request(&handle, &queryable).await;
    sleep_until(started + Duration::from_millis(50)).await;
    assert!(poll_once(call.as_mut(), &wake).is_pending());
    let request = queryable.rx.recv_async().await.unwrap();
    request.token.respond_ack().await.unwrap();
    request
        .token
        .respond_response(Payload::from_static(b"retry reply").into_inner().into())
        .await
        .unwrap();
    assert_eq!(
        call.await.unwrap().payload(),
        &Payload::from_static(b"retry reply")
    );
    assert_eq!(Instant::now(), started + Duration::from_millis(50));
    assert_no_request(&handle, &queryable).await;
}

#[tokio::test(start_paused = true)]
async fn cold_start_retries_share_one_deadline_and_cap_the_final_backoff() {
    let handle = mock_handle().await;
    let queryable = listen(&handle).await;
    let sender = sender();
    let started = Instant::now();
    let budget = Duration::from_millis(125);
    let wake = Arc::new(CallWake::default());
    let mut call = Box::pin(handle.poll_service(
        &sender,
        Payload::new(),
        ServiceQueryKind::UserRequest,
        budget,
    ));
    for attempt in 0..3 {
        assert!(poll_once(call.as_mut(), &wake).is_pending());
        let request = queryable.rx.recv_async().await.unwrap();
        assert_eq!(
            Instant::now(),
            started + Duration::from_millis(attempt * 50)
        );
        drop(request);
        wake.0.notified().await;
        assert!(poll_once(call.as_mut(), &wake).is_pending());
        let next_attempt = Duration::from_millis((attempt + 1) * 50).min(budget);
        sleep_until(started + next_attempt).await;
    }
    let Poll::Ready(result) = poll_once(call.as_mut(), &wake) else {
        panic!("retry backoff must not exceed the remaining caller budget");
    };
    assert_deadline_error(result, false);
    assert_eq!(Instant::now(), started + budget);
    assert_no_request(&handle, &queryable).await;
}

#[tokio::test(start_paused = true)]
async fn unbounded_call_waits_for_the_lock_and_response() {
    let handle = mock_handle().await;
    let queryable = listen(&handle).await;
    let sender = sender();
    let guard = handle.messenger.lock().await;
    let mut call =
        Box::pin(handle.poll_service(&sender, Payload::new(), ServiceQueryKind::UserRequest, None));
    assert!(poll!(call.as_mut()).is_pending());
    advance(BUDGET * 100).await;
    assert!(poll!(call.as_mut()).is_pending());
    drop(guard);
    assert!(poll!(call.as_mut()).is_pending());
    let request = queryable.rx.recv_async().await.unwrap();
    advance(BUDGET * 100).await;
    assert!(poll!(call.as_mut()).is_pending());
    request
        .token
        .respond_response(Payload::from_static(b"unbounded reply").into_inner().into())
        .await
        .unwrap();
    assert_eq!(
        call.await.unwrap().payload(),
        &Payload::from_static(b"unbounded reply")
    );
}

#[tokio::test(start_paused = true)]
async fn issuance_errors_remain_transport_errors() {
    let handle = mock_handle().await;
    handle.messenger.lock().await.stop_session().await.unwrap();
    for response_timeout in [Some(BUDGET), None] {
        let error = handle
            .poll_service(
                &sender(),
                Payload::new(),
                ServiceQueryKind::UserRequest,
                response_timeout,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, Error::PeppyMessagingInterface(pmi::PeppyMessagingInterfaceError::PublishError { topic })
            if topic == SERVICE)
        );
    }
}

#[tokio::test(start_paused = true)]
async fn discovery_and_the_user_request_share_the_tokio_budget() {
    let handle = mock_handle().await;
    let queryable = listen(&handle).await;
    let guard = handle.messenger.lock().await;
    let started = Instant::now();
    let mut call = Box::pin(ServiceMessenger::poll(
        &handle,
        CORE,
        "deadline_caller",
        target(),
        SERVICE,
        ServiceTarget::Any,
        Payload::new(),
        BUDGET,
    ));
    assert!(poll!(call.as_mut()).is_pending());
    let discovery_elapsed = Duration::from_millis(400);
    advance(discovery_elapsed).await;
    drop(guard);
    let _request = tokio::select! {
        result = call.as_mut() => panic!("call finished before the user request: {result:?}"),
        request = queryable.rx.recv_async() => request.unwrap(),
    };
    assert_eq!(Instant::now(), started + discovery_elapsed);
    sleep_until(started + BUDGET).await;
    let Poll::Ready(result) = poll!(call.as_mut()) else {
        panic!("discovery time must count against the user request's budget");
    };
    assert_deadline_error(result, false);
    assert_eq!(Instant::now(), started + BUDGET);
}
