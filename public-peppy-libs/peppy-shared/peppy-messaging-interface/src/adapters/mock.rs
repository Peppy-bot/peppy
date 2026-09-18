use super::super::error::{Error, Result};
use super::super::types::{
    AbortOnDrop, ActionLivelinessProbe, CoreNodePresence, CoreNodePresenceList, IncomingRequest,
    LivelinessEvent, LivelinessToken, LivelinessWatch, Message, Messenger, MessengerAdapter,
    MessengerBackend, MockResponseToken, Payload, PresenceScope, PublisherQoS, ReplyStream,
    ResponseToken, ServiceQueryable, ServiceReply, SubscriberBufferSizes, SubscriberQoS,
    Subscription, TopicMessage, service_query_lifetime,
};
use super::super::wire::zenoh_format::ZenohWireFormat;
use super::super::wire::{
    ActionWireReceiver, ActionWireSender, Segment, ServiceQueryKind, ServiceWireReceiver,
    ServiceWireSender, TopicWireReceiver, TopicWireSender,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// RAII wrapper for a MockAdapter-based Messenger with router started.
pub struct MockInstance {
    messenger: Option<Messenger>,
    pub host: String,
    pub port: u16,
}

impl MockInstance {
    /// Returns a mutable reference to the messenger.
    pub fn messenger(&mut self) -> &mut Messenger {
        self.messenger
            .as_mut()
            .expect("messenger was already taken")
    }

    /// Takes ownership of the messenger, preventing automatic cleanup on drop.
    pub fn take_messenger(&mut self) -> Messenger {
        self.messenger.take().expect("messenger was already taken")
    }
}

impl Drop for MockInstance {
    fn drop(&mut self) {
        let Some(mut messenger) = self.messenger.take() else {
            return;
        };
        let _ = std::thread::spawn(move || {
            if let Ok(rt) = tokio::runtime::Runtime::new() {
                let _ = rt.block_on(async move { messenger.stop_router().await });
            }
        })
        .join();
    }
}

/// Shared map of published messages, keyed by topic. Used to record every
/// publish for later assertions.
type MessageLog = Arc<Mutex<HashMap<String, Vec<Message>>>>;

/// One active subscription entry. `drop_secondary` is set when the
/// subscriber wildcards the link_id slot at the keyexpr level (the topic
/// `from_link_id: None` case) — `route_publish` drops non-primary fan-out
/// for those entries so a multi-link `emit` yields one delivery.
pub struct MockSubscription {
    tx: flume::Sender<TopicMessage>,
    drop_secondary: bool,
}

/// Shared map of active subscriptions, keyed by pattern. Each pattern maps to
/// the senders that should receive a fanout when an intersecting topic is
/// published.
pub type SubscriptionMap = Arc<Mutex<HashMap<String, Vec<MockSubscription>>>>;

/// One in-flight query routed from a `get_keyexpr` caller to a queryable
/// whose declared keyexpr intersects the caller's selector. `attachment`
/// mirrors the Zenoh query attachment (carrying the request kind plus the
/// sibling-pinned exclusion set) so the in-process matcher honors the
/// same protocol semantics as the live transport.
pub(crate) struct MockQuery {
    selector_keyexpr: String,
    payload: Payload,
    attachment: bytes::Bytes,
    reply_tx: mpsc::Sender<ServiceReply>,
}

/// Shared map of declared queryables, keyed by the producer's declared
/// keyexpr. Each entry holds the channels feeding the forwarder tasks
/// behind a [`ServiceQueryable`] — `get_keyexpr` finds matching entries
/// via [`MockAdapter::key_exprs_intersect`] and pushes a [`MockQuery`]
/// onto each.
type QueryableMap = Arc<Mutex<HashMap<String, Vec<mpsc::Sender<MockQuery>>>>>;

/// In-process stand-in for Zenoh's liveliness space. `tokens` counts the
/// live tokens per declared keyexpr (a count, not a set, so two declares
/// on the same keyexpr need two drops to go Gone — mirroring Zenoh, where
/// each token is independent). `watchers` holds the fan-out callbacks of
/// active [`LivelinessWatch`]es keyed by their watch pattern; a
/// declare/drop notifies every watcher whose pattern intersects the
/// token's keyexpr.
#[derive(Default)]
struct MockLivelinessState {
    tokens: HashMap<String, usize>,
    watchers: HashMap<String, Vec<MockLivelinessWatcher>>,
}

/// Registration-time callback that types and forwards one liveliness
/// transition to its watch channel. Each watch domain owns its event
/// mapping (action watches forward the `()` event as-is, presence watches
/// parse the token keyexpr), mirroring how the zenoh adapter's subscriber
/// callbacks own theirs — the shared notifier stays domain-blind.
type MockLivelinessWatcher = Box<dyn Fn(&str, LivelinessEvent) + Send>;

type LivelinessState = Arc<Mutex<MockLivelinessState>>;

pub struct MockAdapter {
    pub(crate) is_session_connected: bool,
    pub(crate) is_router_started: bool,
    pub(crate) messages: MessageLog,
    pub(crate) subscriptions: SubscriptionMap,
    pub(crate) queryables: QueryableMap,
    liveliness: LivelinessState,
}

impl Default for MockAdapter {
    fn default() -> Self {
        Self {
            is_session_connected: false,
            is_router_started: false,
            messages: Arc::new(Mutex::new(HashMap::new())),
            subscriptions: Arc::new(Mutex::new(HashMap::new())),
            queryables: Arc::new(Mutex::new(HashMap::new())),
            liveliness: Arc::new(Mutex::new(MockLivelinessState::default())),
        }
    }
}

impl MockAdapter {
    /// Clone of the shared subscription map. Exposed so matching-status
    /// waits (peppylib's `wait_for_matching_subscriber`) can poll for a
    /// subscriber WITHOUT holding the owning messenger's lock — in-process
    /// mock tests share one messenger between publisher and subscriber, so
    /// polling under that lock would starve the subscribe it waits for.
    pub fn subscription_map(&self) -> SubscriptionMap {
        Arc::clone(&self.subscriptions)
    }

    /// Mock counterpart of Zenoh's publisher matching status: does any LIVE
    /// subscription intersect `sender`'s publish keyexpr? Dropped
    /// subscriptions leave stale senders in the map by design (see
    /// `subscribe_keyexpr`); they are excluded here so a wait cannot match a
    /// subscription that no longer receives.
    pub fn topic_has_matching_subscriber(map: &SubscriptionMap, sender: &TopicWireSender) -> bool {
        let publish_keyexpr = ZenohWireFormat::topic_publish(sender);
        let subscriptions = map.lock().unwrap();
        subscriptions.iter().any(|(declared, entries)| {
            Self::key_exprs_intersect(declared, &publish_keyexpr)
                && entries.iter().any(|entry| !entry.tx.is_disconnected())
        })
    }
}

impl MessengerBackend for MockAdapter {
    async fn start_session(&mut self) -> Result<()> {
        self.is_session_connected = true;
        Ok(())
    }

    async fn stop_session(&mut self) -> Result<()> {
        if !self.is_session_connected {
            return Err(Error::ShutdownError);
        }

        self.is_session_connected = false;
        self.is_router_started = false;

        self.messages.lock().unwrap().clear();
        self.subscriptions.lock().unwrap().clear();
        self.queryables.lock().unwrap().clear();

        // Mirror Zenoh: closing the session removes every liveliness token
        // it declared, and watchers observe the removals as Gone events. A
        // fresh state isolates the next session from guards created by this
        // one.
        let previous_liveliness = std::mem::replace(
            &mut self.liveliness,
            Arc::new(Mutex::new(MockLivelinessState::default())),
        );
        {
            let mut liveliness = previous_liveliness.lock().unwrap();
            let keyexprs: Vec<String> = liveliness.tokens.keys().cloned().collect();
            liveliness.tokens.clear();
            for keyexpr in keyexprs {
                Self::notify_liveliness_watchers(&liveliness, &keyexpr, LivelinessEvent::Gone(()));
            }
            liveliness.watchers.clear();
        }

        Ok(())
    }

    async fn subscribe_topic(
        &self,
        recv: &TopicWireReceiver,
        qos: SubscriberQoS,
    ) -> Result<Subscription> {
        let drop_secondary = recv.drops_secondary_publishes();
        self.subscribe_keyexpr(&ZenohWireFormat::topic_subscribe(recv), qos, drop_secondary)
            .await
    }

    async fn publish_topic(
        &mut self,
        sender: &TopicWireSender,
        payload: Payload,
        _qos: PublisherQoS,
        is_primary: bool,
    ) -> Result<()> {
        self.publish_keyexpr(ZenohWireFormat::topic_publish(sender), payload, is_primary)
            .await
    }

    async fn listen_service(&self, recv: &ServiceWireReceiver) -> Result<ServiceQueryable> {
        if !self.is_session_connected {
            return Err(Error::SubscribeError {
                topic: recv.as_service_name.as_str().to_string(),
            });
        }

        let (tx, rx) = flume::bounded::<IncomingRequest>(
            SubscriberBufferSizes::default().size_for(SubscriberQoS::Standard),
        );

        // One queryable per listen call (see `ZenohAdapter::listen_service`
        // for the rationale — the same shape applies here so peppylib tests
        // exercise the same dispatch logic against the mock).
        let declare_keyexpr = ZenohWireFormat::service_queryable_declare(recv);
        let query_rx = self.declare_queryable_keyexpr(declare_keyexpr);
        let recv_clone = recv.clone();
        let join_handle = tokio::spawn(async move {
            handle_mock_queryable(query_rx, recv_clone, tx).await;
        });

        Ok(ServiceQueryable::new(
            rx,
            vec![Box::new(AbortOnDrop::new(join_handle.abort_handle()))],
        ))
    }

    async fn call_service(
        &self,
        sender: &ServiceWireSender,
        payload: Payload,
        kind: ServiceQueryKind,
        timeout: Option<std::time::Duration>,
    ) -> Result<ReplyStream> {
        if !self.is_session_connected {
            return Err(Error::PublishError {
                topic: sender.to_service_name().to_string(),
            });
        }

        let selector = ZenohWireFormat::service_get_selector(sender);
        let attachment = ZenohWireFormat::service_get_selector_attachment(sender, kind);
        let timeout = service_query_lifetime(timeout);

        let (reply_tx, mut reply_rx) = mpsc::channel::<ServiceReply>(
            SubscriberBufferSizes::default().size_for(SubscriberQoS::Standard),
        );

        // Snapshot matching queryable channels under the map lock, then dispatch
        // outside the lock so async send doesn't hold a sync mutex across await.
        let matching: Vec<mpsc::Sender<MockQuery>> = {
            let queryables = self.queryables.lock().unwrap();
            queryables
                .iter()
                .filter(|(declared, _)| Self::key_exprs_intersect(declared, &selector))
                .flat_map(|(_, senders)| senders.iter().cloned())
                .collect()
        };

        for tx in matching {
            let q = MockQuery {
                selector_keyexpr: selector.clone(),
                payload: payload.clone(),
                attachment: attachment.clone(),
                reply_tx: reply_tx.clone(),
            };
            let _ = tx.send(q).await;
        }

        // Drop the local clone so the reply channel closes once every queryable
        // forwarder's `MockResponseToken` (each holding a `reply_tx` clone) is
        // dropped — typically after the user handler's final `respond` call.
        drop(reply_tx);

        let (output_tx, output_rx) = mpsc::channel::<ServiceReply>(
            SubscriberBufferSizes::default().size_for(SubscriberQoS::Standard),
        );
        let pump_task = tokio::spawn(async move {
            let _ = tokio::time::timeout(timeout, async move {
                while let Some(msg) = reply_rx.recv().await {
                    if output_tx.send(msg).await.is_err() {
                        break;
                    }
                }
            })
            .await;
        });

        Ok(ReplyStream::new(
            output_rx,
            Some(Box::new(AbortOnDrop::new(pump_task.abort_handle()))),
        ))
    }

    async fn subscribe_action_feedback(
        &self,
        sender: &ActionWireSender,
        goal_id: &str,
        qos: SubscriberQoS,
    ) -> Result<Subscription> {
        // Action feedback publishes exactly once per goal (see the wire
        // comment on `action_feedback_publish`), so there are no secondaries
        // to drop even though the subscribe keyexpr wildcards the link_id
        // slot. See the matching note in `ZenohAdapter::subscribe_action_feedback`.
        self.subscribe_keyexpr(
            &ZenohWireFormat::action_feedback_subscribe(sender, goal_id),
            qos,
            false,
        )
        .await
    }

    async fn declare_action_liveliness(
        &self,
        recv: &ActionWireReceiver,
    ) -> Result<LivelinessToken> {
        self.declare_liveliness(ZenohWireFormat::action_liveliness_token(recv))
    }

    async fn watch_action_producer(&self, sender: &ActionWireSender) -> Result<LivelinessWatch> {
        if !self.is_session_connected {
            return Err(Error::MessagingSessionError(
                "Session not initialized".to_string(),
            ));
        }
        let pattern = ZenohWireFormat::action_liveliness_watch(sender);
        let (tx, rx) = flume::unbounded::<LivelinessEvent>();
        {
            let mut liveliness = self.liveliness.lock().unwrap();
            // History emulation: a token that already exists is replayed as
            // an initial Alive, matching the Zenoh watch's `history(true)`.
            if Self::live_keys_matching(&liveliness, &pattern)
                .next()
                .is_some()
            {
                let _ = tx.send(LivelinessEvent::Alive(()));
            }
            liveliness
                .watchers
                .entry(pattern)
                .or_default()
                .push(Box::new(move |_keyexpr, event| {
                    let _ = tx.send(event);
                }));
        }
        // Stale watcher callbacks in the map are benign (they ignore send
        // errors), mirroring the topic SubscriptionMap convention.
        Ok(LivelinessWatch::new(rx, Box::new(())))
    }

    async fn probe_action_producer(
        &self,
        sender: &ActionWireSender,
        _timeout: std::time::Duration,
    ) -> Result<ActionLivelinessProbe> {
        if !self.is_session_connected {
            return Err(Error::MessagingSessionError(
                "Session not initialized".to_string(),
            ));
        }
        let pattern = ZenohWireFormat::action_liveliness_watch(sender);
        let alive = {
            let liveliness = self.liveliness.lock().unwrap();
            Self::live_keys_matching(&liveliness, &pattern)
                .next()
                .is_some()
        };
        // The mock answers instantly: send the alive marker (or don't) and
        // drop the sender so `resolve` returns without waiting.
        let (tx, rx) = flume::bounded::<()>(1);
        if alive {
            let _ = tx.try_send(());
        }
        drop(tx);
        Ok(ActionLivelinessProbe::new(rx))
    }

    async fn declare_core_node_presence(
        &self,
        core_node: &Segment,
        instance_id: &Segment,
    ) -> Result<LivelinessToken> {
        self.declare_liveliness(ZenohWireFormat::core_node_presence_token(
            core_node,
            instance_id,
        ))
    }

    async fn watch_core_node_presence(
        &self,
        core_node: Option<&Segment>,
    ) -> Result<LivelinessWatch<CoreNodePresence>> {
        if !self.is_session_connected {
            return Err(Error::MessagingSessionError(
                "Session not initialized".to_string(),
            ));
        }
        let pattern = ZenohWireFormat::core_node_presence_filter(PresenceScope::Session, core_node);
        let (tx, rx) = flume::unbounded::<LivelinessEvent<CoreNodePresence>>();
        {
            let mut liveliness = self.liveliness.lock().unwrap();
            // History must replay every concrete token, not merely one event
            // per name: duplicate instance ids are the collision signal.
            for keyexpr in Self::live_keys_matching(&liveliness, &pattern) {
                if let Ok(presence) =
                    ZenohWireFormat::parse_core_node_presence(PresenceScope::Session, keyexpr)
                {
                    let _ = tx.send(LivelinessEvent::Alive(presence));
                }
            }
            liveliness
                .watchers
                .entry(pattern)
                .or_default()
                .push(Box::new(move |keyexpr, event| {
                    let Ok(presence) =
                        ZenohWireFormat::parse_core_node_presence(PresenceScope::Session, keyexpr)
                    else {
                        return;
                    };
                    let _ = tx.send(match event {
                        LivelinessEvent::Alive(()) => LivelinessEvent::Alive(presence),
                        LivelinessEvent::Gone(()) => LivelinessEvent::Gone(presence),
                    });
                }));
        }
        Ok(LivelinessWatch::new(rx, Box::new(())))
    }

    /// The mock has no session namespace, so it declares presence tokens with
    /// the bare grammar and a [`PresenceScope::Namespace`] query matches
    /// nothing here. That is deliberate rather than an omission: emulating
    /// zenoh's egress prefixing for presence alone (and not for pub/sub,
    /// services, or action liveliness) would make the mock look like it proves
    /// workspace isolation when it proves only that two strings differ. The
    /// selector and parser are pinned directly in `zenoh_format`'s unit tests,
    /// and isolation over a real transport is proven against a live router in
    /// `tests/zenoh.rs`.
    async fn list_core_node_presence(
        &self,
        scope: PresenceScope<'_>,
        core_node: Option<&Segment>,
        _timeout: std::time::Duration,
    ) -> Result<CoreNodePresenceList> {
        if !self.is_session_connected {
            return Err(Error::MessagingSessionError(
                "Session not initialized".to_string(),
            ));
        }
        let pattern = ZenohWireFormat::core_node_presence_filter(scope, core_node);
        // The mock answers instantly: pre-fill the reply channel and drop
        // the sender so `collect` returns without waiting.
        let (tx, rx) = flume::unbounded::<Result<CoreNodePresence>>();
        {
            let liveliness = self.liveliness.lock().unwrap();
            for keyexpr in Self::live_keys_matching(&liveliness, &pattern) {
                let _ = tx.send(
                    ZenohWireFormat::parse_core_node_presence(scope, keyexpr).map_err(Into::into),
                );
            }
        }
        drop(tx);
        Ok(CoreNodePresenceList::new(rx))
    }

    async fn start_router(&mut self) -> Result<()> {
        self.is_router_started = true;
        Ok(())
    }

    async fn stop_router(&mut self) -> Result<()> {
        self.is_router_started = false;
        Ok(())
    }

    fn get_host(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], config::consts::DEFAULT_MESSAGING_PORT))
    }
}

