//! Node-invariant test machinery behind the `testing` cargo feature.
//!
//! Everything here is untyped (`Payload` in, bytes out) on purpose: this module
//! is the single implementation — per language — of the semantics that
//! generated per-node test code (peppygen's `mock` / `fixtures` surfaces) and
//! peppylib's own test binaries share. Generated code contributes only a typed
//! veneer over these cores: message-type reuse, codecs, identity constants,
//! per-link aggregation. If a helper here ever wants to name a generated type,
//! that logic belongs in the veneer instead; error types are safe because
//! generated crates re-export [`crate::PeppyError`] as their own error type.
//!
//! The module ships in the library (not `#[cfg(test)]`) so generated node test
//! code can use it, but only compiles when the `testing` feature is enabled —
//! node crates enable it via `[dev-dependencies]`, which `cargo build` never
//! resolves, so none of this can reach a production binary.
//!
//! The Python twin (`peppylib-py/peppylib/testing.py`) mirrors this module
//! member for member, plus exactly two of its own: `resolve_node_dir` and
//! `Mocks`. Both cover ground a Rust harness gets for free — the config path
//! is a compile-time `concat!(env!("CARGO_MANIFEST_DIR"), …)` const, and the
//! generated `Mocks` struct tears down by `Drop` — so neither is a gap here.
//! Any *other* asymmetry between the two files is a bug.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Once, PoisonError, Weak};
use std::time::Duration;

use crate::clock::{ClockSource, ClockSourceError, WallClockSource};
use crate::error::{Error, Result};
use crate::messaging::{
    ConcurrentAction, GoalContext, MessengerHandle, PendingGoal, SenderTarget, ServiceEndpoint,
    ServiceMessenger, ServiceRequestContext, ServiceResponder, TopicMessenger, TopicPublisher,
};
use crate::runtime::{
    CancellationToken, NodeBuilder, NodeRunner, StandaloneConfig, TaskHandle, spawn,
};
use crate::types::{Message, Payload};
use config::node::QoSProfile;
use core_node_api::encoding::ClockTick;
use core_node_api::{ServiceId, TopicId, names};
use pmi::{MessengerBackend, ZenohAdapter, ZenohdInstance};
use tracing::warn;

// The identity segment generated test surfaces pin the node-under-test with;
// re-exported here so peppygen's veneers reference it instead of embedding
// the literal (see [`crate::runtime::processor::STANDALONE_CORE_NODE`]).
pub use crate::runtime::STANDALONE_CORE_NODE;
use config::runtime::{ClockDomainId, ClockIncarnation};

/// How long readiness waits (subscriber matching, service/action reachability)
/// may take before failing loudly. Generous on purpose: every wait returns the
/// moment its condition is observed, so the bound only prices the failure
/// path, never the happy path.
pub const READINESS_TIMEOUT: Duration = Duration::from_secs(10);

/// Retry budget for opening a session against a router that just reported
/// ready: zenohd can accept its readiness probe and still refuse the very next
/// connect for a few milliseconds under load.
const CONNECT_RETRIES: u32 = 5;
const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(200);

/// Pre-main: gives zenoh's global Net runtime more worker threads for every
/// test binary that links this module. Stock zenoh (1.9.0 through at least
/// 1.10.1, the fix below has not shipped in a release) can deadlock its
/// routing layer under peer-session churn: a thread holding the routing
/// `ctrl_lock` parks in `block_in_place` waiting on the StartConditions
/// mutex while the Net runtime's single default worker blocks on that same
/// `ctrl_lock`, wedging the mutex queue (fix pending upstream in
/// https://github.com/eclipse-zenoh/zenoh/pull/2637). With more Net workers
/// a free worker can always drain the mutex queue, which un-parks the lock
/// holder.
///
/// Runs before `main` so the variable is set before libtest spawns any
/// thread and before zenoh's lazy global runtimes read it. Spawned zenohd
/// child processes inherit it, which is harmless. An operator-provided
/// `ZENOH_RUNTIME` wins. Remove once the upstream fix ships in a release.
///
/// One of the two scoped unsafe opt-outs in this module (see lib.rs): setting
/// a process environment variable pre-main has no safe equivalent.
#[allow(unsafe_code)]
#[ctor::ctor(unsafe)]
fn ensure_zenoh_net_runtime_workers() {
    if std::env::var_os("ZENOH_RUNTIME").is_none() {
        // SAFETY: runs pre-main on the only live thread, so no other thread
        // can concurrently read or write the process environment.
        unsafe { std::env::set_var("ZENOH_RUNTIME", "(net: (worker_threads: 4))") };
    }
}

/// Raises the process soft `nofile` limit once per test binary. Test suites
/// spawn ephemeral zenoh routers and per-mock sessions, and running them in
/// parallel can exhaust file descriptors under the macOS default soft limit of
/// 256, surfacing as flaky `Too many open files` (EMFILE) errors. Bumping the
/// soft limit toward the hard limit removes that ceiling without reducing test
/// parallelism. Best effort: a failed syscall leaves the original limit in
/// place and the real EMFILE error still surfaces.
///
/// Called by [`EphemeralRouter::start`]; public so binaries that open many
/// sessions without a router can opt in directly.
///
/// One of the two scoped unsafe opt-outs in this module (see lib.rs): the
/// get/setrlimit FFI has no safe equivalent.
#[allow(unsafe_code)]
pub fn ensure_test_fd_limit() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // 8192 is comfortably above the peak concurrent router count and well
        // under the macOS per-process cap (kern.maxfilesperproc).
        const DESIRED_SOFT: libc::rlim_t = 8192;
        // SAFETY: get/setrlimit operate on a stack-allocated rlimit and report
        // failure through their return code, which we honor.
        unsafe {
            let mut limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
                return;
            }
            let target = DESIRED_SOFT.min(limit.rlim_max);
            if limit.rlim_cur >= target {
                return;
            }
            limit.rlim_cur = target;
            let _ = libc::setrlimit(libc::RLIMIT_NOFILE, &limit);
        }
    });
}

