#[cfg(all(test, feature = "zenoh"))]
mod tests;

mod actions;
mod discovery;
mod services;
mod topics;

// `unwrap_result_outcome` is intentionally not re-exported: unlike its public
// siblings (`wrap_result_outcome` / `encode_cancel_ack`, which the integration
// tests use to assert the wire codec), it is only used inside `actions` and its
// unit tests, so it stays `pub(crate)`.
pub use actions::{
    ActionCreation, ActionFeedbackPublisher, ActionFeedbackPublisherFactory, ActionGoalHandle,
    ActionMessenger, ActionResultReply, CancelState, ConcurrentAction, DeclaredFeedback,
    EmptyPayloadError, GoalContext, NonEmptyPayload, PendingGoal, ResultStatus, decode_cancel_ack,
    encode_cancel_ack, generate_goal_id, unwrap_goal_payload, wrap_goal_payload,
    wrap_result_outcome,
};
pub use services::{
    ServiceEndpoint, ServiceMessenger, ServiceRequestContext, ServiceResponder, ServiceTarget,
};
pub use topics::{Subscription, TopicMessenger, TopicPublisher};

mod filter;
pub use filter::{ConsumerFilter, ProducerRef};
// Used only by the processor at startup to pre-resolve per-link_id filters.
pub(crate) use filter::resolve_consumer_filter;

// Curated pmi re-exports. peppylib is a thin layer over PMI, so these types are
// the shared vocabulary of its public messaging API rather than hidden
// implementation details (every consumer also depends on pmi directly).
// `SenderTarget` / `SenderTargetError` appear in nearly every messaging
// signature and are emitted by the code generator. `InterfaceIdentifier` /
// `NodeIdentifier` / `ActionWireSender` / `ActionLivelinessToken` are surfaced
// for the Python bindings, which cache an `ActionWireSender` to drive
// cancel / result calls without re-locking and name `ActionLivelinessToken` as
// the type of the public `ActionCreation::liveliness_token` field. The other
// wire structs (TopicWire*, ServiceWire*, ActionWireReceiver) are internal to
// peppylib's own messaging implementation; each submodule imports them directly
// from `pmi::`.
pub use pmi::{
    ActionLivelinessToken, ActionWireSender, InterfaceIdentifier, NodeIdentifier, SenderTarget,
    SenderTargetError,
};