/// Drop-guard backing the mock's [`LivelinessToken`]. Removing the
/// last token on a keyexpr notifies intersecting watchers with a Gone
/// event, mirroring Zenoh's token undeclaration.
struct MockLivelinessGuard {
    keyexpr: String,
    state: LivelinessState,
}

impl Drop for MockLivelinessGuard {
    fn drop(&mut self) {
        let Ok(mut liveliness) = self.state.lock() else {
            return;
        };
        let remaining = match liveliness.tokens.get_mut(&self.keyexpr) {
            Some(count) => {
                *count = count.saturating_sub(1);
                *count
            }
            // Already cleared by `stop_session` (which notified watchers).
            None => return,
        };
        if remaining == 0 {
            liveliness.tokens.remove(&self.keyexpr);
            MockAdapter::notify_liveliness_watchers(
                &liveliness,
                &self.keyexpr,
                LivelinessEvent::Gone(()),
            );
        }
    }
}

impl MockAdapter {
    /// Fan a liveliness transition out to every watcher whose pattern
    /// intersects `keyexpr`. Each watcher callback attaches its own domain's
    /// event value; send errors (dropped watches) are ignored, mirroring the
    /// topic `SubscriptionMap` convention.
    fn notify_liveliness_watchers(
        liveliness: &MockLivelinessState,
        keyexpr: &str,
        event: LivelinessEvent,
    ) {
        for (pattern, watchers) in liveliness.watchers.iter() {
            if !Self::key_exprs_intersect(pattern, keyexpr) {
                continue;
            }
            for watcher in watchers {
                watcher(keyexpr, event);
            }
        }
    }