/// Serializes the zenoh router/peer meshes within one test binary. Running
/// several independent peer meshes at once starves peer-mode gossip discovery
/// (every peer opens listeners and forms links), which makes cold-start
/// delivery flaky; one mesh at a time keeps discovery fast and deterministic.
///
/// KNOWN FLAKE: zenoh (1.9.0 through at least 1.10.1) can deadlock its
/// routing layer under peer-session churn (see
/// [`ensure_zenoh_net_runtime_workers`], which suppresses the trigger). If it
/// ever fires anyway, the running test hangs forever and every later test
/// queues on this mutex, so the whole binary looks stuck ("test has been
/// running for over 60 seconds" for several tests at once). It cannot be
/// contained in-process: session teardown needs the deadlocked locks. Kill
/// the run and retry; do not debug peppy for it.
static ZENOH_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Acquire the per-process mesh-serialization guard directly, for tests that
/// build their own mesh without an [`EphemeralRouter`]. Held for the guard's
/// lifetime; [`EphemeralRouter::start`] acquires it internally.
pub async fn acquire_mesh_serial() -> tokio::sync::MutexGuard<'static, ()> {
    ZENOH_SERIAL.lock().await
}

/// A process-unique test instance id (`test-<pid>-<counter>`): the default
/// identity for a harness-booted node when no explicit id is supplied.
/// Generated harnesses call this rather than each carrying their own counter,
/// so ids from different nodes in one process can never collide.
pub fn unique_test_instance_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "test-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    )
}

/// An external zenohd process on an ephemeral port, wrapped for tests: started
/// with a real zenoh-open readiness probe (no sleeps), reaped on drop.
///
/// [`start`](Self::start) also holds the process-wide mesh-serialization guard
/// for the router's lifetime, so tests that each own a router run one mesh at
/// a time. That guard is not reentrant: a test that needs two routers at once
/// must use [`start_unserialized`](Self::start_unserialized) for the second
/// (or both) and accept the discovery-contention flake risk.
pub struct EphemeralRouter {
    instance: ZenohdInstance,
    _serial: Option<tokio::sync::MutexGuard<'static, ()>>,
}

impl EphemeralRouter {
    /// Starts a router on `127.0.0.1` with an ephemeral port, serialized
    /// against every other router in this process.
    pub async fn start() -> Result<Self> {
        Self::start_on("127.0.0.1", None).await
    }

    /// [`start`](Self::start) with an explicit host and (optionally) port.
    pub async fn start_on(host: &str, port: Option<u16>) -> Result<Self> {
        let serial = acquire_mesh_serial().await;
        ensure_test_fd_limit();
        let instance = ZenohAdapter::start_router_ephemeral(host, port).await?;
        Ok(Self {
            instance,
            _serial: Some(serial),
        })
    }

    /// [`start`](Self::start) without acquiring the mesh-serialization guard.
    /// For the rare multi-router test; everything else should serialize.
    pub async fn start_unserialized(host: &str, port: Option<u16>) -> Result<Self> {
        ensure_test_fd_limit();
        let instance = ZenohAdapter::start_router_ephemeral(host, port).await?;
        Ok(Self {
            instance,
            _serial: None,
        })
    }

    pub fn host(&self) -> &str {
        &self.instance.host
    }

    pub fn port(&self) -> u16 {
        self.instance.port
    }

    pub fn connection_target(&self) -> (String, u16) {
        (self.instance.host.clone(), self.instance.port)
    }

    /// Opens a fresh gossip-peer session against this router, retrying the
    /// first-connect races a just-ready router can still lose.
    pub async fn connect(&self) -> Result<MessengerHandle> {
        connect_messenger(self.host(), self.port()).await
    }

    /// Stops the router explicitly. Dropping the value has the same effect;
    /// this form surfaces the shutdown error instead of logging it.
    pub async fn shutdown(mut self) -> Result<()> {
        self.instance.messenger().stop_router().await?;
        Ok(())
    }
}

/// Opens a gossip-peer session against `host:port`, retrying the first-connect
/// races a just-ready router can still lose. The free-function form exists for
/// tasks that captured a router's `(host, port)` pair rather than the router
/// itself; [`EphemeralRouter::connect`] delegates here.
pub async fn connect_messenger(host: &str, port: u16) -> Result<MessengerHandle> {
    let mut last_error = None;
    for attempt in 0..CONNECT_RETRIES {
        match MessengerHandle::connect(host, port).await {
            Ok(handle) => return Ok(handle),
            Err(error) => {
                last_error = Some(error);
                if attempt + 1 < CONNECT_RETRIES {
                    tokio::time::sleep(CONNECT_RETRY_DELAY).await;
                }
            }
        }
    }
    Err(last_error.expect("CONNECT_RETRIES is non-zero"))
}

/// A publisher whose **first** publish deterministically waits until the
/// publishing session sees a matching subscriber, then publishes. In gossip
/// mode a freshly-connected publisher learns about existing subscribers
/// asynchronously, so an unguarded first publish can be dropped before routing
/// propagates; the wait is keyed by this publisher's exact wire identity
/// (link_id segment included), so it matches precisely the subscription the
/// consumer under test opened. Subsequent publishes skip the wait.
///
/// No subscriber within [`READINESS_TIMEOUT`] is an error, not a silent drop:
/// it means the node under test never subscribed where the test publishes,
/// which is a wiring bug the test must surface.
pub struct TestTopicPublisher {
    publisher: TopicPublisher,
    messenger: MessengerHandle,
    as_core_node: String,
    as_instance_id: String,
    as_target: SenderTarget,
    link_id: Option<String>,
    /// The peer a pairing publisher addresses; `None` on every other target.
    peer: Option<crate::messaging::PeerInfo>,
    topic: String,
    matched: AtomicBool,
    readiness_timeout: Duration,
}

