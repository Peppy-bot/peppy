#[cfg(test)]
mod deadline_tests;
#[cfg(all(test, feature = "zenoh"))]
mod tests;

mod actions;
mod bound_set;
mod discovery;
mod observation;
mod pairing;
mod presence;
mod services;
mod topics;

// `unwrap_result_outcome` / `unwrap_goal_ack` are intentionally not
// re-exported: unlike their public siblings (`wrap_result_outcome` /
// `wrap_goal_ack` / `encode_cancel_ack`, which the integration tests use to
// assert the wire codec), they are only used inside `actions` and its unit
// tests, so they stay `pub(crate)`.
pub use actions::{
    ActionCreation, ActionFeedbackPublisher, ActionFeedbackPublisherFactory, ActionGoalHandle,
    ActionGoalReply, ActionMessenger, ActionResultReply, CancelState, ConcurrentAction,
    DeclaredFeedback, EmptyPayloadError, GoalContext, NonEmptyPayload, PendingGoal, ResultStatus,
    decode_cancel_ack, encode_cancel_ack, generate_goal_id, unwrap_goal_payload, wrap_goal_ack,
    wrap_goal_payload, wrap_result_outcome,
};
pub use bound_set::NonEmptyProducers;
pub use observation::{
    NonEmptyObservedSources, ObservationState, ObservedMemberState, ObservedSource,
};
pub use pairing::{PeerInfo, PeerMember, PeerSetState};
pub use presence::CoreNodePresenceMessenger;
pub use services::{
    ServiceEndpoint, ServiceMessenger, ServiceRequestContext, ServiceResponder, ServiceTarget,
};
pub use topics::{BoundSetSubscription, Subscription, TopicMessenger, TopicPublisher};
// The fan-in rule every multi-source consumer merges through, shared with the
// pinned-slot engine in `crate::runtime::slot_stream`.
pub(crate) use topics::recv_first_ready;

// Fully-qualified producer address, re-exported from the config model: the
// wire addresses a producer by the `(core_node, instance_id)` pair. Every
// consumer dep slot is bound to an ordered producer set sized by its
// declared cardinality (the launcher validator resolves and stamps each
// member at plan time). Wire operations stay per-producer: a bound-set
// topic subscription opens one pinned subscription per member, and poll /
// send_goal take the single selected member — there is no per-slot filter
// type and no in-process producer filtering.
pub use config::runtime::ProducerRef;

// Curated pmi re-exports. peppylib is a thin layer over PMI, so these types are
// the shared vocabulary of its public messaging API rather than hidden
// implementation details (every consumer also depends on pmi directly).
// `SenderTarget` / `SenderTargetError` appear in nearly every messaging
// signature and are emitted by the code generator. `ContractIdentifier` /
// `NodeIdentifier` / `ActionWireSender` / `LivelinessToken` are surfaced
// for the Python bindings, which cache an `ActionWireSender` to drive
// cancel / result calls without re-locking and name `LivelinessToken` as
// the type of the public `ActionCreation::liveliness_token` field. The other
// wire structs (TopicWire*, ServiceWire*, ActionWireReceiver) are internal to
// peppylib's own messaging implementation; each submodule imports them directly
// from `pmi::`.
pub use pmi::{
    ActionWireSender, ContractIdentifier, CoreNodePresence, LivelinessEvent, LivelinessToken,
    LivelinessWatch, NodeIdentifier, PairingIdentifier, SenderTarget, SenderTargetError,
};

use crate::error::{Error, Result};
use crate::types::{Message, Payload};
use config::node::QoSProfile;
use pmi::{
    ActionWireReceiver, Messenger, MessengerAdapter, MessengerBackend, MessengerPublisher,
    Namespace, PresenceScope, PublisherQoS, Segment, ServiceQueryKind, ServiceReplyKind,
    ServiceWireReceiver, ServiceWireSender, SubscriberQoS, Subscription as PmiSubscription,
    TopicWireReceiver, TopicWireSender, ZenohAdapter, ZenohNetProtocol,
};
use sha2::{Digest, Sha256};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::{
    sync::Mutex,
    time::{Duration, Instant, sleep, timeout_at},
};

