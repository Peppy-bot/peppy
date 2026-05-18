use super::super::error::{Error, Result};
use super::super::types::{
    Message, Messenger, MessengerAdapter, MessengerBackend, Payload, PublisherQoS, SubscriberQoS,
    Subscription, TopicMessage,
};
use super::super::wire::zenoh_format::ZenohWireFormat;
use super::super::wire::{
    ActionWireReceiver, ActionWireSender, ServiceWireReceiver, ServiceWireSender,
    TopicWireReceiver, TopicWireSender,
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

/// Shared map of active subscriptions, keyed by pattern. Each pattern maps to
/// the senders that should receive a fanout when an intersecting topic is
/// published.
type SubscriptionMap = Arc<Mutex<HashMap<String, Vec<mpsc::Sender<TopicMessage>>>>>;

pub struct MockAdapter {
    pub is_session_connected: bool,
    pub is_router_started: bool,
    pub messages: MessageLog,
    pub subscriptions: SubscriptionMap,
}

impl Default for MockAdapter {
    fn default() -> Self {
        Self {
            is_session_connected: false,
            is_router_started: false,
            messages: Arc::new(Mutex::new(HashMap::new())),
            subscriptions: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl MessengerBackend for MockAdapter {
    async fn start_session(&mut self) -> Result<()> {
        self.is_session_connected = true;
        Ok(())
    }

    async fn stop_session(mut self) -> Result<()> {
        if !self.is_session_connected {
            return Err(Error::ShutdownError);
        }

        self.is_session_connected = false;
        self.is_router_started = false;

        self.messages.lock().unwrap().clear();
        self.subscriptions.lock().unwrap().clear();

        Ok(())
    }

    async fn subscribe_topic(
        &self,
        recv: &TopicWireReceiver,
        qos: SubscriberQoS,
    ) -> Result<Subscription> {
        self.subscribe_keyexpr(&ZenohWireFormat::topic_subscribe(recv), qos)
            .await
    }

    async fn publish_topic(
        &mut self,
        sender: &TopicWireSender,
        payload: Payload,
        _qos: PublisherQoS,
    ) -> Result<()> {
        self.publish_keyexpr(ZenohWireFormat::topic_publish(sender), payload)
            .await
    }

    async fn listen_service(&self, recv: &ServiceWireReceiver) -> Result<[Subscription; 4]> {
        let [p0, p1, p2, p3] = ZenohWireFormat::service_listen_patterns(recv);
        let s0 = self.subscribe_keyexpr(&p0, SubscriberQoS::Standard).await?;
        let s1 = self.subscribe_keyexpr(&p1, SubscriberQoS::Standard).await?;
        let s2 = self.subscribe_keyexpr(&p2, SubscriberQoS::Standard).await?;
        let s3 = self.subscribe_keyexpr(&p3, SubscriberQoS::Standard).await?;
        Ok([s0, s1, s2, s3])
    }

    async fn open_service_call(
        &mut self,
        sender: &ServiceWireSender,
        request_id: &str,
        payload: Payload,
    ) -> Result<Subscription> {
        let response_keyexpr = ZenohWireFormat::service_response_subscribe(sender, request_id);
        let response_sub = self
            .subscribe_keyexpr(&response_keyexpr, SubscriberQoS::Standard)
            .await?;
        let request_keyexpr = ZenohWireFormat::service_request_publish(sender, request_id);
        self.publish_keyexpr(request_keyexpr, payload).await?;
        Ok(response_sub)
    }

    async fn publish_service_response(
        &mut self,
        recv: &ServiceWireReceiver,
        received_request: &str,
        payload: Payload,
    ) -> Result<()> {
        let parsed = ZenohWireFormat::parse_received_request(recv, received_request)?;
        self.publish_keyexpr(parsed.response_keyexpr, payload).await
    }

    fn parse_service_request_id(
        &self,
        recv: &ServiceWireReceiver,
        received_request: &str,
    ) -> Result<String> {
        let parsed = ZenohWireFormat::parse_received_request(recv, received_request)?;
        Ok(parsed.request_id)
    }

    async fn subscribe_action_feedback(
        &self,
        sender: &ActionWireSender,
        goal_id: &str,
        qos: SubscriberQoS,
    ) -> Result<Subscription> {
        self.subscribe_keyexpr(
            &ZenohWireFormat::action_feedback_subscribe(sender, goal_id),
            qos,
        )
        .await
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

impl MockAdapter {
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
        goal_id: &str,
        _qos: PublisherQoS,
    ) -> MockPublisher {
        self.declare_publisher_keyexpr(ZenohWireFormat::action_feedback_publish(recv, goal_id))
    }

    fn declare_publisher_keyexpr(&self, topic: String) -> MockPublisher {
        MockPublisher {
            topic,
            subscriptions: Arc::clone(&self.subscriptions),
            messages: Arc::clone(&self.messages),
        }
    }

    async fn publish_keyexpr(&self, topic: String, payload: Payload) -> Result<()> {
        if !self.is_session_connected {
            return Err(Error::PublishError { topic });
        }

        let message = Message::new(&topic, payload.to_bytes());
        Self::route_publish(&topic, &message, &self.messages, &self.subscriptions).await
    }

    /// Records `message` against `topic` in the mock's message log and fans
    /// it out to every subscription whose pattern intersects `topic`. Shared
    /// by [`MockAdapter::publish_keyexpr`] (which holds the adapter
    /// directly) and [`MockPublisher::publish`] (which clones the same
    /// `Arc`s for lock-free per-topic publishing).
    async fn route_publish(
        topic: &str,
        message: &Message,
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

        let senders = {
            let subscriptions = subscriptions.lock().unwrap();
            let mut matched = Vec::new();
            for (pattern, senders) in subscriptions.iter() {
                if Self::key_exprs_intersect(pattern, topic) {
                    matched.extend(senders.iter().cloned());
                }
            }
            matched
        };

        for sender in senders {
            let _ = sender.send(response.clone()).await;
        }
        Ok(())
    }

    async fn subscribe_keyexpr(&self, topic: &str, qos: SubscriberQoS) -> Result<Subscription> {
        if !self.is_session_connected {
            return Err(Error::SubscribeError {
                topic: topic.to_string(),
            });
        }

        let (tx, rx) = mpsc::channel(qos.channel_size());

        {
            let mut subscriptions = self.subscriptions.lock().unwrap();
            subscriptions
                .entry(topic.to_string())
                .or_default()
                .push(tx.clone());
        }

        // No background task is needed — the mock writes directly into the
        // sender from `publish_keyexpr`. The dummy task gives us an abort
        // handle to satisfy the `Subscription` type.
        let join_handle = tokio::spawn(async {});
        let abort_handle = join_handle.abort_handle();

        Ok(Subscription::new(rx, abort_handle))
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
        let message = Message::new(&self.topic, payload);
        MockAdapter::route_publish(&self.topic, &message, &self.messages, &self.subscriptions).await
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
        // Request topic: {to_core_node}/{caller_core_node}/{to_instance}/{caller_instance}/{service_root}/request/{request_id}

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
        // The exact wire shape that motivated bidirectional matching:
        // `emit_topic_message` hard-codes `*` into caller-identity slots, while a
        // subscriber identifies itself with concrete core/instance values. Both
        // sides must intersect or topic delivery against the mock breaks.
        let publisher = "*/core_node/*/responder_inst/topic/clock/clock";
        let subscriber = "caller_core/core_node/caller_inst/responder_inst/topic/clock/clock";
        assert!(MockAdapter::key_exprs_intersect(subscriber, publisher));
        assert!(MockAdapter::key_exprs_intersect(publisher, subscriber));
    }
}