impl TestTopicPublisher {
    /// Declares the underlying publisher now (so the identity is visible to
    /// the mesh immediately) without waiting for any subscriber yet.
    #[allow(clippy::too_many_arguments)]
    pub async fn declare(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        as_target: SenderTarget,
        link_id: Option<&str>,
        topic: &str,
        qos: QoSProfile,
    ) -> Result<Self> {
        let publisher = TopicMessenger::declare_publisher(
            messenger,
            as_core_node,
            as_instance_id,
            as_target.clone(),
            link_id,
            topic,
            qos,
        )
        .await?;
        Ok(Self {
            publisher,
            messenger: messenger.clone(),
            as_core_node: as_core_node.to_string(),
            as_instance_id: as_instance_id.to_string(),
            as_target,
            link_id: link_id.map(str::to_string),
            peer: None,
            topic: topic.to_string(),
            matched: AtomicBool::new(false),
            readiness_timeout: READINESS_TIMEOUT,
        })
    }

    /// Declares a pairing publisher from the mock's slot `link_id` to `peer`
    /// (the node under test, on its slot), for a mock playing the other end
    /// of a pair.
    #[allow(clippy::too_many_arguments)]
    pub async fn declare_to_peer(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        as_target: SenderTarget,
        link_id: &str,
        topic: &str,
        qos: QoSProfile,
        peer: crate::messaging::PeerInfo,
    ) -> Result<Self> {
        let publisher = TopicMessenger::declare_pairing_publisher(
            messenger,
            as_core_node,
            as_instance_id,
            as_target.clone(),
            link_id,
            topic,
            qos,
            &peer,
        )
        .await?;
        Ok(Self {
            publisher,
            messenger: messenger.clone(),
            as_core_node: as_core_node.to_string(),
            as_instance_id: as_instance_id.to_string(),
            as_target,
            link_id: Some(link_id.to_string()),
            peer: Some(peer),
            topic: topic.to_string(),
            matched: AtomicBool::new(false),
            readiness_timeout: READINESS_TIMEOUT,
        })
    }

    /// Override how long the first publish may wait for a subscriber before
    /// failing (default [`READINESS_TIMEOUT`]).
    pub fn with_readiness_timeout(mut self, timeout: Duration) -> Self {
        self.readiness_timeout = timeout;
        self
    }

    /// Waits until a subscriber matching this publisher's exact wire identity
    /// is visible, or `timeout` elapses; returns whether one matched. Marks
    /// the publisher matched on success so later publishes skip the wait.
    pub async fn wait_for_subscriber(&self, timeout: Duration) -> Result<bool> {
        let matched = match (&self.peer, self.link_id.as_deref()) {
            (Some(peer), Some(link_id)) => {
                TopicMessenger::wait_for_pairing_subscriber(
                    &self.messenger,
                    &self.as_core_node,
                    &self.as_instance_id,
                    self.as_target.clone(),
                    link_id,
                    &self.topic,
                    peer,
                    timeout,
                )
                .await?
            }
            _ => {
                TopicMessenger::wait_for_subscriber_with_link_id(
                    &self.messenger,
                    &self.as_core_node,
                    &self.as_instance_id,
                    self.as_target.clone(),
                    self.link_id.as_deref(),
                    &self.topic,
                    timeout,
                )
                .await?
            }
        };
        if matched {
            self.matched.store(true, Ordering::SeqCst);
        }
        Ok(matched)
    }

    /// Publishes `payload`, waiting for a matching subscriber first if none
    /// has been observed yet.
    pub async fn publish(&self, payload: Payload) -> Result<()> {
        if !self.matched.load(Ordering::SeqCst)
            && !self.wait_for_subscriber(self.readiness_timeout).await?
        {
            return Err(Error::Io(std::io::Error::other(format!(
                "no subscriber for topic `{}` (link_id {:?}) appeared within {:?}: \
                 the node under test never opened a matching subscription — check that the link \
                 is seeded in the harness config and that the node subscribes to this topic",
                self.topic, self.link_id, self.readiness_timeout,
            ))));
        }
        self.publisher.publish(payload).await
    }
}

/// A statically pinned peer-topic subscription: the exact wire shape a
/// paired peer's subscription has, the producer's triple and the mock's own
/// slot `as_link_id` both pinned, without the pin-following machinery, which
/// a mock does not need (its pin never changes). This is how a generated
/// pairing mock receives the topics the node under test emits on its slot.
#[allow(clippy::too_many_arguments)]
pub async fn subscribe_peer_pinned(
    messenger: &MessengerHandle,
    as_core_node: &str,
    as_instance_id: &str,
    as_link_id: &str,
    pairing_target: SenderTarget,
    peer: &crate::messaging::ProducerRef,
    peer_link_id: &str,
    to_topic: &str,
    qos: QoSProfile,
) -> Result<crate::messaging::Subscription> {
    TopicMessenger::subscribe_peer_pinned(
        messenger,
        as_core_node,
        as_instance_id,
        pairing_target,
        peer,
        peer_link_id,
        pmi::PairingRecipient::Slot(
            pmi::Segment::try_link_id(as_link_id)
                .map_err(|e| Error::PeppyMessagingInterface(e.into()))?,
        ),
        to_topic,
        qos,
    )
    .await
}

/// Interval between reachability probes in the wait helpers below.
const REACHABILITY_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Waits until the pinned producer's `service_name` answers reachability
/// probes, bounded by `timeout`. The cold-start counterpart of the topic-side
/// subscriber wait: a fresh session's first `poll` can race gossip discovery
/// of an already-declared queryable, and unlike topics a service query that
/// misses is a hard `ServiceUnreachable`, so callers gate on this first.
pub async fn wait_service_reachable(
    messenger: &MessengerHandle,
    bound_core_node: &str,
    as_instance_id: &str,
    to_target: SenderTarget,
    to_service_name: &str,
    producer: &crate::messaging::ProducerRef,
    timeout: Duration,
) -> Result<()> {
    wait_reachable(
        ServiceReadinessKind::Service,
        messenger,
        bound_core_node,
        as_instance_id,
        to_target,
        to_service_name,
        producer,
        timeout,
    )
    .await
}

/// [`wait_service_reachable`] for an action's goal service.
pub async fn wait_action_reachable(
    messenger: &MessengerHandle,
    bound_core_node: &str,
    as_instance_id: &str,
    to_target: SenderTarget,
    to_action_name: &str,
    producer: &crate::messaging::ProducerRef,
    timeout: Duration,
) -> Result<()> {
    wait_reachable(
        ServiceReadinessKind::Action,
        messenger,
        bound_core_node,
        as_instance_id,
        to_target,
        to_action_name,
        producer,
        timeout,
    )
    .await
}

