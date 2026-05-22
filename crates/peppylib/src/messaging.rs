#[cfg(all(test, feature = "zenoh"))]
mod tests;

mod actions;
mod discovery;
mod services;
mod topics;

pub use actions::{
    ActionCreation, ActionFeedbackPublisher, ActionFeedbackPublisherFactory, ActionGoalHandle,
    ActionMessenger, DeclaredFeedback, EmptyPayloadError, NonEmptyPayload, generate_goal_id,
    unwrap_goal_payload, wrap_goal_payload,
};
pub use services::{ServiceEndpoint, ServiceMessenger, ServiceRequestContext, ServiceResponder};
pub use topics::{Subscription, TopicMessenger, TopicPublisher};

// Public re-exports. `SenderTarget` / `InterfaceIdentifier` / `NodeIdentifier`
// / `SenderTargetError` / `ServiceKind` describe the shape of messaging calls
// and surface in user-facing peppylib APIs. `ActionWireSender` is exposed
// because peppylib-py caches one to drive subsequent cancel / result calls
// without locking. The other wire structs (TopicWire*, ServiceWire*,
// ActionWireReceiver) are internal to peppylib's own messaging
// implementation; each submodule imports them directly from `pmi::`.
pub use pmi::{
    ActionWireSender, InterfaceIdentifier, NodeIdentifier, SenderTarget, SenderTargetError,
    ServiceKind,
};

use crate::error::{Error, Result};
use crate::types::{Message, Payload};
use config::node::QoSProfile;
use pmi::{
    ActionWireReceiver, Messenger, MessengerAdapter, MessengerBackend, MessengerPublisher,
    PublisherQoS, ServiceQueryKind, ServiceReplyKind, ServiceWireReceiver, ServiceWireSender,
    SubscriberQoS, Subscription as PmiSubscription, TopicWireReceiver, TopicWireSender,
    ZenohAdapter, ZenohNetProtocol,
};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::{
    Arc, Mutex as StdMutex, RwLock,
    atomic::{AtomicU64, Ordering},
};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::{
    sync::Mutex,
    time::{Duration, Instant, timeout},
};

// services
pub const NODE_HEALTH_SERVICE: &str = "node_health";
pub const NODE_READY_SERVICE: &str = "node_ready";
pub const SHUTDOWN_SERVICE: &str = "shutdown";

/// Timeout for reachability probes sent by `is_reachable`.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// Per-`(producer_name, producer_tag)` set of link_ids that this consumer
/// process has pinned via `depends_on`. Populated once at node bootstrap by
/// codegen via [`MessengerHandle::register_consumer_dependencies`]; consulted
/// at subscribe / poll / send_goal time when the caller's `from_link_id` is
/// `None` (a `from_any: true` consumer) to derive the producer link_ids the
/// from_any path must skip. Empty / absent groups apply no exclusion.
type PinnedSiblingMap = HashMap<(String, String), Vec<String>>;

/// Key in [`MessengerHandle::active_from_any_topics`]. Two from_any topic
/// subscriptions conflict only when they would observe the same producer
/// publishes — i.e. they share the producer-side filter
/// `(from_core_node, from_instance_id, producer_name, producer_tag)`. Two
/// subscriptions on the same `(name, tag)` but filtered to different
/// producer instances target disjoint producers, do not share dedupe
/// scope, and must be allowed to coexist.
type ActiveFromAnyKey = (Option<String>, Option<String>, String, String);

#[derive(Clone)]
pub struct MessengerHandle {
    messenger: Arc<Mutex<Messenger>>,
    pinned_siblings: Arc<RwLock<PinnedSiblingMap>>,
    /// Live from_any topic subscriptions per `(producer_name, producer_tag)`.
    /// The sibling-exclusion filter in [`topics::Subscription`] can only
    /// dedupe correctly when at most one from_any subscription exists per
    /// `(name, tag)` on this messenger; the manifest validator enforces this
    /// at config time, but [`Self::register_consumer_dependencies`] and
    /// direct calls to [`topics::TopicMessenger::subscribe`] can bypass it.
    /// [`topics::TopicMessenger::subscribe`] reserves a key here on the
    /// from_any path; [`FromAnyTopicGuard`] releases it on drop.
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
    pub fn from_shared(messenger: Arc<Mutex<Messenger>>) -> Self {
        Self {
            messenger,
            pinned_siblings: Arc::new(RwLock::new(HashMap::new())),
            active_from_any_topics: Arc::new(StdMutex::new(HashSet::new())),
        }
    }

    /// Seed the per-`(producer_name, producer_tag)` map of pinned sibling
    /// link_ids. Codegen calls this once at node bootstrap with the entries
    /// it discovered in `depends_on.{nodes,interfaces}`: for each `(name,
    /// tag)` group that contains at least one pinned dependency, the value
    /// is the list of pinned link_ids on that group. Groups with only one
    /// entry (whether pinned or from_any) can be omitted; the exclusion
    /// only matters when a from_any sibling needs to skip pinned link_ids.
    ///
    /// Replaces any previous registration in full; multiple calls are
    /// supported but only the latest map is observed.
    pub fn register_consumer_dependencies(&self, pinned_per_group: PinnedSiblingMap) {
        if let Ok(mut guard) = self.pinned_siblings.write() {
            *guard = pinned_per_group;
        }
    }

