use super::super::error::{Error, Result};
use super::super::types::{
    Message, Messenger, MessengerAdapter, MessengerBackend, PublisherQoS, SubscriberQoS,
    Subscription, TopicMessage,
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

pub struct MockAdapter {
    pub is_session_connected: bool,
    pub is_router_started: bool,
    // Store published messages by topic
    pub messages: Arc<Mutex<HashMap<String, Vec<Message>>>>,
    // Store active subscriptions
    pub subscriptions: Arc<Mutex<HashMap<String, Vec<mpsc::Sender<TopicMessage>>>>>,
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

        // Clear all data
        self.messages.lock().unwrap().clear();
        self.subscriptions.lock().unwrap().clear();

        Ok(())
    }

    async fn publish(&mut self, message: Message, _qos: PublisherQoS) -> Result<()> {
        if !self.is_session_connected {
            return Err(Error::PublishError {
                topic: message.identifier().to_string(),
            });
        }

        // Ensure we can convert the message before storing/sending
        Self::to_response_message(&message)?;
        let topic = message.identifier().to_string();

        // Store the message
        {
            let mut messages = self.messages.lock().unwrap();
            messages
                .entry(topic.clone())
                .or_default()
                .push(message.clone());
        }

        // Send to all matching subscribers (supports simple prefix wildcard ending with "/**")
        let senders = {
            let subscriptions = self.subscriptions.lock().unwrap();
            let mut matched = Vec::new();
            for (pattern, senders) in subscriptions.iter() {
                if Self::topic_matches(pattern, &topic) {
                    matched.extend(senders.iter().cloned());
                }
            }
            matched
        };

        for sender in senders {
            // Ignore send errors (subscriber might have dropped)
            let _ = sender.send(Self::to_response_message(&message)?).await;
        }

        Ok(())
    }

    async fn subscribe(&self, topic: &str, qos: SubscriberQoS) -> Result<Subscription> {
        if !self.is_session_connected {
            return Err(Error::SubscribeError {
                topic: topic.to_string(),
            });
        }

        let (tx, rx) = mpsc::channel(qos.channel_size());

        // Store the sender for this subscription
        {
            let mut subscriptions = self.subscriptions.lock().unwrap();
            subscriptions
                .entry(topic.to_string())
                .or_default()
                .push(tx.clone());
        }

        // Create a dummy task that does nothing, just to get an abort handle
        // This maintains compatibility with the Subscription type
        let join_handle = tokio::spawn(async {});
        let abort_handle = join_handle.abort_handle();

        Ok(Subscription::new(rx, abort_handle))
    }

    async fn has_matching_subscribers(&self, topic: &str) -> Result<bool> {
        if !self.is_session_connected {
            return Err(Error::MessagingSessionError(
                "Session not initialized".to_string(),
            ));
        }

        let subscriptions = self.subscriptions.lock().unwrap();
        let has_match = subscriptions.iter().any(|(pattern, senders)| {
            Self::topic_matches(pattern, topic) && senders.iter().any(|sender| !sender.is_closed())
        });
        Ok(has_match)
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

    /// Matches a topic against a pattern using Zenoh key expression semantics.
    ///
    /// Zenoh wildcards:
    /// - `*` matches exactly one chunk (non-empty sequence not containing `/`)
    /// - `**` matches any number of chunks (including zero)
    fn topic_matches(pattern: &str, topic: &str) -> bool {
        let pattern_chunks: Vec<&str> = pattern.split('/').collect();
        let topic_chunks: Vec<&str> = topic.split('/').collect();

        Self::match_chunks(&pattern_chunks, &topic_chunks)
    }

    fn match_chunks(pattern: &[&str], topic: &[&str]) -> bool {
        let mut p_idx = 0;
        let mut t_idx = 0;

        while p_idx < pattern.len() {
            let p_chunk = pattern[p_idx];

            if p_chunk == "**" {
                // ** matches zero or more chunks
                // Try matching the rest of the pattern against all possible positions
                if p_idx == pattern.len() - 1 {
                    // ** at the end matches everything remaining
                    return true;
                }

                // Try matching the rest of the pattern at each position
                for try_t_idx in t_idx..=topic.len() {
                    if Self::match_chunks(&pattern[p_idx + 1..], &topic[try_t_idx..]) {
                        return true;
                    }
                }
                return false;
            }

            // No more topic chunks but pattern has more non-** chunks
            if t_idx >= topic.len() {
                return false;
            }

            let t_chunk = topic[t_idx];

            if p_chunk == "*" {
                // * matches exactly one non-empty chunk
                if t_chunk.is_empty() {
                    return false;
                }
                // Match succeeds, advance both
            } else if p_chunk != t_chunk {
                // Literal chunks must match exactly
                return false;
            }

            p_idx += 1;
            t_idx += 1;
        }

        // Pattern exhausted - topic must also be exhausted
        t_idx == topic.len()
    }

    fn to_response_message(message: &Message) -> Result<TopicMessage> {
        let identifier = message.identifier();
        TopicMessage::new(identifier, message.payload().clone())
    }
}

// Those tests purpose is to test the behaviour of a real messaging system and check if they map to the behaviour of the mock
#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;
    use crate::types::{MessengerAdapter, PublisherQoS, SubscriberQoS};
    use crate::{Message, Messenger, MessengerBackend};

    // Key expression format: target_master/caller_master/target_instance/caller_instance/...
    // Instance ID is extracted from segment index 3 (caller_instance)
    // Master node is extracted from segment index 1 (caller_master)
    const INSTANCE_ID: &str = "test_instance";
    const MASTER_NODE: &str = "test_master";

    /// Creates a valid key expression with the expected format for TopicMessage
    fn make_key_expr(topic: &str) -> String {
        // Format: target_master/caller_master/target_instance/caller_instance/topic
        // Segments: 0=target_master, 1=caller_master, 2=target_instance, 3=caller_instance
        // TopicMessage extracts: instance_id from index 3, master_node from index 1
        format!(
            "target_master/{}/target_instance/{}/{}",
            MASTER_NODE, INSTANCE_ID, topic
        )
    }

    fn create_test_messenger() -> Messenger {
        let adapter = MockAdapter::default();
        Messenger::new(MessengerAdapter::Mock(adapter))
    }

    #[tokio::test]
    async fn test_build_mock_messenger() {
        use super::MockAdapter;

        let mut mock = MockAdapter::default();

        // Initially, nothing should be started or connected
        assert!(!mock.is_router_started);
        assert!(!mock.is_session_connected);

        // Test start_router - should set is_router_started but not is_connected
        assert!(mock.start_router().await.is_ok());
        assert!(mock.is_router_started);
        assert!(!mock.is_session_connected);

        // Test stop_router - should clear is_router_started
        assert!(mock.stop_router().await.is_ok());
        assert!(!mock.is_router_started);
        assert!(!mock.is_session_connected);

        // Start router again for the rest of the test
        assert!(mock.start_router().await.is_ok());
        assert!(mock.is_router_started);

        // Test init - should set is_connected
        assert!(mock.start_session().await.is_ok());
        assert!(mock.is_router_started);
        assert!(mock.is_session_connected);

        // Test publish - should work when connected
        let key = make_key_expr("test_topic");
        let test_message = Message::new(&key, [1, 2, 3]);
        assert!(
            mock.publish(test_message, PublisherQoS::Standard)
                .await
                .is_ok()
        );

        // Verify message was stored
        {
            let messages = mock.messages.lock().unwrap();
            assert!(messages.contains_key(&key));
            assert_eq!(messages.get(&key).unwrap().len(), 1);
        }

        // Test shutdown consumes self and succeeds when connected
        assert!(mock.stop_session().await.is_ok());
    }

    #[tokio::test]
    async fn test_mock_messenger_operations() {
        let mut messenger = create_test_messenger();

        // Test all operations succeed with MockAdapter
        assert!(messenger.start_session().await.is_ok());

        // Test subscribe first - pattern must match the key format from make_key_expr
        // make_key_expr("test/topic/data") = "target_master/test_master/target_instance/test_instance/test/topic/data"
        // Pattern uses wildcards for first 4 segments and matches rest literally
        let subscription = messenger
            .subscribe("*/*/*/*/test/topic/**", SubscriberQoS::Standard)
            .await;
        assert!(subscription.is_ok());
        let mut subscription = subscription.unwrap();

        // Test publish - use key expression format expected by TopicMessage
        let key = make_key_expr("test/topic/data");
        let message = Message::new(&key, b"test payload");
        assert!(
            messenger
                .publish(message.clone(), PublisherQoS::Standard)
                .await
                .is_ok()
        );

        // Verify subscription receives the published message
        let received = subscription.rx.recv().await;
        assert!(received.is_some());
        let received_msg = received.unwrap();
        assert_eq!(received_msg.instance_id(), INSTANCE_ID);
        assert_eq!(received_msg.master_node(), MASTER_NODE);
        assert_eq!(received_msg.payload(), message.payload());

        // Test shutdown
        assert!(messenger.stop_session().await.is_ok());
    }

    #[tokio::test]
    async fn test_mock_messenger_subscription_returns_valid_channel() {
        let mut messenger = create_test_messenger();

        // Must start router and connect first
        assert!(messenger.start_session().await.is_ok());

        // Test that subscription returns a valid channel
        let mut subscription = messenger
            .subscribe("test/topic/**", SubscriberQoS::Standard)
            .await
            .unwrap();

        // The receiver should be created even if no messages are sent
        // Try to receive with a timeout to check if channel is empty
        let result =
            tokio::time::timeout(std::time::Duration::from_millis(10), subscription.rx.recv())
                .await;
        assert!(result.is_err()); // Should timeout since no messages
    }

    #[tokio::test]
    async fn test_mock_messenger_multiple_subscriptions() {
        let mut messenger = create_test_messenger();

        // Must start router and connect first
        assert!(messenger.start_session().await.is_ok());

        // Test multiple subscriptions to different topics
        let sub1 = messenger
            .subscribe("topic/1/**", SubscriberQoS::Standard)
            .await;
        let sub2 = messenger
            .subscribe("topic/2/**", SubscriberQoS::HighThroughput)
            .await;
        let sub3 = messenger
            .subscribe("topic/3/**", SubscriberQoS::HighThroughput)
            .await;

        assert!(sub1.is_ok());
        assert!(sub2.is_ok());
        assert!(sub3.is_ok());
    }

    #[tokio::test]
    async fn test_mock_messenger_publish_multiple_messages() {
        let mut messenger = create_test_messenger();

        // Test publishing before start_session should fail
        let early_message = Message::new(&make_key_expr("topic/early"), b"too_early");
        assert!(
            messenger
                .publish(early_message, PublisherQoS::Standard)
                .await
                .is_err()
        );

        // Must start router and connect first
        assert!(messenger.start_session().await.is_ok());

        // Test publishing multiple messages
        let messages = vec![
            Message::new(&make_key_expr("topic/1"), Bytes::from_static(b"payload1")),
            Message::new(&make_key_expr("topic/2"), Bytes::from_static(b"payload2")),
            Message::new(&make_key_expr("topic/3"), Bytes::from_static(b"payload3")),
        ];

        for message in messages {
            assert!(
                messenger
                    .publish(message, PublisherQoS::Standard)
                    .await
                    .is_ok()
            );
        }
    }

    #[tokio::test]
    async fn test_mock_messenger_late_subscription_only_receives_new_messages() {
        let mut messenger = create_test_messenger();

        assert!(messenger.start_session().await.is_ok());

        // Publish before any subscription exists
        let early_msg = Message::new(&make_key_expr("test/topic/live"), b"early");
        assert!(
            messenger
                .publish(early_msg, PublisherQoS::Standard)
                .await
                .is_ok()
        );

        // Subscribe after the first publish; should not receive the earlier message
        let mut subscription = messenger
            .subscribe("*/*/*/*/test/topic/**", SubscriberQoS::Standard)
            .await
            .expect("subscription should succeed");

        let result =
            tokio::time::timeout(std::time::Duration::from_millis(20), subscription.rx.recv())
                .await;
        assert!(
            result.is_err(),
            "late subscription should not replay history"
        );

        // Publish a new message that matches the subscription
        let live_msg = Message::new(&make_key_expr("test/topic/live"), b"live");
        assert!(
            messenger
                .publish(live_msg.clone(), PublisherQoS::Standard)
                .await
                .is_ok()
        );

        let received =
            tokio::time::timeout(std::time::Duration::from_millis(50), subscription.rx.recv())
                .await
                .expect("should receive live message")
                .expect("channel should deliver a message");
        assert_eq!(received.payload(), live_msg.payload());

        assert!(messenger.stop_session().await.is_ok());
    }

    #[test]
    fn test_topic_matches_exact() {
        assert!(MockAdapter::topic_matches("a/b/c", "a/b/c"));
        assert!(!MockAdapter::topic_matches("a/b/c", "a/b/d"));
        assert!(!MockAdapter::topic_matches("a/b/c", "a/b"));
        assert!(!MockAdapter::topic_matches("a/b", "a/b/c"));
    }

    #[test]
    fn test_topic_matches_single_wildcard() {
        // * matches exactly one chunk
        assert!(MockAdapter::topic_matches("a/*/c", "a/b/c"));
        assert!(MockAdapter::topic_matches("a/*/c", "a/xyz/c"));
        assert!(!MockAdapter::topic_matches("a/*/c", "a/b/d"));
        assert!(!MockAdapter::topic_matches("a/*/c", "a/b/c/d"));
        assert!(MockAdapter::topic_matches("*/b/c", "a/b/c"));
        assert!(MockAdapter::topic_matches("a/b/*", "a/b/c"));
        assert!(MockAdapter::topic_matches("*/*/*/*", "a/b/c/d"));
    }

    #[test]
    fn test_topic_matches_double_wildcard() {
        // ** matches zero or more chunks
        assert!(MockAdapter::topic_matches("a/**", "a"));
        assert!(MockAdapter::topic_matches("a/**", "a/b"));
        assert!(MockAdapter::topic_matches("a/**", "a/b/c"));
        assert!(MockAdapter::topic_matches("a/**", "a/b/c/d"));
        assert!(!MockAdapter::topic_matches("a/**", "b"));
        assert!(!MockAdapter::topic_matches("a/**", "b/a"));
        assert!(MockAdapter::topic_matches("**", "a"));
        assert!(MockAdapter::topic_matches("**", "a/b/c"));
    }

    #[test]
    fn test_topic_matches_mixed_wildcards() {
        // Combination of * and **
        assert!(MockAdapter::topic_matches("a/*/c/**", "a/b/c"));
        assert!(MockAdapter::topic_matches("a/*/c/**", "a/b/c/d"));
        assert!(MockAdapter::topic_matches("a/*/c/**", "a/b/c/d/e"));
        assert!(!MockAdapter::topic_matches("a/*/c/**", "a/b/d"));
        assert!(MockAdapter::topic_matches(
            "*/*/service/**",
            "master/caller/service/ping/request/123"
        ));
    }

    #[test]
    fn test_topic_matches_service_patterns() {
        // Real patterns from the service messenger
        // Subscription pattern: {bound_master_node}/*/{as_instance_id}/*/{service_root}/request/**
        // Request topic: {target_master}/{caller_master}/{target_instance}/{caller_instance}/{service_root}/request/{request_id}

        // Pattern 1: Specific master, specific instance
        // Service bound to master "listener_master" with instance "listener_instance"
        let pattern = "listener_master/*/listener_instance/*/service/node/ping/request/**";
        // Request targeting the specific instance
        let topic = "listener_master/caller_master/listener_instance/caller_instance/service/node/ping/request/12345";
        assert!(MockAdapter::topic_matches(pattern, topic));

        // Pattern 3: Broadcast master (_any_), specific instance
        let pattern = "_any_/*/listener_instance/*/service/node/ping/request/**";
        let topic =
            "_any_/caller_master/listener_instance/caller_instance/service/node/ping/request/12345";
        assert!(MockAdapter::topic_matches(pattern, topic));

        // Pattern 4: Broadcast master, broadcast instance
        let pattern = "_any_/*/_any_/*/service/node/ping/request/**";
        let topic = "_any_/caller_master/_any_/caller_instance/service/node/ping/request/12345";
        assert!(MockAdapter::topic_matches(pattern, topic));

        // MasterNode uses its own name as the bound master (e.g., "master_node")
        // This allows targeted requests to reach the master node specifically
        let pattern = "master_node/*/listener_instance/*/service/node/ping/request/**";
        let topic = "master_node/caller_master/listener_instance/caller_instance/service/node/ping/request/12345";
        assert!(MockAdapter::topic_matches(pattern, topic));
    }
}