/// The shared probe loop behind the two `wait_*_reachable` helpers: poll the
/// matching `is_reachable` probe until it answers or `timeout` expires.
#[allow(clippy::too_many_arguments)]
async fn wait_reachable(
    kind: ServiceReadinessKind,
    messenger: &MessengerHandle,
    bound_core_node: &str,
    as_instance_id: &str,
    to_target: SenderTarget,
    service_name: &str,
    producer: &crate::messaging::ProducerRef,
    timeout: Duration,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let reachable = match kind {
            ServiceReadinessKind::Service => {
                ServiceMessenger::is_reachable(
                    messenger,
                    bound_core_node,
                    as_instance_id,
                    to_target.clone(),
                    service_name,
                    crate::messaging::ServiceTarget::Producer(producer),
                )
                .await?
            }
            ServiceReadinessKind::Action => {
                crate::messaging::ActionMessenger::is_reachable(
                    messenger,
                    bound_core_node,
                    as_instance_id,
                    to_target.clone(),
                    service_name,
                    Some(producer),
                )
                .await?
            }
        };
        if reachable {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(Error::ServiceUnreachable {
                instance_id: Some(producer.instance_id.clone()),
                service_name: service_name.to_string(),
            });
        }
        tokio::time::sleep(REACHABILITY_POLL_INTERVAL).await;
    }
}

/// One service request captured by a [`MockServiceCore`], kept for later
/// assertions regardless of whether the response was scripted or manual.
#[derive(Clone)]
pub struct CapturedServiceRequest {
    /// Caller identity and request payload.
    pub message: Message,
    /// Producer-side link_id that received the request.
    pub link_id: String,
}

/// Server side of one mocked service: a background pump owns the endpoint and
/// captures every inbound request; a request is answered from the scripted
/// response queue when one is enqueued, and handed to
/// [`next_request`](Self::next_request) otherwise. Requests that neither path
/// consumed are reported loudly at drop, so a node call the test never
/// noticed cannot pass silently.
pub struct MockServiceCore {
    service_name: String,
    requests: flume::Receiver<(ServiceRequestContext, ServiceResponder)>,
    scripted: flume::Sender<Payload>,
    scripted_pending: flume::Receiver<Payload>,
    captured: Arc<StdMutex<Vec<CapturedServiceRequest>>>,
    pump: TaskHandle<()>,
}

impl MockServiceCore {
    /// Exposes the service under the given identity and starts the pump.
    pub async fn listen(
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        as_identity: SenderTarget,
        as_service_name: &str,
    ) -> Result<Self> {
        let mut endpoint: ServiceEndpoint = ServiceMessenger::listen(
            messenger,
            as_core_node,
            as_instance_id,
            as_identity,
            as_service_name,
        )
        .await?;

        let (requests_tx, requests_rx) = flume::unbounded();
        let (scripted_tx, scripted_rx) = flume::unbounded::<Payload>();
        let captured = Arc::new(StdMutex::new(Vec::new()));
        let captured_for_pump = Arc::clone(&captured);
        let scripted_for_pump = scripted_rx.clone();
        let service_name = as_service_name.to_string();
        let service_name_for_pump = service_name.clone();

        let pump = spawn(async move {
            loop {
                // Spelled out rather than `while let Ok(Some(..))`: that form
                // swallows a broken receive stream as an ordinary end of
                // input, and the mock then goes quiet in a way the test can
                // only observe as a timeout.
                let (context, responder) = match endpoint.recv_next_request().await {
                    Ok(Some(pair)) => pair,
                    Ok(None) => break,
                    Err(error) => {
                        warn!(
                            service = %service_name_for_pump,
                            %error,
                            "mock service request pump stopped"
                        );
                        break;
                    }
                };
                captured_for_pump
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(CapturedServiceRequest {
                        message: context.message().clone(),
                        link_id: context.link_id().to_string(),
                    });
                // Scripted responses win over manual receives: a test that
                // enqueued N responses gets them served in order as requests
                // arrive, and only unscripted requests park for next_request.
                match scripted_for_pump.try_recv() {
                    Ok(response) => {
                        if let Err(error) = responder.respond(response).await {
                            warn!(
                                service = %service_name_for_pump,
                                %error,
                                "mock service failed to send a scripted response"
                            );
                        }
                    }
                    Err(_) => {
                        if requests_tx.send_async((context, responder)).await.is_err() {
                            // Core dropped; the endpoint goes with this task.
                            break;
                        }
                    }
                }
            }
        });

        Ok(Self {
            service_name,
            requests: requests_rx,
            scripted: scripted_tx,
            scripted_pending: scripted_rx,
            captured,
            pump,
        })
    }

    /// The next unscripted request, with the responder the test must use to
    /// answer it. Errors after `timeout` — a node that never called is a test
    /// failure surfaced here, not a hang.
    pub async fn next_request(
        &self,
        timeout: Duration,
    ) -> Result<(ServiceRequestContext, ServiceResponder)> {
        match tokio::time::timeout(timeout, self.requests.recv_async()).await {
            Ok(Ok(pair)) => Ok(pair),
            Ok(Err(_)) => Err(Error::ServiceRequestStreamClosed),
            Err(_) => Err(Error::Io(std::io::Error::other(format!(
                "mock service `{}` received no request within {timeout:?}",
                self.service_name,
            )))),
        }
    }

    /// Enqueue one response to be served automatically to the next inbound
    /// request (FIFO across repeated calls).
    pub fn enqueue_response(&self, response: Payload) {
        let _ = self.scripted.send(response);
    }