// services
pub const NODE_HEALTH_SERVICE: &str = "node_health";
pub const NODE_READY_SERVICE: &str = "node_ready";
/// Framework service a node exposes when its manifest declares
/// `execution.endpoints`: the daemon reads the sockets the node bound during
/// setup, one per declared label, and turns them into the URLs it reports.
/// Registered post-setup next to `node_health`, once the set is sealed.
pub const NODE_ENDPOINTS_SERVICE: &str = "node_endpoints";
pub const SHUTDOWN_SERVICE: &str = "shutdown";
/// Framework service every node exposes: the daemon delivers absolute
/// pairing-slot state (pair, re-pin, clear) over it. Registered pre-setup —
/// a node may block in its `setup_fn` forever, and pairing delivery must not
/// depend on user code. Triggers no user code.
pub const PEER_UPDATE_SERVICE: &str = "peer_update";

/// Framework service every node exposes: the daemon delivers absolute
/// observer-slot state (source pin, source generation, source liveness) over it.
/// Registered pre-setup for the same reason as [`PEER_UPDATE_SERVICE`], and it
/// triggers no user code.
pub const OBSERVATION_UPDATE_SERVICE: &str = "observation_update";

/// Timeout for a single reachability probe sent by `is_reachable`.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// Budget for the wildcard discover-then-pin step of core-node infra calls
/// (capped by the caller's own timeout). Larger than [`PROBE_TIMEOUT`]
/// because a freshly-connected peer learns producers' queryables via gossip,
/// which is not instantaneous like the old client/router star —
/// `discover_producer` re-probes within this budget so a discovery-scoped
/// `poll` waits for discovery to settle instead of failing the moment it
/// runs ahead of it.
pub(crate) const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub struct MessengerHandle {
    messenger: Arc<Mutex<Messenger>>,
}