    /// Shared body of the liveliness declaration methods: bumps the live
    /// count for `keyexpr`, notifies intersecting watchers, and returns a
    /// token whose drop reverses both.
    fn declare_liveliness(&self, keyexpr: String) -> Result<LivelinessToken> {
        if !self.is_session_connected {
            return Err(Error::MessagingSessionError(
                "Session not initialized".to_string(),
            ));
        }
        {
            let mut liveliness = self.liveliness.lock().unwrap();
            *liveliness.tokens.entry(keyexpr.clone()).or_insert(0) += 1;
            Self::notify_liveliness_watchers(&liveliness, &keyexpr, LivelinessEvent::Alive(()));
        }
        Ok(LivelinessToken::new(Box::new(MockLivelinessGuard {
            keyexpr,
            state: Arc::clone(&self.liveliness),
        })))
    }

    /// Keyexprs of the currently live tokens intersecting `pattern` — the
    /// mock-side equivalent of a liveliness query.
    fn live_keys_matching<'a>(
        liveliness: &'a MockLivelinessState,
        pattern: &'a str,
    ) -> impl Iterator<Item = &'a str> {
        liveliness
            .tokens
            .iter()
            .filter(move |(keyexpr, count)| {
                **count > 0 && Self::key_exprs_intersect(keyexpr, pattern)
            })
            .map(|(keyexpr, _)| keyexpr.as_str())
    }

    /// Creates a new MockAdapter, wraps it in a Messenger, starts the router,
    /// and returns a `MockInstance` for managing the lifecycle.
    ///
    /// This mirrors the interface of `ZenohAdapter::start_router_ephemeral`.
    pub async fn start_router() -> Result<MockInstance> {
        let adapter = Self::default();
        let mut messenger = Messenger::new(MessengerAdapter::Mock(adapter));
        messenger.start_router().await?;

        Ok(MockInstance {
            messenger: Some(messenger),
            host: config::consts::DEFAULT_MESSAGING_HOST.to_string(),
            port: config::consts::DEFAULT_MESSAGING_PORT,
        })
    }

    /// Returns `true` when two key expressions intersect — that is, when there
    /// exists at least one concrete key matched by both. Wildcards on either
    /// side are honored, mirroring Zenoh's bidirectional `keyexpr` matching:
    ///
    /// - `*` matches exactly one non-empty chunk.
    /// - `**` matches zero or more non-empty chunks.
    ///
    /// Symmetric in its arguments.
    fn key_exprs_intersect(a: &str, b: &str) -> bool {
        let a_chunks: Vec<&str> = a.split('/').collect();
        let b_chunks: Vec<&str> = b.split('/').collect();
        Self::intersect_chunks(&a_chunks, &b_chunks)
    }

    fn intersect_chunks(a: &[&str], b: &[&str]) -> bool {
        match (a.first().copied(), b.first().copied()) {
            (None, None) => true,
            // The non-empty side can still intersect if every remaining chunk
            // is `**` (each can collapse to zero chunks).
            (None, Some(_)) => b.iter().all(|c| *c == "**"),
            (Some(_), None) => a.iter().all(|c| *c == "**"),
            // `**` either consumes zero chunks (skip it) or one chunk from the
            // other side (stay put). Standard regex-style branching.
            (Some("**"), _) => {
                Self::intersect_chunks(&a[1..], b) || Self::intersect_chunks(a, &b[1..])
            }
            (_, Some("**")) => {
                Self::intersect_chunks(a, &b[1..]) || Self::intersect_chunks(&a[1..], b)
            }
            (Some(a0), Some(b0)) => {
                Self::single_chunk_intersect(a0, b0) && Self::intersect_chunks(&a[1..], &b[1..])
            }
        }
    }

    fn single_chunk_intersect(a: &str, b: &str) -> bool {
        match (a, b) {
            ("*", x) | (x, "*") => !x.is_empty(),
            (x, y) => x == y,
        }
    }

    fn to_response_message(message: &Message) -> Result<TopicMessage> {
        let identifier = message.identifier();
        TopicMessage::new(identifier, message.payload().clone())
    }

    /// Pre-bind a per-topic publisher for `sender`. The returned publisher
    /// clones the adapter's `Arc`s, bypassing the central `Messenger` mutex.
    pub fn declare_topic_publisher(
        &self,
        sender: &TopicWireSender,
        _qos: PublisherQoS,
    ) -> MockPublisher {
        self.declare_publisher_keyexpr(ZenohWireFormat::topic_publish(sender))
    }

    /// Pre-bind a per-goal action-feedback publisher.
    pub fn declare_action_feedback_publisher(
        &self,
        recv: &ActionWireReceiver,
        link_id: &str,
        goal_id: &str,
        _qos: PublisherQoS,
    ) -> MockPublisher {
        self.declare_publisher_keyexpr(ZenohWireFormat::action_feedback_publish(
            recv, link_id, goal_id,
        ))
    }

    fn declare_publisher_keyexpr(&self, topic: String) -> MockPublisher {
        MockPublisher {
            topic,
            subscriptions: Arc::clone(&self.subscriptions),
            messages: Arc::clone(&self.messages),
        }
    }

    async fn publish_keyexpr(
        &self,
        topic: String,
        payload: Payload,
        is_primary: bool,
    ) -> Result<()> {
        if !self.is_session_connected {
            return Err(Error::PublishError { topic });
        }

        let message = Message::new(&topic, payload.to_bytes());
        Self::route_publish(
            &topic,
            &message,
            is_primary,
            &self.messages,
            &self.subscriptions,
        )
        .await
    }

    /// Records `message` against `topic` in the mock's message log and fans
    /// it out to every subscription whose pattern intersects `topic`. Shared
    /// by [`MockAdapter::publish_keyexpr`] (which holds the adapter
    /// directly) and [`MockPublisher::publish`] (which clones the same
    /// `Arc`s for lock-free per-topic publishing). `is_primary` is the
    /// wire-attachment dedup marker — subscribers that wildcarded the
    /// link_id slot drop non-primary fan-out.
    async fn route_publish(
        topic: &str,
        message: &Message,
        is_primary: bool,
        messages: &MessageLog,
        subscriptions: &SubscriptionMap,
    ) -> Result<()> {
        let response = Self::to_response_message(message)?;

        {
            let mut messages = messages.lock().unwrap();
            messages
                .entry(topic.to_string())
                .or_default()
                .push(message.clone());
        }

        let senders: Vec<flume::Sender<TopicMessage>> = {
            let subscriptions = subscriptions.lock().unwrap();
            let mut matched = Vec::new();
            for (pattern, subs) in subscriptions.iter() {
                if !Self::key_exprs_intersect(pattern, topic) {
                    continue;
                }
                for sub in subs.iter() {
                    if sub.drop_secondary && !is_primary {
                        continue;
                    }
                    matched.push(sub.tx.clone());
                }
            }
            matched
        };

        for sender in senders {
            let _ = sender.send_async(response.clone()).await;
        }
        Ok(())
    }

    /// Register a queryable under `declared_keyexpr` and return the channel
    /// the per-queryable forwarder task reads inbound queries from. Senders
    /// stored in the map outlive the forwarder task — `get_keyexpr` ignores
    /// closed senders rather than garbage-collecting them, mirroring the
    /// topic [`SubscriptionMap`] convention.
    fn declare_queryable_keyexpr(&self, declared_keyexpr: String) -> mpsc::Receiver<MockQuery> {
        let (tx, rx) =
            mpsc::channel(SubscriberBufferSizes::default().size_for(SubscriberQoS::Standard));
        let mut queryables = self.queryables.lock().unwrap();
        queryables.entry(declared_keyexpr).or_default().push(tx);
        rx
    }

    async fn subscribe_keyexpr(
        &self,
        topic: &str,
        qos: SubscriberQoS,
        drop_secondary: bool,
    ) -> Result<Subscription> {
        if !self.is_session_connected {
            return Err(Error::SubscribeError {
                topic: topic.to_string(),
            });
        }

        let (tx, rx) = flume::bounded(SubscriberBufferSizes::default().size_for(qos));

        {
            let mut subscriptions = self.subscriptions.lock().unwrap();
            subscriptions
                .entry(topic.to_string())
                .or_default()
                .push(MockSubscription {
                    tx: tx.clone(),
                    drop_secondary,
                });
        }

        // No background task or guard is needed — the mock writes directly
        // into the sender from `publish_keyexpr`, so dropping the
        // Subscription's `rx` is enough to stop reception. The stale `tx`
        // clone in the subscriptions map is benign: `route_publish` ignores
        // send errors on dead senders.
        Ok(Subscription::new(rx, Box::new(())))
    }
}