    /// Every request captured so far (scripted and manual alike), in arrival
    /// order.
    pub fn captured(&self) -> Vec<CapturedServiceRequest> {
        self.captured
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Drop for MockServiceCore {
    fn drop(&mut self) {
        self.pump.abort();
        let mut unconsumed = 0usize;
        while self.requests.try_recv().is_ok() {
            unconsumed += 1;
        }
        if unconsumed > 0 {
            warn!(
                service = %self.service_name,
                unconsumed,
                "mock service dropped with unconsumed requests: the node called this \
                 service and the test neither scripted a response nor received the request"
            );
        }
        let mut unserved = 0usize;
        while self.scripted_pending.try_recv().is_ok() {
            unserved += 1;
        }
        if unserved > 0 {
            warn!(
                service = %self.service_name,
                unserved,
                "mock service dropped with scripted responses never requested by the node"
            );
        }
    }
}

/// Server side of one mocked action, on the real [`ConcurrentAction`] engine:
/// the full goal lifecycle (admission ack, cancel routing, feedback stream,
/// result retention) behaves exactly as a production action server's.
///
/// [`stop`](Self::stop) is the deterministic producer-loss primitive: it
/// disarms every live goal's close-on-drop transition (so no clean
/// feedback-end sentinel races the loss signal) and drops the engine, whose
/// liveliness token going absent is what consumers observe as
/// [`Error::ActionFeedbackProducerGone`]. The caller must also drop the mock's
/// session for the loss to be complete; the generated veneer owns that
/// ordering.
pub struct MockActionServerCore {
    action_name: String,
    engine: ConcurrentAction,
    live: Arc<StdMutex<Vec<Weak<GoalContext>>>>,
}

impl MockActionServerCore {
    /// Exposes the action under the given identity and starts its engine.
    /// `has_feedback` must reflect whether the action declares a feedback
    /// topic, exactly as for [`ConcurrentAction::expose`].
    pub async fn expose(
        messenger: &MessengerHandle,
        bound_core_node: &str,
        as_instance_id: &str,
        as_identity: SenderTarget,
        as_action_name: &str,
        has_feedback: bool,
    ) -> Result<Self> {
        let engine = ConcurrentAction::expose(
            messenger,
            bound_core_node,
            as_instance_id,
            as_identity,
            as_action_name,
            has_feedback,
        )
        .await?;
        Ok(Self {
            action_name: as_action_name.to_string(),
            engine,
            live: Arc::new(StdMutex::new(Vec::new())),
        })
    }

    /// Parks until the node under test sends a goal, bounded by `timeout`.
    pub async fn next_goal(&mut self, timeout: Duration) -> Result<MockPendingGoal> {
        match tokio::time::timeout(timeout, self.engine.recv_next_goal()).await {
            Ok(Ok(Some(pending))) => Ok(MockPendingGoal {
                pending,
                live: Arc::clone(&self.live),
            }),
            Ok(Ok(None)) => Err(Error::ServiceRequestStreamClosed),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(Error::Io(std::io::Error::other(format!(
                "mock action `{}` received no goal within {timeout:?}",
                self.action_name,
            )))),
        }
    }

    /// Simulate this producer disappearing mid-goal, deterministically: every
    /// live goal context is disarmed (its eventual drop emits neither the
    /// `Abandoned` transition nor the feedback-end sentinel), then the engine
    /// drops, releasing the producer liveliness token consumers latch on.
    pub fn stop(self) {
        let live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        for weak in live.iter() {
            if let Some(context) = weak.upgrade() {
                context.disarm_close_on_drop();
            }
        }
        drop(live);
        // `self` drops here: the engine's routing loops stop and its
        // liveliness token is released — the consumer-visible loss signal.
    }
}

/// A goal received by a [`MockActionServerCore`], awaiting the test's
/// admission decision.
pub struct MockPendingGoal {
    pending: PendingGoal,
    live: Arc<StdMutex<Vec<Weak<GoalContext>>>>,
}

impl MockPendingGoal {
    pub fn goal_id(&self) -> &str {
        self.pending.goal_id()
    }

    /// The envelope-stripped goal request payload, ready to decode.
    pub fn request_bytes(&self) -> &[u8] {
        self.pending.request_bytes()
    }

    /// Accept the goal. The returned context drives feedback/completion; it is
    /// registered with the owning mock so [`MockActionServerCore::stop`] can
    /// disarm it for deterministic producer-loss.
    pub async fn accept(self, response: Payload) -> Result<Arc<GoalContext>> {
        let context = Arc::new(self.pending.accept(response).await?);
        let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        live.retain(|weak| weak.strong_count() > 0);
        live.push(Arc::downgrade(&context));
        Ok(context)
    }

    /// Reject the goal with an optional human-readable reason.
    pub async fn reject(self, reason: Option<&str>, response: Payload) -> Result<()> {
        self.pending.reject(reason, response).await
    }
}

/// The instance identity a [`MockClock`] serves under by default. Each test
/// owns its own mesh (one router per harness), so a constant cannot collide
/// across concurrently running harnesses.
pub const MOCK_CLOCK_INSTANCE_ID: &str = "standalone-clock";

/// Tick cadence of a wall-mode [`MockClock`]: the daemon's production default
/// (10 Hz), so `peppylib::clock::subscribe` observes the same drumbeat under
/// the harness as under a real stack.
pub const MOCK_CLOCK_TICK_INTERVAL: Duration = Duration::from_millis(100);

/// The simulated domain a harness offers, named once so a test reading a log
/// or a key sees the same word the harness config uses.
pub const HARNESS_CLOCK_DOMAIN: &str = "harness";

/// Which clock a harness boots its node on, the test-side spelling of a
/// deployment's `framework.clock` and of the publisher a domain declaration
/// names.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HarnessClock {
    /// The node reads its own machine's clock, as an instance whose
    /// deployment names no domain does.
    #[default]
    Wall,
    /// The node reads the harness's simulated domain, and the test supplies
    /// its time with [`MockClock::tick`].
    Consumer,
    /// The node publishes the harness's simulated domain. The test supplies no
    /// time; it reads what the node sends with `peppylib::clock::subscribe`.
    Publisher,
}

/// Wall time plus a test-scripted signed skew: how a test reproduces "the
/// daemon's clock disagrees with mine" without touching a host clock.
struct SkewedWallSource {
    offset_ns: Arc<AtomicI64>,
}

impl ClockSource for SkewedWallSource {
    fn now_ns(&self) -> std::result::Result<u64, ClockSourceError> {
        let now = WallClockSource.now_ns()?;
        Ok(now.saturating_add_signed(self.offset_ns.load(Ordering::Relaxed)))
    }
}

enum MockClockDriver {
    /// Publishes wall ticks on the core node's own `clock` key, as a daemon
    /// does.
    Wall { ticker: TaskHandle<()> },
    /// Stands in for a simulator: publishes the harness domain's stream, and
    /// only when the test says so. Boxed: the publisher carries its whole wire
    /// identity, which dwarfs the other variants.
    Domain { publisher: Box<TestTopicPublisher> },
    /// Publishes nothing, because the node under test publishes the domain.
    Silent,
}

/// The daemon's clock surface under the harness: a `clock` service queryable
/// answering `peppylib::clock::synchronize`'s NTP-style exchange (through the
/// same [`handle_clock_request`](crate::clock::handle_clock_request) the
/// daemon serves), plus the `clock` topic.
///
/// [`HarnessClock::Wall`] mirrors an instance reading its machine's clock:
/// ticks are published automatically at [`MOCK_CLOCK_TICK_INTERVAL`].
/// [`HarnessClock::Consumer`] mirrors one bound to a simulated domain, with
/// the test playing the simulator: nothing ticks until it calls
/// [`tick`](Self::tick). [`HarnessClock::Publisher`] mirrors the instance a
/// domain declaration names, and the harness publishes nothing at all.
///
/// The `clock` service answers from OS wall time in every mode, skewable via
/// [`set_offset_ns`](Self::set_offset_ns), because that is what a daemon
/// serves whatever its instances read.
pub struct MockClock {
    core_node: String,
    instance_id: String,
    pump: TaskHandle<()>,
    /// Skew applied to every timestamp the `clock` service answers with, in
    /// every mode: the service serves wall time whatever the node reads.
    offset_ns: Arc<AtomicI64>,
    clock: HarnessClock,
    driver: MockClockDriver,
}

impl MockClock {
    /// Serves the daemon's clock surface for `core_node` (under the harness:
    /// [`STANDALONE_CORE_NODE`]) in the shape `clock` asks for.
    ///
    /// The `clock` SERVICE always answers from OS wall time, skewable with
    /// [`set_offset_ns`](Self::set_offset_ns), because a daemon serves wall
    /// time whatever its instances read. What differs per mode is the tick
    /// stream: wall ticks on the core node's key, the harness domain's stream
    /// driven by [`tick`](Self::tick), or nothing at all when the node itself
    /// publishes the domain.
    pub async fn start(
        messenger: &MessengerHandle,
        core_node: &str,
        instance_id: &str,
        clock: HarnessClock,
    ) -> Result<Self> {
        let offset_ns = Arc::new(AtomicI64::new(0));
        let source: Arc<dyn ClockSource> = Arc::new(SkewedWallSource {
            offset_ns: Arc::clone(&offset_ns),
        });
        let pump = Self::listen(messenger, core_node, instance_id, Arc::clone(&source)).await?;
        let driver = match clock {
            HarnessClock::Wall => {
                let publisher = TopicMessenger::declare_publisher(
                    messenger,
                    core_node,
                    instance_id,
                    SenderTarget::node(core_node, names::CORE_NODE_TAG)?,
                    None,
                    TopicId::Clock.name(),
                    QoSProfile::SensorData,
                )
                .await?;
                // Same loop shape as the daemon's wall-mode publisher:
                // SensorData QoS (stale time is useless), skip catch-up
                // bursts, a failed read or publish skips the tick rather than
                // killing the stream. No cancel token: `Drop` aborts the task.
                let ticker = spawn(async move {
                    let mut interval = tokio::time::interval(MOCK_CLOCK_TICK_INTERVAL);
                    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    loop {
                        interval.tick().await;
                        let now_ns = match source.now_ns() {
                            Ok(t) => t,
                            Err(e) => {
                                warn!("mock clock tick skipped, {e}");
                                continue;
                            }
                        };
                        let payload = match ClockTick::new(now_ns).encode() {
                            Ok(p) => p,
                            Err(e) => {
                                warn!("mock clock tick encode failed: {e}");
                                continue;
                            }
                        };
                        if let Err(e) = publisher.publish(payload).await {
                            warn!("mock clock tick emit failed: {e}");
                        }
                    }
                });
                MockClockDriver::Wall { ticker }
            }
            HarnessClock::Consumer => {
                let domain = Self::domain_of(core_node)?;
                let link_id = domain.link_id();
                let publisher = TestTopicPublisher::declare(
                    messenger,
                    core_node,
                    instance_id,
                    SenderTarget::node(core_node, names::CORE_NODE_TAG)?,
                    Some(&link_id),
                    TopicId::Clock.name(),
                    QoSProfile::SensorData,
                )
                .await?;
                MockClockDriver::Domain {
                    publisher: Box::new(publisher),
                }
            }
            HarnessClock::Publisher => MockClockDriver::Silent,
        };
        Ok(Self {
            core_node: core_node.to_string(),
            instance_id: instance_id.to_string(),
            pump,
            offset_ns,
            clock,
            driver,
        })
    }