/// 16 hex chars (64 bits) of correlation entropy, salted with `domain` so
/// IDs from different namespaces (request, goal, ...) cannot collide on a
/// timestamp + thread_id tie. A process-wide counter is folded in to keep
/// IDs unique even when two calls land on the same thread within a single
/// clock tick.
pub(crate) fn generate_short_id(domain: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let thread_id = std::thread::current().id();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);

    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    hasher.update(timestamp.to_le_bytes());
    hasher.update(format!("{thread_id:?}").as_bytes());
    hasher.update(counter.to_le_bytes());
    let result = hasher.finalize();

    use std::fmt::Write;
    let mut hex = String::with_capacity(16);
    for b in result.iter().take(8) {
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

/// Transport for a [`MessengerConnect`]: plaintext TCP (`tcp/`, the default) or
/// TLS (`tls/`), the latter validated against a [`pmi::TlsConfig`].
enum Transport {
    Tcp,
    Tls(pmi::TlsConfig),
}

/// Discovery parameters pulled from a node's
/// [`DiscoveryConfig`](config::runtime::DiscoveryConfig) by
/// [`SessionScope::Discovery`]: the gossip seed list, the gossip toggle, and
/// subscriber buffer sizing. The workspace namespace travels on
/// [`MessengerConnect`] itself, not here.
struct DiscoveryParams {
    seed_peers: Vec<String>,
    gossip: bool,
    buffer_sizes: pmi::SubscriberBufferSizes,
}

/// How a [`MessengerConnect`] session attaches to the messaging fabric at
/// `host:port`. Selected by [`MessengerConnect::scope`]; the default is
/// [`SessionKind::GossipPeer`].
enum SessionKind {
    /// Gossip peer: binds a wildcard listener and forms direct links with
    /// peers discovered via gossip seeded by `host:port`.
    GossipPeer,
    /// Pure client: no listener and no peer discovery; every message relays
    /// through the router at `host:port`.
    Client,
    /// Peer or client per a node's
    /// [`DiscoveryConfig`](config::runtime::DiscoveryConfig) gossip toggle,
    /// with its seed list and subscriber buffer sizing applied.
    Discovery(DiscoveryParams),
}

/// How a [`MessengerConnect`] session resolves its workspace namespace, and
/// which [`SessionKind`] it opens. Passed to [`MessengerConnect::scope`]. The
/// two cases are mutually exclusive *by construction*: a session is either
/// pinned to an explicit namespace or driven by a node's discovery config,
/// never both.
pub enum SessionScope<'a> {
    /// Open a pure client session under an explicit workspace namespace
    /// instead of the `local` default: no listener, no gossip
    /// discovery, every request relayed through the router it dialed. Used by
    /// a CLI control session so it reaches the daemon/node services under the
    /// same namespace the daemon runs, and so a one-shot (non-reconnecting)
    /// connect fails promptly when that router is down instead of timing out
    /// per request.
    Namespace(Namespace),
    /// Apply the node's [`DiscoveryConfig`](config::runtime::DiscoveryConfig): an
    /// explicit gossip seed list (falling back to `host:port`), the gossip
    /// toggle, subscriber buffer sizing, and the workspace namespace stamped
    /// by the daemon in `cfg.namespace`. Used by the node
    /// runtime so peers form direct links per the daemon-supplied discovery
    /// settings and stay routing-isolated under the daemon's namespace.
    Discovery(&'a config::runtime::DiscoveryConfig),
}

/// Fluent builder for opening a [`MessengerHandle`] session, returned by
/// [`MessengerHandle::connect`]. Await it directly — it implements
/// [`IntoFuture`](std::future::IntoFuture) — to perform the connect. Every
/// builder method is infallible; the only failure point is the awaited connect,
/// which yields `Result<MessengerHandle>`.
pub struct MessengerConnect {
    host: String,
    port: u16,
    reconnect: bool,
    namespace: Namespace,
    transport: Transport,
    kind: SessionKind,
}

impl MessengerConnect {
    /// Opt into a *reconnecting* session: if the router is restarted under it
    /// (e.g. the daemon's watchdog respawning zenohd), the session re-establishes
    /// and re-declares its subscriptions/queryables instead of going dead. Used
    /// by long-lived node processes; short-lived / CLI connections leave it off
    /// so a dead daemon fails fast rather than blocking on connection retries.
    pub fn reconnecting(mut self) -> Self {
        self.reconnect = true;
        self
    }

    /// Dial the router over TLS (`tls/`), validating its certificate against the
    /// trust configured in `tls`, instead of the plaintext `tcp/` default. Used
    /// by an out-of-band prober that connects to a router's `tls/` listener.
    pub fn tls(mut self, tls: pmi::TlsConfig) -> Self {
        self.transport = Transport::Tls(tls);
        self
    }

    /// Set how the session resolves its workspace namespace, and which
    /// [`SessionKind`] it opens. The two cases, an explicit
    /// [`Namespace`](SessionScope::Namespace) or a node's
    /// [`DiscoveryConfig`](SessionScope::Discovery), are mutually exclusive by
    /// construction, so a session can never request both at once. Left unset, the
    /// session opens under the `local` default namespace as a gossip peer.
    pub fn scope(mut self, scope: SessionScope<'_>) -> Self {
        match scope {
            SessionScope::Namespace(ns) => {
                // A namespace-scoped control session relays every request
                // through the router it dialed, so it opens as a pure client:
                // no listener, no discovery. Overwriting the kind also drops
                // any params a prior `scope(Discovery(..))` call may have set,
                // keeping the two modes mutually exclusive across chained
                // calls.
                self.namespace = ns;
                self.kind = SessionKind::Client;
            }
            SessionScope::Discovery(cfg) => {
                // A stamped namespace is already validated by runtime-config
                // parsing. Absence is legitimate standalone operation and
                // resolves to `local`.
                self.namespace = cfg.namespace.clone().unwrap_or_else(Namespace::local);
                self.kind = SessionKind::Discovery(DiscoveryParams {
                    seed_peers: cfg.seed_peers.clone(),
                    gossip: cfg.gossip,
                    buffer_sizes: pmi::SubscriberBufferSizes::from(cfg),
                });
            }
        }
        self
    }
}

impl std::future::IntoFuture for MessengerConnect {
    type Output = Result<MessengerHandle>;
    type IntoFuture =
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<MessengerHandle>> + Send>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let adapter = match (self.transport, self.kind) {
                (Transport::Tcp, SessionKind::GossipPeer) => {
                    ZenohAdapter::connect_to(ZenohNetProtocol::Tcp, &self.host, self.port)?
                }
                // Empty seeds fall back to `host:port`; `gossip = false`
                // selects client mode, so no listener is bound.
                (Transport::Tcp, SessionKind::Client) => ZenohAdapter::connect_to_with_discovery(
                    ZenohNetProtocol::Tcp,
                    &self.host,
                    self.port,
                    Vec::new(),
                    false,
                    pmi::SubscriberBufferSizes::default(),
                    None,
                )?,
                // TLS sessions are always client mode, whichever kind the
                // builder holds.
                (Transport::Tls(tls), SessionKind::GossipPeer | SessionKind::Client) => {
                    ZenohAdapter::connect_to_tls(&self.host, self.port, tls)?
                }
                (Transport::Tcp, SessionKind::Discovery(d)) => {
                    ZenohAdapter::connect_to_with_discovery(
                        ZenohNetProtocol::Tcp,
                        &self.host,
                        self.port,
                        d.seed_peers,
                        d.gossip,
                        d.buffer_sizes,
                        None,
                    )?
                }
                (Transport::Tls(_), SessionKind::Discovery(_)) => {
                    return Err(Error::ConfigurationError(
                        "TLS transport with discovery is not supported".into(),
                    ));
                }
            };
            let adapter = if self.reconnect {
                adapter.with_session_reconnect()
            } else {
                adapter
            };
            let adapter = adapter.with_namespace(Some(self.namespace));
            let messenger = MessengerHandle::new_session(adapter).await?;
            Ok(MessengerHandle::from_shared(Arc::new(Mutex::new(
                messenger,
            ))))
        })
    }
}