use crate::error::{Error, Result};
use crate::types::{Message, Payload};
use config::node::QoSProfile;
use pmi::{
    ActionLivelinessWatch, ActionWireReceiver, Messenger, MessengerAdapter, MessengerBackend,
    MessengerPublisher, PublisherQoS, ServiceQueryKind, ServiceReplyKind, ServiceWireReceiver,
    ServiceWireSender, SubscriberQoS, Subscription as PmiSubscription, TopicWireReceiver,
    TopicWireSender, ZenohAdapter, ZenohNetProtocol,
};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::sync::{
    Arc, Mutex as StdMutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::{
    sync::Mutex,
    time::{Duration, Instant, sleep, timeout},
};

// services
pub const NODE_HEALTH_SERVICE: &str = "node_health";
pub const NODE_READY_SERVICE: &str = "node_ready";
pub const SHUTDOWN_SERVICE: &str = "shutdown";
/// Framework service every node exposes: on request, the node performs a clock
/// exchange against the core node and reports its measured offset. Used by
/// `peppy stack benchmark` to normalize cross-host producer timestamps. Like
/// `node_health`, this triggers no user code.
pub const CLOCK_OFFSET_SERVICE: &str = "clock_offset";

/// Timeout for a single reachability probe sent by `is_reachable`.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// Budget for the wildcard / from_any discover-then-pin step (capped by the
/// caller's own timeout). Larger than [`PROBE_TIMEOUT`] because a
/// freshly-connected peer learns producers' queryables via gossip, which is not
/// instantaneous like the old client/router star — `discover_producer` re-probes
/// within this budget so a from_any `poll`/`send_goal` waits for discovery to
/// settle instead of failing the moment it runs ahead of it.
pub(crate) const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(2);

/// Key in [`MessengerHandle::active_from_any_topics`]. Two from_any topic
/// subscriptions conflict only when they would observe the same producer
/// publishes — i.e. they share the producer-side wire pin (the full
/// `(core_node, instance_id)` pair, or `None` for a wildcard) and
/// `(producer_name, producer_tag)`. Two subscriptions on the same
/// `(name, tag)` but pinned to different producers target disjoint
/// producers, do not share dedupe scope, and must be allowed to coexist.
type ActiveFromAnyKey = (Option<filter::ProducerRef>, String, String);

#[derive(Clone)]
pub struct MessengerHandle {
    messenger: Arc<Mutex<Messenger>>,
    /// Live from_any topic subscriptions per `(producer_name, producer_tag)`.
    /// [`topics::TopicMessenger::subscribe`] reserves a key here on the
    /// from_any path; [`FromAnyTopicGuard`] releases it on drop. The manifest
    /// validator already rejects duplicate from_any consumers, but this
    /// guards against bypasses via direct messenger calls.
    active_from_any_topics: Arc<StdMutex<HashSet<ActiveFromAnyKey>>>,
}

/// RAII reservation in [`MessengerHandle::active_from_any_topics`]. Held by
/// a [`topics::Subscription`] for its full lifetime; the slot is released
/// when the subscription is dropped, freeing the `(name, tag)` for a future
/// from_any subscription.
pub(crate) struct FromAnyTopicGuard {
    key: ActiveFromAnyKey,
    set: Arc<StdMutex<HashSet<ActiveFromAnyKey>>>,
}

impl Drop for FromAnyTopicGuard {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.set.lock() {
            guard.remove(&self.key);
        }
    }
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

impl MessengerHandle {
    /// Build a handle from an already-shared `pmi::Messenger`. This is the
    /// escape hatch for consumers that construct and own the messenger
    /// themselves (peppy and core-node-internal both do); the `from_host_port*`
    /// constructors are the normal path for opening a fresh session.
    pub fn from_shared(messenger: Arc<Mutex<Messenger>>) -> Self {
        Self {
            messenger,
            active_from_any_topics: Arc::new(StdMutex::new(HashSet::new())),
        }
    }

    /// Wrap a freshly-opened `Messenger` in the shared-handle state. Shared by
    /// the `from_host_port*` constructors so the handle's field initialization
    /// lives in one place.
    fn from_messenger(messenger: Messenger) -> Self {
        Self {
            messenger: Arc::new(Mutex::new(messenger)),
            active_from_any_topics: Arc::new(StdMutex::new(HashSet::new())),
        }
    }

    /// Reserve a `(from_producer, producer_name, producer_tag)` slot in
    /// the active from_any topic set. Returns a guard that releases the
    /// slot on drop, or [`Error::DuplicateFromAnyConsumer`] if a from_any
    /// topic subscription matching the same producer-side pin is already
    /// live on this messenger. The pin is part of the key because two
    /// from_any subs scoped to different producers do not share dedupe
    /// scope and must coexist; the failure mode the guard prevents is two
    /// from_any subs observing the *same* producer's emits, which is
    /// exactly when their `(name, tag)` exclusion sets and
    /// primary/secondary filtering need to give one (and only one)
    /// delivery per emit.
    pub(crate) fn reserve_from_any_topic(
        &self,
        from_producer: Option<&filter::ProducerRef>,
        name: &str,
        tag: &str,
    ) -> Result<FromAnyTopicGuard> {
        let key: ActiveFromAnyKey = (from_producer.cloned(), name.to_string(), tag.to_string());
        let mut guard = self
            .active_from_any_topics
            .lock()
            .expect("active_from_any_topics mutex poisoned");
        if !guard.insert(key.clone()) {
            return Err(Error::DuplicateFromAnyConsumer {
                name: name.to_string(),
                tag: tag.to_string(),
            });
        }
        Ok(FromAnyTopicGuard {
            key,
            set: Arc::clone(&self.active_from_any_topics),
        })
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

    pub async fn from_host_port(host: &str, port: u16) -> Result<Self> {
        let adapter = ZenohAdapter::connect_to(ZenohNetProtocol::Tcp, host, port)?;
        let messenger = Self::new_session(adapter).await?;
        Ok(Self::from_messenger(messenger))
    }

    /// Like [`from_host_port`](Self::from_host_port) but opens a *reconnecting*
    /// session: if the router is restarted under it (e.g. the daemon's router
    /// watchdog respawning zenohd), the session re-establishes and re-declares
    /// its subscriptions/queryables instead of going dead.
    ///
    /// Used by long-lived node processes. Short-lived / CLI connections keep
    /// [`from_host_port`](Self::from_host_port) so a dead daemon fails fast
    /// rather than blocking on connection retries.
    pub async fn from_host_port_reconnecting(host: &str, port: u16) -> Result<Self> {
        let adapter =
            ZenohAdapter::connect_to(ZenohNetProtocol::Tcp, host, port)?.with_session_reconnect();
        let messenger = Self::new_session(adapter).await?;
        Ok(Self::from_messenger(messenger))
    }

    /// Like [`from_host_port_reconnecting`](Self::from_host_port_reconnecting)
    /// but applies the node's [`DiscoveryConfig`](config::runtime::DiscoveryConfig):
    /// an explicit gossip seed list (falling back to `host:port`) and the gossip
    /// toggle. Used by the node runtime so peers form direct links per the
    /// daemon-supplied discovery settings.
    pub async fn from_host_port_reconnecting_with_discovery(
        host: &str,
        port: u16,
        discovery: &config::runtime::DiscoveryConfig,
    ) -> Result<Self> {
        let buffer_sizes = pmi::SubscriberBufferSizes::from(discovery);
        let adapter = ZenohAdapter::connect_to_with_discovery(
            ZenohNetProtocol::Tcp,
            host,
            port,
            discovery.seed_peers.clone(),
            discovery.gossip,
            buffer_sizes,
        )?
        .with_session_reconnect();
        let messenger = Self::new_session(adapter).await?;
        Ok(Self::from_messenger(messenger))
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

    /// Waits (deterministically, via Zenoh matching status) until a subscriber
    /// for `sender`'s topic is known to this session, or `timeout` elapses;
    /// returns whether a match was observed. A freshly-connected peer learns
    /// remote subscriptions through gossip, which is not instantaneous, so its
    /// first reliable publish can be dropped before discovery propagates; awaiting
    /// a match closes that window without a fixed sleep. The mock backend has no
    /// propagation delay and returns `true` immediately.
    pub(crate) async fn wait_for_matching_subscriber(
        &self,
        sender: &TopicWireSender,
        timeout: Duration,
    ) -> Result<bool> {
        let messenger = self.messenger.lock().await;
        match &messenger.adapter {
            #[cfg(feature = "zenoh")]
            MessengerAdapter::Zenoh(adapter) => adapter
                .wait_for_topic_subscriber(sender, timeout)
                .await
                .map_err(Error::PeppyMessagingInterface),
            _ => Ok(true),
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
        // reply (`Ok(None)`) the instant it runs ahead of discovery. When that
        // happens *before any Ack* — the target was never reached — re-issue the
        // query within the caller's remaining budget so the call deterministically
        // waits for discovery to settle instead of failing immediately. Once an
        // Ack arrives the target is reachable, so a later miss is a genuine
        // `ServiceTimeout`, never a retry. This adds no happy-path overhead:
        // retries only fire on an actual cold-start miss.
        let reply = 'attempts: loop {
            if let Some(deadline) = deadline
                && Instant::now() >= deadline
            {
                return Err(unreachable());
            }

            let attempt_timeout = deadline.map(|d| d.saturating_duration_since(Instant::now()));
            let mut response_subscription = {
                let messenger = self.messenger.lock().await;
                messenger
                    .call_service(sender, request_bytes.clone().into(), kind, attempt_timeout)
                    .await
                    .map_err(Error::PeppyMessagingInterface)?
            };

            let mut received_ack = false;
            loop {
                let received = match deadline {
                    Some(deadline) => {
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            return Err(if received_ack {
                                timed_out()
                            } else {
                                unreachable()
                            });
                        }
                        match timeout(remaining, response_subscription.rx.recv()).await {
                            Ok(maybe) => maybe,
                            // Deadline elapsed with the query still open: a real
                            // timeout (slow/absent target), not a cold-start miss.
                            Err(_) => {
                                return Err(if received_ack {
                                    timed_out()
                                } else {
                                    unreachable()
                                });
                            }
                        }
                    }
                    None => response_subscription.rx.recv().await,
                };

                match received {
                    Some(reply) => match reply.kind() {
                        ServiceReplyKind::Ack => {
                            received_ack = true;
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
                        // retry, so fail fast (matching the pre-retry behavior)
                        // rather than re-probing forever.
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
    ) -> Result<ActionLivelinessToken> {
        let messenger = self.messenger.lock().await;
        messenger
            .declare_action_liveliness(recv)
            .await
            .map_err(Error::PeppyMessagingInterface)
    }

    pub(crate) async fn watch_action_producer(
        &self,
        sender: &ActionWireSender,
    ) -> Result<ActionLivelinessWatch> {
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
}