    /// The harness's simulated domain on `core_node`. One incarnation, since a
    /// harness boots one node once.
    fn domain_of(core_node: &str) -> Result<ClockDomainId> {
        Ok(ClockDomainId::new(
            config::runtime::Name::new(HARNESS_CLOCK_DOMAIN)
                .map_err(|e| Error::Io(std::io::Error::other(e.to_string())))?,
            config::runtime::CoreNodeName::new(core_node)
                .map_err(|e| Error::Io(std::io::Error::other(e.to_string())))?,
            ClockIncarnation::try_from(1).expect("one is not zero"),
        ))
    }

    /// The binding the node under test boots with, which the generated
    /// harness hands to `StandaloneConfig::with_clock`.
    pub fn binding(&self) -> Result<config::runtime::ClockBinding> {
        Ok(match self.clock {
            HarnessClock::Wall => config::runtime::ClockBinding::Wall,
            HarnessClock::Consumer => config::runtime::ClockBinding::consumer(
                Self::domain_of(&self.core_node)?,
                self.producer_ref(),
            ),
            HarnessClock::Publisher => {
                config::runtime::ClockBinding::publisher(Self::domain_of(&self.core_node)?)
            }
        })
    }

    async fn listen(
        messenger: &MessengerHandle,
        core_node: &str,
        instance_id: &str,
        source: Arc<dyn ClockSource>,
    ) -> Result<TaskHandle<()>> {
        let mut endpoint: ServiceEndpoint = ServiceMessenger::listen(
            messenger,
            core_node,
            instance_id,
            SenderTarget::node(core_node, names::CORE_NODE_TAG)?,
            ServiceId::Clock.name(),
        )
        .await?;
        Ok(spawn(async move {
            // Nothing joins this handle (`Drop` aborts it), so the only way
            // a broken receive stream can be seen is here: without the log a
            // dead queryable reaches the test as an unexplained `synchronize`
            // timeout.
            if let Err(error) = endpoint
                .handle_requests(move |context| {
                    let source = Arc::clone(&source);
                    async move { crate::clock::handle_clock_request(source.as_ref(), context) }
                })
                .await
            {
                warn!(%error, "mock clock service pump stopped");
            }
        }))
    }