/// Per-queryable forwarder for the mock adapter. Mirrors
/// [`super::zenoh::handle_queryable`]: drains inbound `MockQuery`s, parses
/// the caller identity and link_id slot, claims the producer's default `_`
/// segment via [`ParsedInboundQuery::claim`], builds an [`IncomingRequest`]
/// with a [`ResponseToken::Mock`] carrying the per-query reply channel, and
/// pushes it to peppylib. Queries whose link_id slot is neither `*` nor `_`
/// are dropped (the `mock_query`'s reply_tx clone falls out of scope at end
/// of iteration so the caller's reply stream finalizes once every reply_tx
/// is dropped).
async fn handle_mock_queryable(
    mut query_rx: mpsc::Receiver<MockQuery>,
    recv: ServiceWireReceiver,
    tx: flume::Sender<IncomingRequest>,
) {
    while let Some(mock_query) = query_rx.recv().await {
        let parsed = match ZenohWireFormat::parse_inbound_query(
            &recv,
            &mock_query.selector_keyexpr,
            mock_query.attachment.as_ref(),
        ) {
            Ok(p) => p,
            Err(err) => {
                tracing::warn!(
                    selector = %mock_query.selector_keyexpr,
                    %err,
                    "mock queryable: failed to parse selector",
                );
                continue;
            }
        };

        let chosen_link_id = match parsed.claim() {
            Some(l) => l.to_string(),
            None => {
                tracing::trace!(
                    selector = %mock_query.selector_keyexpr,
                    parsed_link_id = %parsed.link_id,
                    "mock queryable: dropping query with link_id slot neither '*' nor '_'",
                );
                continue;
            }
        };

        let reply_keyexpr = ZenohWireFormat::service_reply_keyexpr(
            &recv,
            &chosen_link_id,
            &parsed.caller_core,
            &parsed.caller_inst,
        );

        let token = ResponseToken::Mock(MockResponseToken::new(mock_query.reply_tx, reply_keyexpr));

        // Mirrors the zenoh adapter: probes (liveness, discovery, benchmark
        // sized-probes) are answered in the dispatch path — Response-kind,
        // never Ack — and never reach the endpoint channel, so a producer
        // busy in user code still answers them.
        if parsed.kind == ServiceQueryKind::Probe {
            let response =
                crate::probe::probe_response_body(mock_query.payload.as_bytes().as_ref());
            if let Err(err) = token.respond_response(Payload::from_bytes(response)).await {
                tracing::warn!(%err, "mock queryable: failed to publish probe response");
            }
            continue;
        }

        let request = IncomingRequest {
            payload: mock_query.payload,
            kind: parsed.kind,
            link_id: chosen_link_id,
            caller_core: parsed.caller_core,
            caller_inst: parsed.caller_inst,
            token,
        };

        if tx.send_async(request).await.is_err() {
            break;
        }
    }
}

