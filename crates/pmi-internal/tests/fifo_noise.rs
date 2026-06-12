//! Invariant: the three zenoh adapter integration points (`call_service`,
//! `subscribe_keyexpr`, `listen_service`) must use the callback handler, not
//! zenoh's default `flume::bounded` FIFO handler. The FIFO handler logs
//! `error=sending on a closed channel` at ERROR (`target =
//! zenoh::api::handlers::fifo`) when zenoh delivers a reply/sample/query
//! after the receiver is dropped — which fires routinely once a consumer
//! takes the first reply of a wildcard service call and drops its
//! `ReplyStream` while the query window stays open.
//!
//! This test runs that scenario end-to-end against a real zenohd process and
//! asserts zero `zenoh::api::handlers::fifo` ERROR events. Failure means an
//! integration point was reverted to the FIFO handler.

#![cfg(feature = "build_zenoh")]

mod common;
use common::{ZENOH_SERIAL, test_node_target};

use bytes::Bytes;
use pmi::{
    MessengerBackend, Payload, ServiceKind, ServiceQueryKind, ServiceWireReceiver,
    ServiceWireSender, ZenohAdapter,
};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::Registry;

/// Process-wide counter of `zenoh::api::handlers::fifo` ERROR events. The
/// global subscriber installed by [`install_subscriber_once`] increments this
/// from arbitrary zenoh worker threads; tests reset and read it under the
/// `ZENOH_SERIAL` mutex so each test owns a clean window.
static FIFO_ERROR_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Counts ERROR-level events whose `target` starts with
/// `zenoh::api::handlers::fifo`. Cheap (one atomic add per matching event)
/// and side-effect-free for every other event.
struct CountingLayer;

impl<S: Subscriber> Layer<S> for CountingLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        if *metadata.level() == tracing::Level::ERROR
            && metadata.target().starts_with("zenoh::api::handlers::fifo")
        {
            FIFO_ERROR_COUNT.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Tracing's global default subscriber can only be set once per process, so
/// we lazy-install on first test entry and the same subscriber serves every
/// subsequent test in this binary.
fn install_subscriber_once() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        let subscriber = Registry::default().with(CountingLayer);
        tracing::subscriber::set_global_default(subscriber)
            .expect("no other tracing subscriber should be installed in this test binary");
    });
}

fn service_receiver(bound_core_node: &str, as_instance_id: &str) -> ServiceWireReceiver {
    ServiceWireReceiver::new(
        bound_core_node,
        as_instance_id,
        test_node_target("robot_arm"),
        "ping",
        ServiceKind::Service,
    )
    .expect("valid wire fields")
}

fn wildcard_service_sender() -> ServiceWireSender {
    ServiceWireSender::new(
        "client_core",
        "client_inst",
        None,
        test_node_target("robot_arm"),
        "ping",
        ServiceKind::Service,
    )
    .expect("valid wire fields")
}

/// Two distinct producers (different `bound_core_node`) listen on the same
/// service. Producer A responds immediately; producer B responds after a
/// delay long enough that the consumer has already received A's reply and
/// dropped the `ReplyStream`. With the FIFO handler this layout produced one
/// ERROR log per late reply (typically: B's reply, plus zenoh's session
/// timing-out the query). With the callback handler, late replies hit a
/// closure that silently no-ops on a dropped consumer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wildcard_service_call_emits_no_fifo_errors() {
    let _lock = ZENOH_SERIAL.lock().await;
    install_subscriber_once();
    FIFO_ERROR_COUNT.store(0, Ordering::Relaxed);

    let mut instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("start zenohd");
    instance.messenger().start_session().await.unwrap();

    let receiver_a = service_receiver("server_a_core", "server_a_inst");
    let receiver_b = service_receiver("server_b_core", "server_b_inst");

    let queryable_a = instance
        .messenger()
        .listen_service(&receiver_a)
        .await
        .unwrap();
    let queryable_b = instance
        .messenger()
        .listen_service(&receiver_b)
        .await
        .unwrap();

    // Let the two queryables propagate through zenoh's discovery before the
    // wildcard get goes out. Same delay the other integration tests use.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Server A: respond as soon as the query lands.
    let server_a = tokio::spawn(async move {
        let incoming = queryable_a
            .rx
            .recv_async()
            .await
            .expect("server A receives the wildcard query");
        incoming
            .token
            .respond_response(Payload::from_bytes(Bytes::from_static(b"reply_a")))
            .await
            .expect("server A respond");
    });

    // Server B: hold the response back long enough that the consumer has
    // already dropped its `ReplyStream` by the time we reply. 250ms is well
    // beyond the consumer's "take first reply and return" window below.
    let server_b = tokio::spawn(async move {
        let incoming = queryable_b
            .rx
            .recv_async()
            .await
            .expect("server B receives the wildcard query");
        tokio::time::sleep(Duration::from_millis(250)).await;
        incoming
            .token
            .respond_response(Payload::from_bytes(Bytes::from_static(b"reply_b_late")))
            .await
            .expect("server B respond");
    });

    let sender = wildcard_service_sender();
    let mut reply_stream = instance
        .messenger()
        .call_service(
            &sender,
            Payload::from_bytes(Bytes::from_static(b"ping?")),
            ServiceQueryKind::UserRequest,
            // Bound the zenoh-side `.timeout(...)` to one second so the
            // session finalizes within the test rather than holding onto
            // the query for `NO_TIMEOUT_SENTINEL` (24h).
            Some(Duration::from_secs(1)),
        )
        .await
        .unwrap();

    // Consumer side: take whichever reply arrives first (expected: A) and
    // drop the stream. This mirrors `discover_producer` / health-monitor
    // poll semantics, which is the call site that produced the original
    // log spam in production.
    let first = tokio::time::timeout(Duration::from_secs(2), reply_stream.rx.recv())
        .await
        .expect("first reply arrived within budget")
        .expect("reply stream did not close prematurely");
    let reply_a = Bytes::from_static(b"reply_a");
    let reply_b = Bytes::from_static(b"reply_b_late");
    let payload = first.message().payload();
    assert!(
        payload == &reply_a || payload == &reply_b,
        "first reply payload should be one of the two server responses",
    );
    drop(reply_stream);

    // Hold past server B's reply AND past the zenoh `.timeout(...)` so any
    // session-side late deliveries finish flushing through the (now-absent)
    // FIFO. If the regression returns, this is the window in which the
    // ERROR log would have fired.
    tokio::time::sleep(Duration::from_millis(1_500)).await;

    // Both server tasks should have completed (B's response went into the
    // void, but the responding task itself finished without panicking).
    server_a.await.expect("server A task did not panic");
    server_b.await.expect("server B task did not panic");

    let count = FIFO_ERROR_COUNT.load(Ordering::Relaxed);
    assert_eq!(
        count, 0,
        "expected zero `zenoh::api::handlers::fifo` ERROR events during a \
         wildcard service call with a late-replying sibling producer, but \
         observed {count}. This usually means one of the adapter's zenoh \
         integration points (`call_service`, `subscribe_keyexpr`, \
         `listen_service` in `crates/pmi-internal/src/adapters/zenoh.rs`) \
         was reverted from the callback handler back to the default FIFO \
         handler — re-introducing log spam on every wildcard discovery."
    );
}