    /// Advance the harness domain to `time_ns` and publish it, for a node
    /// booted [`HarnessClock::Consumer`].
    ///
    /// The first publish waits until the node's clock subscription is visible
    /// ([`TestTopicPublisher`] semantics): ticking a domain no node reads is a
    /// wiring bug surfaced as a loud error, not a silent drop. `0` is the
    /// wire's not-ready sentinel and is clamped to `1`.
    ///
    /// Errors in the other modes: a wall-mode clock ticks itself, and a node
    /// publishing its own domain is the only thing that may advance it.
    pub async fn tick(&self, time_ns: u64) -> Result<()> {
        let MockClockDriver::Domain { publisher } = &self.driver else {
            return Err(Error::Io(std::io::Error::other(format!(
                "tick() drives a node booted HarnessClock::Consumer; this harness booted \
                 {:?}. A wall-mode clock ticks itself, and a publisher node supplies its own \
                 domain",
                self.clock
            ))));
        };
        publisher
            .publish(ClockTick::new(time_ns.max(1)).encode()?)
            .await
    }

    /// Skew every timestamp the `clock` service answers with, by a signed
    /// offset from the OS clock: the scripted stand-in for a daemon host whose
    /// clock drifted from the node's, so offset handling is testable without
    /// touching a real clock. Also skews a wall-mode clock's published ticks,
    /// which come from the same source.
    pub fn set_offset_ns(&self, offset_ns: i64) {
        self.offset_ns.store(offset_ns, Ordering::Relaxed);
    }

    /// The wire identity the clock serves under, as a probe-able producer.
    pub fn producer_ref(&self) -> crate::messaging::ProducerRef {
        crate::messaging::ProducerRef::new(&self.core_node, &self.instance_id)
    }

    /// This clock's entry for the harness's pre-setup reachability barrier,
    /// so a `synchronize` in the node's `setup` cannot race gossip discovery
    /// of the queryable.
    pub fn readiness(&self) -> Result<ServiceReadiness> {
        Ok(ServiceReadiness {
            target: SenderTarget::node(&self.core_node, names::CORE_NODE_TAG)?,
            name: ServiceId::Clock.name().to_string(),
            producer: self.producer_ref(),
            kind: ServiceReadinessKind::Service,
        })
    }
}

impl Drop for MockClock {
    fn drop(&mut self) {
        self.pump.abort();
        if let MockClockDriver::Wall { ticker } = &self.driver {
            ticker.abort();
        }
    }
}

/// One entry of the harness's pre-setup readiness barrier: before the node's
/// `setup` runs, the harness waits — on the node's own session — until a
/// subscriber matching the exact keyexpr the node's `declare_publisher` will
/// emit on is visible. The harness/mocks declare their observation
/// subscriptions before the node exists, but in gossip mode the node's fresh
/// session still has to *discover* them; without this barrier the node's very
/// first publish can be dropped (subscribe-first alone is not sufficient).
pub struct PublisherReadiness {
    /// The identity the node publishes under for this topic.
    pub target: SenderTarget,
    /// The node's own producer-side link_id for slot-scoped publishers
    /// (pairing slots); `None` for plain emitted topics.
    pub link_id: Option<String>,
    /// The mock peer a pairing publisher addresses; `None` for plain emitted
    /// topics.
    pub peer: Option<crate::messaging::PeerInfo>,
    pub topic: String,
}

/// Whether a [`ServiceReadiness`] entry probes a plain service queryable or an
/// action's goal service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceReadinessKind {
    Service,
    Action,
}

/// One entry of the harness's pre-setup reachability barrier for mocked
/// services and actions: the node under test is a *fresh caller*, so its very
/// first `poll`/`send_goal` inside `setup` can race gossip discovery of a mock
/// queryable that was declared long before the node's session existed. The
/// harness therefore waits — on the node's own session — until each mock's
/// queryable answers reachability probes, before `setup` runs.
pub struct ServiceReadiness {
    /// The identity the mock serves under (the dependency's node/contract
    /// target).
    pub target: SenderTarget,
    /// Service or action name.
    pub name: String,
    /// The mock's wire identity, as seeded into the node's bound set.
    pub producer: crate::messaging::ProducerRef,
    pub kind: ServiceReadinessKind,
}

/// The node-invariant half of the generated test harness: builds the node
/// in-process from an already-seeded [`StandaloneConfig`], runs the pre-setup
/// readiness barrier, spawns the node's `setup`, and owns teardown
/// convergence. The generated veneer contributes what is per-node: mock
/// construction, config seeding, and typed observation clients.
///
/// Teardown contract ([`shutdown`](Self::shutdown)): cancel the node's token →
/// await `setup` bounded by the shutdown grace (propagating its error if it
/// returned one; a long-running `setup` that parks on the token's subscriptions
/// is aborted like production drops it) → run the registered shutdown hooks →
/// release the node session as the harness drops. Dropping without calling
/// `shutdown` still cancels and aborts, but skips the hook run — prefer the
/// explicit call.
pub struct HarnessCore {
    node_runner: Arc<NodeRunner>,
    cancellation_token: CancellationToken,
    setup_task: Option<TaskHandle<Result<()>>>,
}

