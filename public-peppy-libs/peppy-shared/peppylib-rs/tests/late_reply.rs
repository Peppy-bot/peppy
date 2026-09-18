//! A routed reply is discarded quietly after the real service caller times out.
//!
//! Only the caller's Tokio clock advances. Zenoh's independent runtimes keep a
//! long native query budget, bounded here by an external-I/O watchdog. The PMI
//! lifetime unit tests separately establish the exact ten-second wire grace.

mod common;

use common::test_node_target;
use peppylib::PeppyError;
use peppylib::messaging::{
    MessengerHandle, ProducerRef, ServiceMessenger, ServiceTarget, SessionScope,
};
use peppylib::testing::{acquire_mesh_serial, ensure_test_fd_limit, wait_service_reachable};
use peppylib::types::Payload;
use pmi::{MessengerBackend, ZenohAdapter};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, oneshot};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::Registry;

const SERVICE: &str = "late_reply_ping";
const SERVER_CORE: &str = "server_core";
const SERVER_INSTANCE: &str = "server_instance";
const CALL_BUDGET: Duration = Duration::from_secs(3600);
const IO_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Default)]
struct RecordedEvents {
    active: bool,
    query_id: Option<u32>,
    closed_queries: HashSet<u32>,
    acknowledged: bool,
    discarded: bool,
    noisy: Vec<String>,
    handoffs: Vec<String>,
}

#[derive(Default)]
struct Recorder {
    events: Mutex<RecordedEvents>,
    changed: Notify,
}

impl Recorder {
    fn start(&self) {
        *self.events.lock().unwrap() = RecordedEvents {
            active: true,
            ..RecordedEvents::default()
        };
    }

    async fn wait_for(&self, condition: impl Fn(&RecordedEvents) -> bool) {
        loop {
            let notified = self.changed.notified();
            if condition(&self.events.lock().unwrap()) {
                return;
            }
            notified.await;
        }
    }
}

#[derive(Default)]
struct EventFields {
    message: String,
    service_name: String,
    key_expr: String,
}

impl Visit for EventFields {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "message" => self.message = value.to_owned(),
            "service_name" => self.service_name = value.to_owned(),
            "key_expr" => self.key_expr = value.to_owned(),
            _ => {}
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.record_str(field, &format!("{value:?}"));
    }
}

struct RecordingLayer(Arc<Recorder>);

impl<S: Subscriber> Layer<S> for RecordingLayer {
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        let metadata = event.metadata();
        let target = metadata.target();
        let level = *metadata.level();
        let watched = target.starts_with("zenoh::")
            || target.starts_with("pmi::adapters::zenoh")
            || target.starts_with("peppylib::messaging");
        if !watched {
            return;
        }