/// Mock-side per-topic publisher returned by [`MockAdapter::declare_publisher`].
/// Holds `Arc`s into the adapter's in-process matcher state, so `publish` is
/// independent of the `Arc<Mutex<Messenger>>` global lock that everyone shares.
pub struct MockPublisher {
    topic: String,
    subscriptions: SubscriptionMap,
    messages: MessageLog,
}

impl MockPublisher {
    pub async fn publish(&self, payload: bytes::Bytes) -> Result<()> {
        // Pre-bound publishers are single-link, so each publish is its own
        // emit's only sample — always primary, mirroring the Zenoh side.
        let message = Message::new(&self.topic, payload);
        MockAdapter::route_publish(
            &self.topic,
            &message,
            true,
            &self.messages,
            &self.subscriptions,
        )
        .await
    }
}

// End-to-end behavior of the mock vs. real messaging is covered by the typed
// roundtrip tests in `tests/wire.rs`. These local tests pin the
// `key_exprs_intersect` matching primitive that drives in-process routing.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_key_exprs_intersect_exact() {
        assert!(MockAdapter::key_exprs_intersect("a/b/c", "a/b/c"));
        assert!(!MockAdapter::key_exprs_intersect("a/b/c", "a/b/d"));
        assert!(!MockAdapter::key_exprs_intersect("a/b/c", "a/b"));
        assert!(!MockAdapter::key_exprs_intersect("a/b", "a/b/c"));
    }

    #[test]
    fn test_key_exprs_intersect_single_wildcard() {
        // * matches exactly one chunk
        assert!(MockAdapter::key_exprs_intersect("a/*/c", "a/b/c"));
        assert!(MockAdapter::key_exprs_intersect("a/*/c", "a/xyz/c"));
        assert!(!MockAdapter::key_exprs_intersect("a/*/c", "a/b/d"));
        assert!(!MockAdapter::key_exprs_intersect("a/*/c", "a/b/c/d"));
        assert!(MockAdapter::key_exprs_intersect("*/b/c", "a/b/c"));
        assert!(MockAdapter::key_exprs_intersect("a/b/*", "a/b/c"));
        assert!(MockAdapter::key_exprs_intersect("*/*/*/*", "a/b/c/d"));
    }

    #[test]
    fn test_key_exprs_intersect_double_wildcard() {
        // ** matches zero or more chunks
        assert!(MockAdapter::key_exprs_intersect("a/**", "a"));
        assert!(MockAdapter::key_exprs_intersect("a/**", "a/b"));
        assert!(MockAdapter::key_exprs_intersect("a/**", "a/b/c"));
        assert!(MockAdapter::key_exprs_intersect("a/**", "a/b/c/d"));
        assert!(!MockAdapter::key_exprs_intersect("a/**", "b"));
        assert!(!MockAdapter::key_exprs_intersect("a/**", "b/a"));
        assert!(MockAdapter::key_exprs_intersect("**", "a"));
        assert!(MockAdapter::key_exprs_intersect("**", "a/b/c"));
    }

    #[test]
    fn test_key_exprs_intersect_mixed_wildcards() {
        // Combination of * and **
        assert!(MockAdapter::key_exprs_intersect("a/*/c/**", "a/b/c"));
        assert!(MockAdapter::key_exprs_intersect("a/*/c/**", "a/b/c/d"));
        assert!(MockAdapter::key_exprs_intersect("a/*/c/**", "a/b/c/d/e"));
        assert!(!MockAdapter::key_exprs_intersect("a/*/c/**", "a/b/d"));
        assert!(MockAdapter::key_exprs_intersect(
            "*/*/service/**",
            "core_node/caller/service/ping/request/123"
        ));
    }

    #[test]
    fn test_key_exprs_intersect_service_patterns() {
        // Real patterns from the service messenger
        // Subscription pattern: {bound_core_node}/*/{as_instance_id}/*/{service_root}/request/**
        // Request topic: {target_core_node}/{caller_core_node}/{to_instance}/{caller_instance}/{service_root}/request/{request_id}

        // Pattern 1: Specific core node, specific instance
        // Service bound to core node "listener_core_node" with instance "listener_instance"
        let pattern = "listener_core_node/*/listener_instance/*/service/node/ping/request/**";
        // Request targeting the specific instance
        let topic = "listener_core_node/caller_core_node/listener_instance/caller_instance/service/node/ping/request/12345";
        assert!(MockAdapter::key_exprs_intersect(pattern, topic));

        // Pattern 3: Broadcast core node (_any_), specific instance
        let pattern = "_any_/*/listener_instance/*/service/node/ping/request/**";
        let topic = "_any_/caller_core_node/listener_instance/caller_instance/service/node/ping/request/12345";
        assert!(MockAdapter::key_exprs_intersect(pattern, topic));

        // Pattern 4: Broadcast core node, broadcast instance
        let pattern = "_any_/*/_any_/*/service/node/ping/request/**";
        let topic = "_any_/caller_core_node/_any_/caller_instance/service/node/ping/request/12345";
        assert!(MockAdapter::key_exprs_intersect(pattern, topic));

        // CoreNode uses its own name as the bound core node (e.g., "core_node")
        // This allows targeted requests to reach the core node specifically
        let pattern = "core_node/*/listener_instance/*/service/node/ping/request/**";
        let topic = "core_node/caller_core_node/listener_instance/caller_instance/service/node/ping/request/12345";
        assert!(MockAdapter::key_exprs_intersect(pattern, topic));
    }

    #[test]
    fn test_key_exprs_intersect_is_symmetric() {
        // Wildcards on either side intersect; order of arguments must not matter.
        assert!(MockAdapter::key_exprs_intersect("a/*/c", "*/b/c"));
        assert!(MockAdapter::key_exprs_intersect("*/b/c", "a/*/c"));
        assert!(MockAdapter::key_exprs_intersect("a/**", "**/c"));
        assert!(MockAdapter::key_exprs_intersect("**/c", "a/**"));
        // Two `**` on the same side never share a key when their literal anchors differ.
        assert!(!MockAdapter::key_exprs_intersect("**/a", "**/b"));
    }

    #[test]
    fn test_key_exprs_intersect_topic_publisher_vs_subscriber() {
        // The exact wire shape that motivated bidirectional matching: the topic
        // publish path hard-codes `*` into caller-identity slots, while a
        // subscriber identifies itself with concrete core/instance values. Both
        // sides must intersect or topic delivery against the mock breaks.
        let publisher = "*/core_node/*/responder_inst/topic/clock/clock";
        let subscriber = "caller_core/core_node/caller_inst/responder_inst/topic/clock/clock";
        assert!(MockAdapter::key_exprs_intersect(subscriber, publisher));
        assert!(MockAdapter::key_exprs_intersect(publisher, subscriber));
    }

    #[tokio::test]
    async fn mock_queryable_roundtrip_wildcard_selector() {
        // Direct exercise of the mock's queryable plumbing: a producer declares
        // a queryable on a concrete keyexpr; a `get` selector with a Zenoh
        // wildcard at one slot must still match and deliver the query, and the
        // responder must be able to push a reply that the caller observes —
        // without going through a real zenohd.
        use crate::wire::{
            SenderTarget, ServiceKind, ServiceQueryKind, ServiceReplyKind, ServiceWireReceiver,
            ServiceWireSender,
        };

        let mut adapter = MockAdapter::default();
        adapter.start_session().await.expect("session should start");

        let receiver = ServiceWireReceiver::new(
            "server_core",
            "server_inst",
            SenderTarget::contract("depth_camera", "v1").expect("contract target"),
            "ping",
            ServiceKind::Service,
        )
        .expect("valid receiver");

        // The link_id wire slot is unconditionally `*`; the producer accepts
        // it via `ParsedInboundQuery::claim` and dispatches under `_`.
        let sender = ServiceWireSender::new(
            "caller_core",
            "caller_inst",
            Some(&config::runtime::ProducerRef::new(
                "server_core",
                "server_inst",
            )),
            SenderTarget::contract("depth_camera", "v1").expect("contract target"),
            "ping",
            ServiceKind::Service,
        )
        .expect("valid sender");

        let queryable = adapter
            .listen_service(&receiver)
            .await
            .expect("queryable declare should succeed");

        let mut reply_stream = adapter
            .call_service(
                &sender,
                Payload::from_bytes(bytes::Bytes::from_static(b"ping?")),
                ServiceQueryKind::UserRequest,
                Some(std::time::Duration::from_millis(500)),
            )
            .await
            .expect("call_service should succeed");

        let incoming = queryable
            .rx
            .recv_async()
            .await
            .expect("producer should receive the query");
        assert_eq!(incoming.payload.to_bytes().as_ref(), b"ping?");
        assert_eq!(incoming.kind, ServiceQueryKind::UserRequest);
        assert_eq!(incoming.link_id, "_");
        assert_eq!(incoming.caller_core, "caller_core");
        assert_eq!(incoming.caller_inst, "caller_inst");

        incoming
            .token
            .respond_response(Payload::from_bytes(bytes::Bytes::from_static(b"pong")))
            .await
            .expect("respond should succeed");

        let reply = reply_stream
            .rx
            .recv()
            .await
            .expect("caller should receive the reply");
        assert_eq!(reply.kind(), ServiceReplyKind::Response);
        assert_eq!(reply.message().payload().to_bytes().as_ref(), b"pong");
    }

    /// Probes are answered by the adapter's dispatch path — one
    /// Response-kind reply — and never reach the producer's endpoint
    /// channel, so a producer busy in user code (not parked in its recv
    /// loop) still answers liveness/discovery/benchmark probes. Sized
    /// probes get a response of the requested size; plain probes get an
    /// empty one.
    #[tokio::test]
    async fn mock_queryable_answers_probes_in_dispatch_without_enqueueing() {
        use crate::wire::{
            SenderTarget, ServiceKind, ServiceQueryKind, ServiceReplyKind, ServiceWireReceiver,
            ServiceWireSender,
        };

        let mut adapter = MockAdapter::default();
        adapter.start_session().await.expect("session should start");

        let receiver = ServiceWireReceiver::new(
            "server_core",
            "server_inst",
            SenderTarget::node("camera", "v1").expect("node target"),
            "ping",
            ServiceKind::Service,
        )
        .expect("valid receiver");
        let sender = ServiceWireSender::new(
            "caller_core",
            "caller_inst",
            None, // wildcard target: the discovery probe shape
            SenderTarget::node("camera", "v1").expect("node target"),
            "ping",
            ServiceKind::Service,
        )
        .expect("valid sender");

        let queryable = adapter
            .listen_service(&receiver)
            .await
            .expect("queryable declare should succeed");
        // Nobody drains `queryable.rx` — the producer is "busy".

        // Plain (empty-body) probe: empty Response-kind reply.
        let mut reply_stream = adapter
            .call_service(
                &sender,
                Payload::from_bytes(bytes::Bytes::new()),
                ServiceQueryKind::Probe,
                Some(std::time::Duration::from_millis(500)),
            )
            .await
            .expect("probe call should succeed");
        let reply = reply_stream
            .rx
            .recv()
            .await
            .expect("probe must be answered without the endpoint loop");
        assert_eq!(reply.kind(), ServiceReplyKind::Response);
        assert!(reply.message().payload().is_empty());

        // Benchmark sized probe: response carries the requested size.
        let mut reply_stream = adapter
            .call_service(
                &sender,
                Payload::from_bytes(crate::probe::build_sized_probe_request(64, 4096)),
                ServiceQueryKind::Probe,
                Some(std::time::Duration::from_millis(500)),
            )
            .await
            .expect("sized probe call should succeed");
        let reply = reply_stream
            .rx
            .recv()
            .await
            .expect("sized probe must be answered without the endpoint loop");
        assert_eq!(reply.kind(), ServiceReplyKind::Response);
        assert_eq!(reply.message().payload().len(), 4096);

        // Neither probe leaked into the endpoint channel.
        assert!(
            queryable.rx.try_recv().is_err(),
            "probes must never reach the endpoint channel"
        );
    }

    #[tokio::test]
    async fn mock_liveliness_token_lifecycle_drives_watch_and_probe() {
        // The mock must mirror Zenoh's liveliness semantics: a watch created
        // after the token exists replays an initial Alive (history), dropping
        // the token emits Gone, and the one-shot probe answers presence.
        use crate::wire::{ActionWireReceiver, ActionWireSender, SenderTarget};

        let mut adapter = MockAdapter::default();
        adapter.start_session().await.expect("session should start");

        let target = SenderTarget::node("arm", "v1").expect("node target");
        let receiver =
            ActionWireReceiver::new("server_core", "server_inst", target.clone(), "move")
                .expect("valid receiver");
        let sender = ActionWireSender::new(
            "caller_core",
            "caller_inst",
            Some(&config::runtime::ProducerRef::new(
                "server_core",
                "server_inst",
            )),
            target.clone(),
            "move",
        )
        .expect("valid sender");

        // Probe before any token: absent.
        let probe = adapter
            .probe_action_producer(&sender, std::time::Duration::from_secs(1))
            .await
            .expect("probe should issue");
        assert!(!probe.resolve().await, "no token declared yet");

        let token = adapter
            .declare_action_liveliness(&receiver)
            .await
            .expect("token should declare");

        // History: a watch declared after the token sees an initial Alive.
        let watch = adapter
            .watch_action_producer(&sender)
            .await
            .expect("watch should declare");
        assert_eq!(
            watch.rx.recv_async().await.expect("initial event"),
            LivelinessEvent::Alive(())
        );

        let probe = adapter
            .probe_action_producer(&sender, std::time::Duration::from_secs(1))
            .await
            .expect("probe should issue");
        assert!(probe.resolve().await, "token is alive");

        // Dropping the token is the producer-death signal.
        drop(token);
        assert_eq!(
            watch.rx.recv_async().await.expect("gone event"),
            LivelinessEvent::Gone(())
        );
        let probe = adapter
            .probe_action_producer(&sender, std::time::Duration::from_secs(1))
            .await
            .expect("probe should issue");
        assert!(!probe.resolve().await, "token is gone");
    }

    #[tokio::test]
    async fn mock_stop_start_session_isolates_liveliness_generations() {
        // Closing the session removes its tokens, reports Gone, and retires
        // the old watches and token guards before a new session starts.
        use crate::wire::{ActionWireReceiver, ActionWireSender, SenderTarget};

        let mut adapter = MockAdapter::default();
        adapter.start_session().await.expect("session should start");

        let target = SenderTarget::node("arm", "v1").expect("node target");
        let receiver =
            ActionWireReceiver::new("server_core", "server_inst", target.clone(), "move")
                .expect("valid receiver");
        let sender = ActionWireSender::new(
            "caller_core",
            "caller_inst",
            Some(&config::runtime::ProducerRef::new(
                "server_core",
                "server_inst",
            )),
            target,
            "move",
        )
        .expect("valid sender");

        let old_token = adapter
            .declare_action_liveliness(&receiver)
            .await
            .expect("token should declare");
        let old_watch = adapter
            .watch_action_producer(&sender)
            .await
            .expect("watch should declare");
        assert_eq!(
            old_watch.rx.recv_async().await.expect("initial event"),
            LivelinessEvent::Alive(())
        );

        adapter.stop_session().await.expect("session should stop");
        assert_eq!(
            old_watch.rx.recv_async().await.expect("gone event"),
            LivelinessEvent::Gone(())
        );
        assert!(
            matches!(
                old_watch.rx.try_recv(),
                Err(flume::TryRecvError::Disconnected)
            ),
            "the stopped session's watch should be retired"
        );

        adapter
            .start_session()
            .await
            .expect("session should restart");
        let new_watch = adapter
            .watch_action_producer(&sender)
            .await
            .expect("new watch should declare");
        let new_token = adapter
            .declare_action_liveliness(&receiver)
            .await
            .expect("new token should declare");
        assert_eq!(
            new_watch.rx.recv_async().await.expect("new alive event"),
            LivelinessEvent::Alive(())
        );

        drop(old_token);
        assert!(
            matches!(new_watch.rx.try_recv(), Err(flume::TryRecvError::Empty)),
            "an old token guard must not remove the restarted session's token"
        );
        let probe = adapter
            .probe_action_producer(&sender, std::time::Duration::from_secs(1))
            .await
            .expect("probe should issue");
        assert!(probe.resolve().await, "the new token should remain alive");

        drop(new_token);
        assert_eq!(
            new_watch.rx.recv_async().await.expect("new gone event"),
            LivelinessEvent::Gone(())
        );
    }

    fn seg(value: &str) -> Segment {
        Segment::try_from(value).expect("valid keyexpr segment")
    }

    /// Issues and collects a presence list (the mock answers instantly, so
    /// the timeout is irrelevant).
    async fn list_presence(
        adapter: &MockAdapter,
        core_node: Option<&Segment>,
    ) -> Vec<CoreNodePresence> {
        adapter
            .list_core_node_presence(
                PresenceScope::Session,
                core_node,
                std::time::Duration::from_secs(1),
            )
            .await
            .expect("presence list should issue")
            .collect()
            .await
            .expect("presence list should collect")
    }

    #[tokio::test]
    async fn mock_core_node_presence_lifecycle_drives_history_watch_and_list() {
        let mut adapter = MockAdapter::default();
        adapter.start_session().await.expect("session should start");

        let token = adapter
            .declare_core_node_presence(&seg("daemon_a"), &seg("generation_1"))
            .await
            .expect("presence token should declare");
        let watch = adapter
            .watch_core_node_presence(Some(&seg("daemon_a")))
            .await
            .expect("presence watch should declare");
        let expected = CoreNodePresence::new("daemon_a", "generation_1");
        assert_eq!(
            watch.rx.recv_async().await.expect("history event"),
            LivelinessEvent::Alive(expected.clone())
        );
        assert_eq!(list_presence(&adapter, None).await, vec![expected.clone()]);

        drop(token);
        assert_eq!(
            watch.rx.recv_async().await.expect("gone event"),
            LivelinessEvent::Gone(expected)
        );
        assert!(list_presence(&adapter, None).await.is_empty());
    }

    #[tokio::test]
    async fn mock_core_node_presence_list_filters_and_preserves_collision_shape() {
        let mut adapter = MockAdapter::default();
        adapter.start_session().await.expect("session should start");

        let _a1 = adapter
            .declare_core_node_presence(&seg("daemon_a"), &seg("generation_1"))
            .await
            .expect("first token should declare");
        let _a2 = adapter
            .declare_core_node_presence(&seg("daemon_a"), &seg("generation_2"))
            .await
            .expect("collision token should declare");
        let _b = adapter
            .declare_core_node_presence(&seg("daemon_b"), &seg("generation_3"))
            .await
            .expect("other token should declare");

        let mut all = list_presence(&adapter, None).await;
        all.sort();
        assert_eq!(
            all,
            vec![
                CoreNodePresence::new("daemon_a", "generation_1"),
                CoreNodePresence::new("daemon_a", "generation_2"),
                CoreNodePresence::new("daemon_b", "generation_3"),
            ]
        );

        let daemon_a_only = list_presence(&adapter, Some(&seg("daemon_a"))).await;
        assert_eq!(daemon_a_only.len(), 2, "both colliding tokens must remain");
        assert!(
            daemon_a_only
                .iter()
                .all(|presence| presence.core_node == "daemon_a")
        );
    }

    #[tokio::test]
    async fn mock_stop_session_reports_core_node_presence_gone() {
        let mut adapter = MockAdapter::default();
        adapter.start_session().await.expect("session should start");
        let _token = adapter
            .declare_core_node_presence(&seg("daemon_a"), &seg("generation_1"))
            .await
            .expect("presence token should declare");
        let watch = adapter
            .watch_core_node_presence(Some(&seg("daemon_a")))
            .await
            .expect("presence watch should declare");
        let expected = CoreNodePresence::new("daemon_a", "generation_1");
        assert_eq!(
            watch.rx.recv_async().await.expect("history event"),
            LivelinessEvent::Alive(expected.clone())
        );

        adapter.stop_session().await.expect("session should stop");
        assert_eq!(
            watch.rx.recv_async().await.expect("gone event"),
            LivelinessEvent::Gone(expected)
        );
    }

    #[tokio::test]
    async fn mock_topic_wildcard_subscriber_drops_secondary_publish() {
        // Mirrors the peppylib integration test against zenohd, but in
        // process. Two `publish_topic` calls with the same payload on
        // different link_ids — the first marked primary, the second
        // secondary — must deliver to a wildcard subscriber exactly once
        // (primary only) and to each pinned subscriber exactly once
        // (regardless of marker).
        use crate::wire::{SenderTarget, TopicWireReceiver, TopicWireSender};

        let mut adapter = MockAdapter::default();
        adapter.start_session().await.expect("session should start");

        let target = SenderTarget::contract("depth_camera", "v1").expect("contract target");

        let sender_left = TopicWireSender::new(
            "pub_core",
            "pub_inst",
            target.clone(),
            Some("wrist_left"),
            "frames",
        )
        .expect("sender left");
        let sender_right = TopicWireSender::new(
            "pub_core",
            "pub_inst",
            target.clone(),
            Some("wrist_right"),
            "frames",
        )
        .expect("sender right");

        let recv_any = TopicWireReceiver::new(
            "sub_core",
            "sub_any",
            None,
            None,
            Some(target.clone()),
            None,
            "frames",
        )
        .expect("recv any");
        let recv_left = TopicWireReceiver::new(
            "sub_core",
            "sub_left",
            None,
            None,
            Some(target.clone()),
            Some("wrist_left"),
            "frames",
        )
        .expect("recv left");
        let recv_right = TopicWireReceiver::new(
            "sub_core",
            "sub_right",
            None,
            None,
            Some(target.clone()),
            Some("wrist_right"),
            "frames",
        )
        .expect("recv right");

        let sub_any = adapter
            .subscribe_topic(&recv_any, SubscriberQoS::Standard)
            .await
            .expect("wildcard subscribe");
        let sub_left = adapter
            .subscribe_topic(&recv_left, SubscriberQoS::Standard)
            .await
            .expect("pinned left subscribe");
        let sub_right = adapter
            .subscribe_topic(&recv_right, SubscriberQoS::Standard)
            .await
            .expect("pinned right subscribe");

        let payload = || Payload::from_bytes(bytes::Bytes::from_static(b"frame-0"));

        adapter
            .publish_topic(&sender_left, payload(), PublisherQoS::Standard, true)
            .await
            .expect("primary publish");
        adapter
            .publish_topic(&sender_right, payload(), PublisherQoS::Standard, false)
            .await
            .expect("secondary publish");

        // Wildcard subscriber: receives the primary only.
        let first = sub_any
            .rx
            .recv_async()
            .await
            .expect("wildcard receives once");
        assert_eq!(first.payload().to_bytes().as_ref(), b"frame-0");
        // No second delivery in-process — the secondary was dropped.
        assert!(
            sub_any.rx.try_recv().is_err(),
            "wildcard subscriber must not receive a duplicate"
        );

        // Pinned subscribers each receive their one publish, regardless of
        // whether it was tagged primary or secondary.
        let left = sub_left
            .rx
            .recv_async()
            .await
            .expect("pinned left receives");
        assert_eq!(left.payload().to_bytes().as_ref(), b"frame-0");
        let right = sub_right
            .rx
            .recv_async()
            .await
            .expect("pinned right receives");
        assert_eq!(right.payload().to_bytes().as_ref(), b"frame-0");
    }

    #[tokio::test]
    async fn messenger_declare_topic_publisher_delivers_to_a_subscriber() {
        // Exercises the `Messenger` dispatch wrapper (not the adapter directly):
        // a pre-bound publisher must reach a subscriber on the same topic and be
        // marked primary so a wildcard subscriber keeps it.
        use crate::wire::{SenderTarget, TopicWireReceiver, TopicWireSender};

        let mut messenger = Messenger::new(MessengerAdapter::Mock(MockAdapter::default()));
        messenger
            .start_session()
            .await
            .expect("session should start");

        let target = SenderTarget::contract("depth_camera", "v1").expect("contract target");
        let sender = TopicWireSender::new(
            "pub_core",
            "pub_inst",
            target.clone(),
            Some("wrist"),
            "frames",
        )
        .expect("sender");
        let receiver = TopicWireReceiver::new(
            "sub_core",
            "sub_inst",
            None,
            None,
            Some(target),
            None,
            "frames",
        )
        .expect("receiver");

        let subscription = messenger
            .subscribe_topic(&receiver, SubscriberQoS::Standard)
            .await
            .expect("subscribe");

        let publisher = messenger
            .declare_topic_publisher(&sender, PublisherQoS::Standard)
            .expect("declare publisher");
        publisher
            .publish(bytes::Bytes::from_static(b"frame-0"))
            .await
            .expect("publish");

        let received = subscription
            .rx
            .recv_async()
            .await
            .expect("subscriber receives the published frame");
        assert_eq!(received.payload().to_bytes().as_ref(), b"frame-0");
    }

    #[tokio::test]
    async fn messenger_declare_action_feedback_publisher_reaches_goal_subscriber() {
        // The action-feedback dispatch wrapper: feedback pre-bound for a
        // specific (link_id, goal_id) must reach a consumer subscribed to that
        // producer's feedback for the same goal.
        use crate::wire::{ActionWireReceiver, ActionWireSender, SenderTarget};

        let mut messenger = Messenger::new(MessengerAdapter::Mock(MockAdapter::default()));
        messenger
            .start_session()
            .await
            .expect("session should start");

        let target = SenderTarget::node("arm", "v1").expect("node target");
        let receiver =
            ActionWireReceiver::new("server_core", "server_inst", target.clone(), "move")
                .expect("receiver");
        let sender = ActionWireSender::new(
            "caller_core",
            "caller_inst",
            Some(&config::runtime::ProducerRef::new(
                "server_core",
                "server_inst",
            )),
            target,
            "move",
        )
        .expect("sender");

        let subscription = messenger
            .subscribe_action_feedback(&sender, "g7", SubscriberQoS::Standard)
            .await
            .expect("subscribe feedback");

        let publisher = messenger
            .declare_action_feedback_publisher(&receiver, "_", "g7", PublisherQoS::Standard)
            .expect("declare feedback publisher");
        publisher
            .publish(bytes::Bytes::from_static(b"progress-50"))
            .await
            .expect("publish feedback");

        let received = subscription
            .rx
            .recv_async()
            .await
            .expect("feedback subscriber receives the goal update");
        assert_eq!(received.payload().to_bytes().as_ref(), b"progress-50");
    }
}