/// Waits for a service await until the caller's absolute deadline.
///
/// The deadline bounds the wait, never the outcome: a future that is ready
/// when polled completes even when the caller's task resumes after the
/// deadline, so a reply the producer already sent is never discarded for the
/// caller's own scheduling latency. `None` when the deadline passes with the
/// future still pending, which drops it.
async fn within_service_deadline<F: std::future::Future>(
    deadline: Option<Instant>,
    future: F,
) -> Option<F::Output> {
    match deadline {
        Some(deadline) => timeout_at(deadline, future).await.ok(),
        None => Some(future.await),
    }
}

/// Whether the caller's deadline has passed, so no query may be issued for it
/// any more.
fn service_deadline_passed(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|deadline| Instant::now() >= deadline)
}

impl MessengerHandle {
    /// Build a handle from an already-shared `pmi::Messenger`. This is the
    /// escape hatch for consumers that construct and own the messenger
    /// themselves (peppy and core-node-internal both do); the [`connect`](Self::connect)
    /// builder is the normal path for opening a fresh session.
    pub fn from_shared(messenger: Arc<Mutex<Messenger>>) -> Self {
        Self { messenger }
    }

    /// Pre-bind a per-topic publisher. Locks the messenger once at
    /// declaration to extract the per-adapter handle, then never again — the
    /// returned [`pmi::MessengerPublisher`] holds its own state (an
    /// `Arc<zenoh::Session>` clone or `Arc<Mutex<HashMap>>` clones for the
    /// mock) and `publish` skips the central messenger mutex.
    pub(crate) async fn declare_topic_publisher(
        &self,
        sender: &TopicWireSender,
        qos: PublisherQoS,
    ) -> Result<MessengerPublisher> {
        let messenger = self.messenger.lock().await;
        messenger
            .declare_topic_publisher(sender, qos)
            .map_err(Error::PeppyMessagingInterface)
    }

    /// Pre-bind a per-goal action-feedback publisher under the link_id the
    /// consumer targeted in the goal request.
    pub(crate) async fn declare_action_feedback_publisher(
        &self,
        recv: &ActionWireReceiver,
        link_id: &str,
        goal_id: &str,
        qos: PublisherQoS,
    ) -> Result<MessengerPublisher> {
        let messenger = self.messenger.lock().await;
        messenger
            .declare_action_feedback_publisher(recv, link_id, goal_id, qos)
            .map_err(Error::PeppyMessagingInterface)
    }

    pub async fn messaging_port(&self) -> u16 {
        let messenger = self.messenger.lock().await;
        messenger.get_host().port()
    }

    pub async fn messaging_endpoint(&self) -> Option<(String, u16)> {
        let messenger = self.messenger.lock().await;
        match &messenger.adapter {
            #[cfg(feature = "zenoh")]
            MessengerAdapter::Zenoh(adapter) => {
                let (host, port) = adapter.client_endpoint();
                (!host.is_empty() && port != 0).then(|| (host.to_string(), port))
            }
            _ => None,
        }
    }