impl HarnessCore {
    /// Builds and starts the node under test. `standalone_config` must already
    /// carry the messaging endpoint, instance id, parameters, and one seeding
    /// call per mock; `publisher_readiness` lists every topic the harness (or
    /// a pairing mock) subscribed to that the node will publish on.
    ///
    /// `setup` is the node's real entry point — the exact closure shape
    /// `NodeBuilder::run` takes — spawned only after the readiness barrier
    /// passes, so its first publish routes.
    pub async fn start<Params, F, Fut>(
        peppy_config_path: impl Into<std::path::PathBuf>,
        standalone_config: StandaloneConfig,
        publisher_readiness: &[PublisherReadiness],
        service_readiness: &[ServiceReadiness],
        setup: F,
    ) -> Result<Self>
    where
        Params: serde::de::DeserializeOwned + schemars::JsonSchema,
        F: FnOnce(Params, Arc<NodeRunner>) -> Fut,
        Fut: std::future::Future<Output = Result<()>> + Send + 'static,
    {
        let cancellation_token = CancellationToken::new();
        let mut context = NodeBuilder::<Params>::new()
            .with_config_path(peppy_config_path)
            .standalone(standalone_config)
            .init_standalone()?
            .with_cancellation_token(cancellation_token.clone());
        let params = context.take_parameters()?;
        let node_runner = context.create_node_runner().await?;

        // Pre-setup readiness barrier: a publisher-side matching wait per
        // observed topic, on the node's session, with the identical keyexpr
        // the node's declare_publisher will use.
        let processor = node_runner.processor();
        for probe in publisher_readiness {
            let matched = match (&probe.peer, probe.link_id.as_deref()) {
                (Some(peer), Some(link_id)) => {
                    TopicMessenger::wait_for_pairing_subscriber(
                        node_runner.messenger(),
                        processor.bound_core_node(),
                        processor.bound_instance_id(),
                        probe.target.clone(),
                        link_id,
                        &probe.topic,
                        peer,
                        READINESS_TIMEOUT,
                    )
                    .await?
                }
                _ => {
                    TopicMessenger::wait_for_subscriber_with_link_id(
                        node_runner.messenger(),
                        processor.bound_core_node(),
                        processor.bound_instance_id(),
                        probe.target.clone(),
                        probe.link_id.as_deref(),
                        &probe.topic,
                        READINESS_TIMEOUT,
                    )
                    .await?
                }
            };
            if !matched {
                return Err(Error::Io(std::io::Error::other(format!(
                    "readiness barrier: the harness subscription for topic `{}` (link_id {:?}) \
                     was not visible to the node's session within {READINESS_TIMEOUT:?}; \
                     the mesh never routed it — this is a harness/mock wiring bug, not a node bug",
                    probe.topic, probe.link_id,
                ))));
            }
        }

        // Reachability barrier for mocked services/actions: the node is a
        // fresh caller, so gate its session's discovery of each mock
        // queryable before setup's first poll/send_goal can race it.
        for probe in service_readiness {
            wait_reachable(
                probe.kind,
                node_runner.messenger(),
                processor.bound_core_node(),
                processor.bound_instance_id(),
                probe.target.clone(),
                &probe.name,
                &probe.producer,
                READINESS_TIMEOUT,
            )
            .await?;
        }

        // Setup ends with the same check production runs: every endpoint the
        // manifest declares was announced, and nothing can be announced after.
        let setup_future = setup(params, Arc::clone(&node_runner));
        let setup_task = spawn({
            let node_runner = Arc::clone(&node_runner);
            async move {
                setup_future.await?;
                node_runner.seal_endpoints().map(drop)
            }
        });

        Ok(Self {
            node_runner,
            cancellation_token,
            setup_task: Some(setup_task),
        })
    }

    /// The running node, for registering observation subscriptions or calling
    /// its runtime surface directly.
    pub fn node_runner(&self) -> &Arc<NodeRunner> {
        &self.node_runner
    }

    pub fn instance_id(&self) -> &str {
        self.node_runner.processor().bound_instance_id()
    }

    pub fn bound_core_node(&self) -> &str {
        self.node_runner.processor().bound_core_node()
    }

    /// Whether the spawned `setup` has already returned (many setups register
    /// their loops and return immediately; long-running ones never do until
    /// shutdown).
    pub fn setup_finished(&self) -> bool {
        self.setup_task.as_ref().is_none_or(TaskHandle::is_finished)
    }

    /// Converge the node: see the type-level teardown contract. Returns the
    /// `setup` error if it failed — a test whose node errored during setup
    /// should fail even when its assertions never noticed.
    pub async fn shutdown(mut self) -> Result<()> {
        self.cancellation_token.cancel();
        let grace = self.node_runner.processor().shutdown_grace();
        let setup_result = match self.setup_task.take() {
            Some(mut task) => match tokio::time::timeout(grace, &mut task).await {
                Ok(Ok(result)) => result,
                Ok(Err(join_error)) if join_error.is_cancelled() => Ok(()),
                Ok(Err(join_error)) => Err(Error::Io(std::io::Error::other(format!(
                    "the node's setup task panicked: {join_error}"
                )))),
                Err(_elapsed) => {
                    // A setup that parks forever without watching the node's
                    // cancellation token; production drops it at shutdown the
                    // same way.
                    task.abort();
                    Ok(())
                }
            },
            None => Ok(()),
        };
        self.node_runner.run_shutdown_hooks(grace).await;
        setup_result
    }
}

impl Drop for HarnessCore {
    fn drop(&mut self) {
        self.cancellation_token.cancel();
        if let Some(task) = self.setup_task.take()
            && !task.is_finished()
        {
            task.abort();
            warn!(
                "HarnessCore dropped without shutdown(): the node's shutdown hooks did \
                 not run; call `shutdown().await` for a clean teardown"
            );
        }
    }
}