        let mut events = self.0.events.lock().unwrap();
        if !events.active {
            return;
        }
        let mut fields = EventFields::default();
        event.record(&mut fields);
        if fields.message.starts_with("Register query ")
            || fields.message.starts_with("Close query ")
            || fields.message == "service request acknowledged"
            || fields.message == "discarding service reply after caller stopped waiting"
        {
            events.handoffs.push(format!(
                "{target}: {} (service={}, key={})",
                fields.message, fields.service_name, fields.key_expr
            ));
        }
        if level == Level::WARN || level == Level::ERROR {
            events
                .noisy
                .push(format!("{level} {target}: {}", fields.message));
        }
        if fields.message == "service request acknowledged" && fields.service_name == SERVICE {
            events.acknowledged = true;
        }
        if fields.message == "discarding service reply after caller stopped waiting"
            && fields.key_expr.contains(SERVICE)
        {
            events.discarded = true;
        }
        if target == "zenoh::api::session" {
            // Readiness is complete before recording starts. This service call
            // is the only query issuer, including any cold-start retries.
            if let Some(id) = fields.message.strip_prefix("Register query ") {
                events.query_id = id.split_whitespace().next().and_then(|id| id.parse().ok());
            }
            if let Some(id) = fields.message.strip_prefix("Close query ")
                && let Ok(id) = id.parse()
            {
                events.closed_queries.insert(id);
            }
        }
        drop(events);
        self.0.changed.notify_one();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn routed_late_reply_is_discarded_after_caller_timeout() {
    let _serial = acquire_mesh_serial().await;
    ensure_test_fd_limit();
    assert!(
        std::env::var_os("PEPPY_ZENOHD_LOG").is_none()
            || std::env::var("PEPPY_ZENOHD_LOG").as_deref() == Ok("zenoh=warn"),
        "this test requires PEPPY_ZENOHD_LOG unset or zenoh=warn to observe router warnings"
    );
    let recorder = Arc::new(Recorder::default());
    tracing::subscriber::set_global_default(
        Registry::default().with(RecordingLayer(Arc::clone(&recorder))),
    )
    .expect("this integration-test binary owns its tracing subscriber");

    tokio::time::timeout(IO_TIMEOUT, run_routed_late_reply(&recorder))
        .await
        .unwrap_or_else(|_| {
            panic!(
                "routed late-reply handoffs exceeded the external-I/O watchdog: {:#?}",
                recorder.events.lock().unwrap()
            )
        });
}

async fn run_routed_late_reply(recorder: &Arc<Recorder>) {
    let mut router = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("start zenohd");
    let namespace = config::namespace::Namespace::parse("late-reply-workspace").unwrap();
    // Namespace-scoped clients have neither listeners nor gossip links. The
    // producer and caller can exchange replies only through this router.
    let server = MessengerHandle::connect(&router.host, router.port)
        .scope(SessionScope::Namespace(namespace.clone()))
        .await
        .expect("connect producer client");
    let client = MessengerHandle::connect(&router.host, router.port)
        .scope(SessionScope::Namespace(namespace))
        .await
        .expect("connect caller client");
    let mut service = ServiceMessenger::listen(
        &server,
        SERVER_CORE,
        SERVER_INSTANCE,
        test_node_target("robot_arm"),
        SERVICE,
    )
    .await
    .expect("declare producer service");
    wait_service_reachable(
        &client,
        "client_core",
        "client_instance",
        test_node_target("robot_arm"),
        SERVICE,
        &ProducerRef::new(SERVER_CORE, SERVER_INSTANCE),
        IO_TIMEOUT,
    )
    .await
    .expect("service is routed before starting the call");

    let router_log_start = std::fs::read(&router.log_path)
        .expect("read the router's captured log")
        .len();
    recorder.start();
    let (advance_tx, advance_rx) = oneshot::channel();
    let caller_handle = client.clone();
    let caller = tokio::task::spawn_blocking(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .expect("build caller runtime");
        runtime.block_on(async move {
            // An active blocking task inhibits Tokio's automatic clock jumps
            // while the caller waits for real network I/O. Dropping the sender
            // releases it on both success and unwinding.
            let (hold_clock, clock_release) = std::sync::mpsc::channel::<()>();
            let (held_tx, held_rx) = oneshot::channel();
            let clock_guard = tokio::task::spawn_blocking(move || {
                let _ = held_tx.send(());
                let _ = clock_release.recv();
            });
            held_rx.await.expect("clock guard started");
            let call = tokio::spawn(async move {
                ServiceMessenger::poll(
                    &caller_handle,
                    "client_core",
                    "client_instance",
                    test_node_target("robot_arm"),
                    SERVICE,
                    ServiceTarget::Producer(&ProducerRef::new(SERVER_CORE, SERVER_INSTANCE)),
                    Payload::from_static(b"ping?"),
                    CALL_BUDGET,
                )
                .await
            });
            advance_rx.await.expect("test releases the caller clock");
            tokio::time::advance(CALL_BUDGET + Duration::from_millis(1)).await;
            let result = call.await.expect("caller task did not panic");
            drop(hold_clock);
            clock_guard.await.expect("clock guard did not panic");
            result
        })
    });

    let (request, responder) = service
        .recv_next_request()
        .await
        .expect("receive producer request")
        .expect("producer service remains open");
    assert_eq!(request.message().payload(), &Payload::from_static(b"ping?"));
    recorder
        .wait_for(|events| events.acknowledged && events.query_id.is_some())
        .await;
    advance_tx
        .send(())
        .expect("caller waits for clock advancement");
    let result = caller.await.expect("caller runtime did not panic");
    assert!(
        matches!(
            result,
            Err(PeppyError::ServiceTimeout { ref instance_id, ref service_name })
                if instance_id.as_deref() == Some(SERVER_INSTANCE) && service_name == SERVICE
        ),
        "the actual caller must time out after ACK, got {result:?}"
    );

    responder
        .respond(Payload::from_static(b"late reply"))
        .await
        .expect("producer can reply after the caller has returned");
    recorder
        .wait_for(|events| {
            events.discarded
                && events
                    .query_id
                    .is_some_and(|id| events.closed_queries.contains(&id))
        })
        .await;
    let noisy = recorder.events.lock().unwrap().noisy.clone();
    assert!(
        noisy.is_empty(),
        "unexpected session warnings/errors:\n{}",
        noisy.join("\n")
    );
    let router_log = std::fs::read(&router.log_path).expect("read router log after finalization");
    let router_window = String::from_utf8_lossy(
        router_log
            .get(router_log_start..)
            .expect("router log was not truncated"),
    );
    let router_noise: Vec<_> = router_window
        .lines()
        .filter(|line| line.contains("WARN") || line.contains("ERROR"))
        .collect();
    assert!(
        router_noise.is_empty(),
        "unexpected router warnings/errors:\n{}",
        router_noise.join("\n")
    );

    // Drop the sessions on the unpaused multithread runtime before the router.
    drop(service);
    drop(client);
    drop(server);
    router.messenger().stop_router().await.expect("stop zenohd");
}