    /// One entry point for opening a session to a router at `host:port`.
    ///
    /// Returns a [`MessengerConnect`] builder, awaited directly to perform the
    /// connect. Defaults: plaintext TCP peer (gossip) mode, one-shot (no
    /// reconnect), and the [`Namespace::local()`] namespace. Opt into the
    /// other axes fluently:
    ///
    /// ```ignore
    /// MessengerHandle::connect(host, port)
    ///     .reconnecting()                          // re-establishing session for long-lived nodes
    ///     .scope(SessionScope::Namespace(ns))      // client-mode control session under an explicit namespace
    ///     .scope(SessionScope::Discovery(&cfg))    // gossip/seed peers + namespace from a node's DiscoveryConfig
    ///     .tls(cfg)                                // TLS client transport
    ///     .await                                   // -> Result<MessengerHandle>
    /// ```
    pub fn connect(host: &str, port: u16) -> MessengerConnect {
        MessengerConnect {
            host: host.to_string(),
            port,
            reconnect: false,
            namespace: Namespace::local(),
            transport: Transport::Tcp,
            kind: SessionKind::GossipPeer,
        }
    }

    async fn new_session(adapter: ZenohAdapter) -> Result<Messenger> {
        let mut messenger = Messenger::new(MessengerAdapter::Zenoh(adapter));
        messenger
            .start_session()
            .await
            .map_err(Error::PeppyMessagingInterface)?;

        Ok(messenger)
    }

    async fn subscribe_to_topic(
        &self,
        recv: &TopicWireReceiver,
        qos: QoSProfile,
    ) -> Result<PmiSubscription> {
        let messenger = self.messenger.lock().await;
        messenger
            .subscribe_topic(recv, qos.into())
            .await
            .map_err(Error::PeppyMessagingInterface)
    }