    /// Look up the pinned-sibling link_ids registered for a given
    /// `(producer_name, producer_tag)`. Returns an empty vec when nothing
    /// is registered or the (name, tag) pair isn't present. Callers
    /// (TopicMessenger::subscribe, ServiceMessenger::poll,
    /// ActionMessenger::send_goal) consult this when their `from_link_id`
    /// argument is `None` to derive the excluded set for the wire receiver
    /// / sender.
    pub(crate) fn excluded_link_ids_for(&self, name: &str, tag: &str) -> Vec<String> {
        let Ok(guard) = self.pinned_siblings.read() else {
            return Vec::new();
        };
        guard
            .get(&(name.to_string(), tag.to_string()))
            .cloned()
            .unwrap_or_default()
    }

    /// Returns the sibling-pinned exclusion set when this caller is a
    /// wildcard consumer of `target` (i.e. `link_id` is `None`). Returns
    /// an empty vec for pinned callers and for topic subscriptions with no
    /// addressable target. Centralizes the "only consult the map when
    /// wildcard" gate so the three messaging entry points stay one-line.
    pub(crate) fn excluded_link_ids_for_wildcard(
        &self,
        target: Option<&SenderTarget>,
        link_id: Option<&str>,
    ) -> Vec<String> {
        let (Some(target), None) = (target, link_id) else {
            return Vec::new();
        };
        self.excluded_link_ids_for(target.name(), target.tag())
    }

    /// Reserve a `(from_core_node, from_instance_id, producer_name,
    /// producer_tag)` slot in the active from_any topic set. Returns a
    /// guard that releases the slot on drop, or
    /// [`Error::DuplicateFromAnyConsumer`] if a from_any topic subscription
    /// matching the same producer-side filter is already live on this
    /// messenger. The producer-side filter is part of the key because two
    /// from_any subs scoped to different producer instances do not share
    /// dedupe scope and must coexist; the failure mode the guard prevents
    /// is two from_any subs observing the *same* producer's emits, which
    /// is exactly when their `(name, tag)` exclusion sets and
    /// primary/secondary filtering need to give one (and only one)
    /// delivery per emit.
    pub(crate) fn reserve_from_any_topic(
        &self,
        from_core_node: Option<&str>,
        from_instance_id: Option<&str>,
        name: &str,
        tag: &str,
    ) -> Result<FromAnyTopicGuard> {
        let key: ActiveFromAnyKey = (
            from_core_node.map(str::to_string),
            from_instance_id.map(str::to_string),
            name.to_string(),
            tag.to_string(),
        );
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
        Ok(Self {
            messenger: Arc::new(Mutex::new(messenger)),
            pinned_siblings: Arc::new(RwLock::new(HashMap::new())),
            active_from_any_topics: Arc::new(StdMutex::new(HashSet::new())),
        })
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

    async fn emit_topic_message(
        &self,
        sender: &TopicWireSender,
        qos: QoSProfile,
        payload: Payload,
        is_primary: bool,
    ) -> Result<()> {
        let mut messenger = self.messenger.lock().await;
        messenger
            .publish_topic(sender, payload.into_inner().into(), qos.into(), is_primary)
            .await
            .map_err(Error::PeppyMessagingInterface)
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

        let mut response_subscription = {
            let messenger = self.messenger.lock().await;
            messenger
                .call_service(
                    sender,
                    request_payload.into_inner().into(),
                    kind,
                    response_timeout,
                )
                .await
                .map_err(Error::PeppyMessagingInterface)?
        };

        // Wait for the response, filtering replies by their attachment-side
        // `ServiceReplyKind`. The producer sends `Ack` immediately on receiving
        // a real user request (before the handler runs) and a terminal
        // `Response` or `HandlerError` once the handler returns. With a
        // timeout, ack-without-response → ServiceTimeout, no ack at all →
        // ServiceUnreachable. Probes get a single `Response` reply with an
        // empty payload (no ACK), so a probe consumer is guaranteed to break
        // the loop on the first reply.
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

        let reply = match response_timeout {
            Some(response_timeout) => {
                let deadline = Instant::now() + response_timeout;
                let mut received_ack = false;

                loop {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(if received_ack {
                            timed_out()
                        } else {
                            unreachable()
                        });
                    }

                    match timeout(remaining, response_subscription.rx.recv()).await {
                        Ok(Some(reply)) => match reply.kind() {
                            ServiceReplyKind::Ack => {
                                received_ack = true;
                                continue;
                            }
                            ServiceReplyKind::Response | ServiceReplyKind::HandlerError => {
                                break reply;
                            }
                        },
                        Ok(None) | Err(_) => {
                            return Err(if received_ack {
                                timed_out()
                            } else {
                                unreachable()
                            });
                        }
                    }
                }
            }
            None => loop {
                match response_subscription.rx.recv().await {
                    Some(reply) => match reply.kind() {
                        ServiceReplyKind::Ack => continue,
                        ServiceReplyKind::Response | ServiceReplyKind::HandlerError => {
                            break reply;
                        }
                    },
                    None => return Err(unreachable()),
                }
            },
        };

        let kind = reply.kind();
        let message = Message::from(reply.into_message());
        match kind {
            ServiceReplyKind::HandlerError => {
                let reason = match std::str::from_utf8(message.payload().as_ref()) {
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

        // Per-goal feedback uses `Important` (Block on congestion, DataHigh
        // priority) rather than `Standard`. The publisher is declared inside
        // the goal handler — the moment a fast server's first `emit_feedback`
        // fires, the local routing tables may not yet have the client's
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
}