    /// Waits (deterministically, via the backend's matching status) until a
    /// subscriber for `sender`'s topic is known to this session, or `timeout`
    /// elapses; returns whether a match was observed. A freshly-connected peer
    /// learns remote subscriptions through gossip, which is not instantaneous,
    /// so its first reliable publish can be dropped before discovery
    /// propagates; awaiting a match closes that window without a fixed sleep.
    /// Both backends wait outside the central messenger lock (mirroring
    /// [`probe_action_producer`](Self::probe_action_producer)): a wait bounded
    /// only by `timeout` held under it would starve every other messenger
    /// call — including, in mock tests that share the handle between
    /// publisher and subscriber, the very subscribe being awaited.
    pub(crate) async fn wait_for_matching_subscriber(
        &self,
        sender: &TopicWireSender,
        timeout: Duration,
    ) -> Result<bool> {
        let subscription_map = {
            let messenger = self.messenger.lock().await;
            match &messenger.adapter {
                #[cfg(feature = "zenoh")]
                MessengerAdapter::Zenoh(adapter) => {
                    let wait = adapter
                        .topic_subscriber_wait(sender)
                        .map_err(Error::PeppyMessagingInterface)?;
                    drop(messenger);
                    return wait
                        .resolve(timeout)
                        .await
                        .map_err(Error::PeppyMessagingInterface);
                }
                MessengerAdapter::Mock(adapter) => adapter.subscription_map(),
            }
        };
        let deadline = Instant::now() + timeout;
        loop {
            if pmi::MockAdapter::topic_has_matching_subscriber(&subscription_map, sender) {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            sleep(Duration::from_millis(5)).await;
        }
    }

    pub(crate) async fn expose_service(
        &self,
        recv: &ServiceWireReceiver,
    ) -> Result<ServiceEndpoint> {
        let queryable = {
            let messenger = self.messenger.lock().await;
            messenger
                .listen_service(recv)
                .await
                .map_err(Error::PeppyMessagingInterface)?
        };
        Ok(ServiceEndpoint::new(Arc::clone(&self.messenger), queryable))
    }

    pub(crate) async fn poll_service(
        &self,
        sender: &ServiceWireSender,
        request_payload: Payload,
        kind: ServiceQueryKind,
        response_timeout: impl Into<Option<Duration>>,
    ) -> Result<Message> {
        let response_timeout: Option<Duration> = response_timeout.into();

        let to_service_name = sender.to_service_name().to_string();
        let target_instance_id = sender.target_instance_id().map(str::to_string);
        let unreachable = || Error::ServiceUnreachable {
            instance_id: target_instance_id.clone(),
            service_name: to_service_name.clone(),
        };
        let timed_out = || Error::ServiceTimeout {
            instance_id: target_instance_id.clone(),
            service_name: to_service_name.clone(),
        };

        // Re-issue the query (cheap `Bytes` clone per attempt) on a cold-start
        // miss; see the retry rationale below.
        let request_bytes = request_payload.into_inner();
        const COLD_START_BACKOFF: Duration = Duration::from_millis(50);
        let deadline = response_timeout.map(|t| Instant::now() + t);

        // Each attempt issues the query and waits for its first terminal reply.
        // The producer sends `Ack` immediately on receiving a real user request
        // (before the handler runs) and a terminal `Response` / `HandlerError`
        // once the handler returns. Probes get a single `Response` (no Ack).
        //
        // Cold-start retry (peer mode): a freshly-connected caller may not have
        // learned the target's queryable yet, so the query finalizes with no
        // reply (a closed stream) the instant it runs ahead of discovery. When that
        // happens before any Ack, the target was never reached: re-issue the
        // query within the caller's remaining budget so the call deterministically
        // waits for discovery to settle instead of failing immediately. Once an
        // Ack arrives the target is reachable, so a later miss is a genuine
        // `ServiceTimeout`, never a retry. This adds no happy-path overhead:
        // retries only fire on an actual cold-start miss.
        let reply = 'attempts: loop {
            let mut response_subscription = {
                let messenger = within_service_deadline(deadline, self.messenger.lock())
                    .await
                    .ok_or_else(unreachable)?;
                // A query is issued only while the caller still waits for it. A
                // lock acquired past the deadline (a contended lock, a retry
                // backoff that ran late) would otherwise put a request on the
                // wire that the producer executes while this caller reports it
                // as unreachable.
                if service_deadline_passed(deadline) {
                    return Err(unreachable());
                }
                let attempt_timeout = deadline.map(|d| d.saturating_duration_since(Instant::now()));
                within_service_deadline(
                    deadline,
                    messenger.call_service(
                        sender,
                        request_bytes.clone().into(),
                        kind,
                        attempt_timeout,
                    ),
                )
                .await
                .ok_or_else(unreachable)?
                .map_err(Error::PeppyMessagingInterface)?
            };

            let mut received_ack = false;
            loop {
                let received = within_service_deadline(deadline, response_subscription.rx.recv())
                    .await
                    .ok_or_else(|| {
                        if received_ack {
                            timed_out()
                        } else {
                            unreachable()
                        }
                    })?;

                match received {
                    Some(reply) => match reply.kind() {
                        ServiceReplyKind::Ack => {
                            received_ack = true;
                            tracing::trace!(
                                service_name = %to_service_name,
                                ?target_instance_id,
                                "service request acknowledged"
                            );
                            continue;
                        }
                        ServiceReplyKind::Response | ServiceReplyKind::HandlerError => {
                            break 'attempts reply;
                        }
                    },
                    // Query finalized with no reply.
                    None => {
                        if received_ack {
                            return Err(timed_out());
                        }
                        // Cold-start retry only makes sense against a bounded
                        // budget. With no timeout there is nothing to bound the
                        // retry, so fail fast rather than re-probing forever.
                        let Some(deadline) = deadline else {
                            return Err(unreachable());
                        };
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            return Err(unreachable());
                        }
                        sleep(COLD_START_BACKOFF.min(remaining)).await;
                        continue 'attempts;
                    }
                }
            }
        };

        let kind = reply.kind();
        let message = Message::from(reply.into_message());
        match kind {
            ServiceReplyKind::HandlerError => {
                let reason = match std::str::from_utf8(message.payload_bytes().as_ref()) {
                    Ok(s) => s.to_string(),
                    Err(_) => "service returned a non-UTF8 error payload".to_string(),
                };
                Err(Error::ServiceError {
                    instance_id: target_instance_id,
                    service_name: to_service_name,
                    reason,
                })
            }
            ServiceReplyKind::Response => Ok(message),
            ServiceReplyKind::Ack => unreachable!("ACK replies are skipped above"),
        }
    }

    pub(crate) async fn expose_action(&self, recv: &ActionWireReceiver) -> Result<ActionCreation> {
        let goal_service = self.expose_service(&recv.goal_service()).await?;
        let cancel_service = self.expose_service(&recv.cancel_service()).await?;
        let result_service = self.expose_service(&recv.result_service()).await?;

        // Advertise this producer instance's liveliness for the lifetime of
        // the action endpoint. The transport removes the token when the
        // producing session dies — gracefully or by hard process death — so
        // consumers can detect a producer that vanished without closing its
        // goals (see `ActionGoalHandle::on_next_feedback`).
        let liveliness_token = self.declare_action_liveliness(recv).await?;

        // Per-goal feedback uses `Important` (Block on congestion, DataHigh
        // priority) rather than `Standard`. The publisher is declared inside
        // the goal handler — the moment a fast server's first feedback
        // publish fires, the local routing tables may not yet have the client's
        // subscription propagated through the router. Empirically, `Standard`
        // (Drop, Data) loses the first publish in tight in-process tests;
        // `Important` is delivered reliably. The block-on-congestion semantic
        // is also the right call for action feedback: it's preferable to
        // backpressure a fast emitter than to silently drop progress updates.
        let feedback_publisher_factory = actions::ActionFeedbackPublisherFactory::new(
            self.clone(),
            recv.clone(),
            PublisherQoS::Important,
        );

        Ok(ActionCreation {
            goal_service,
            cancel_service,
            feedback_publisher_factory,
            result_service,
            liveliness_token,
        })
    }

    pub(crate) async fn subscribe_action_feedback(
        &self,
        sender: &ActionWireSender,
        goal_id: &str,
        qos: SubscriberQoS,
    ) -> Result<PmiSubscription> {
        let messenger = self.messenger.lock().await;
        messenger
            .subscribe_action_feedback(sender, goal_id, qos)
            .await
            .map_err(Error::PeppyMessagingInterface)
    }

    pub(crate) async fn declare_action_liveliness(
        &self,
        recv: &ActionWireReceiver,
    ) -> Result<LivelinessToken> {
        let messenger = self.messenger.lock().await;
        messenger
            .declare_action_liveliness(recv)
            .await
            .map_err(Error::PeppyMessagingInterface)
    }

    pub(crate) async fn watch_action_producer(
        &self,
        sender: &ActionWireSender,
    ) -> Result<LivelinessWatch> {
        let messenger = self.messenger.lock().await;
        messenger
            .watch_action_producer(sender)
            .await
            .map_err(Error::PeppyMessagingInterface)
    }

    /// One-shot probe of the targeted producer's liveliness token. The
    /// central messenger lock is held only for query issuance; the wait for
    /// the answer (bounded by `timeout`) happens after it is released.
    pub(crate) async fn probe_action_producer(
        &self,
        sender: &ActionWireSender,
        timeout: Duration,
    ) -> Result<bool> {
        let probe = {
            let messenger = self.messenger.lock().await;
            messenger
                .probe_action_producer(sender, timeout)
                .await
                .map_err(Error::PeppyMessagingInterface)?
        };
        Ok(probe.resolve().await)
    }

    pub(crate) async fn declare_core_node_presence(
        &self,
        core_node: &Segment,
        instance_id: &Segment,
    ) -> Result<LivelinessToken> {
        let messenger = self.messenger.lock().await;
        messenger
            .declare_core_node_presence(core_node, instance_id)
            .await
            .map_err(Error::PeppyMessagingInterface)
    }

    pub(crate) async fn watch_core_node_presence(
        &self,
        core_node: Option<&Segment>,
    ) -> Result<LivelinessWatch<CoreNodePresence>> {
        let messenger = self.messenger.lock().await;
        messenger
            .watch_core_node_presence(core_node)
            .await
            .map_err(Error::PeppyMessagingInterface)
    }

    /// Enumerates live core-node daemon tokens. The central messenger lock
    /// is held only for query issuance; the wait for the replies (bounded by
    /// `timeout`) happens after it is released, mirroring
    /// [`probe_action_producer`](Self::probe_action_producer).
    pub(crate) async fn list_core_node_presence(
        &self,
        core_node: Option<&Segment>,
        timeout: Duration,
    ) -> Result<Vec<CoreNodePresence>> {
        let list = {
            let messenger = self.messenger.lock().await;
            messenger
                .list_core_node_presence(PresenceScope::Session, core_node, timeout)
                .await
                .map_err(Error::PeppyMessagingInterface)?
        };
        list.collect().await.map_err(Error::PeppyMessagingInterface)
    }
}
