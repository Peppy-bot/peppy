use super::super::error::{Error, Result};
use super::super::types::{Message, MessengerBackend, PublisherQoS, SubscriberQoS, Subscription};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

pub struct MockAdapter {
    pub is_session_connected: bool,
    pub is_router_started: bool,
    // Store published messages by topic
    pub messages: Arc<Mutex<HashMap<String, Vec<Message>>>>,
    // Store active subscriptions
    pub subscriptions: Arc<Mutex<HashMap<String, Vec<mpsc::Sender<Message>>>>>,
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
                topic: message.topic.clone(),
            });
        }

        // Store the message
        {
            let mut messages = self.messages.lock().unwrap();
            messages
                .entry(message.topic.clone())
                .or_default()
                .push(message.clone());
        }

        // Send to all matching subscribers (supports simple prefix wildcard ending with "/**")
        let senders = {
            let subscriptions = self.subscriptions.lock().unwrap();
            let mut matched = Vec::new();
            for (pattern, senders) in subscriptions.iter() {
                if Self::topic_matches(pattern, &message.topic) {
                    matched.extend(senders.iter().cloned());
                }
            }
            matched
        };

        for sender in senders {
            // Ignore send errors (subscriber might have dropped)
            let _ = sender.send(message.clone()).await;
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

        // Send any existing messages for this topic
        let existing_messages = {
            let messages = self.messages.lock().unwrap();
            let mut matched = Vec::new();
            for (msg_topic, msgs) in messages.iter() {
                if Self::topic_matches(topic, msg_topic) {
                    matched.extend(msgs.iter().cloned());
                }
            }
            matched
        };

        for msg in existing_messages {
            let _ = tx.send(msg).await;
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
}

impl MockAdapter {
    fn topic_matches(pattern: &str, topic: &str) -> bool {
        if let Some(prefix) = pattern.strip_suffix("/**") {
            if prefix.is_empty() {
                return true;
            }
            if topic == prefix {
                return true;
            }
            topic.starts_with(prefix)
                && topic.len() > prefix.len()
                && topic.as_bytes()[prefix.len()] == b'/'
        } else {
            pattern == topic
        }
    }
}

// Those tests purpose is to test the behaviour of a real messaging system and check if they map to the behaviour of the mock
#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MessengerAdapter, PublisherQoS, SubscriberQoS};
    use crate::{Message, Messenger, MessengerBackend};

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
        let test_message = Message::new("test_topic", &[1, 2, 3]);
        assert!(
            mock.publish(test_message, PublisherQoS::Standard)
                .await
                .is_ok()
        );

        // Verify message was stored
        {
            let messages = mock.messages.lock().unwrap();
            assert!(messages.contains_key("test_topic"));
            assert_eq!(messages.get("test_topic").unwrap().len(), 1);
        }

        // Test shutdown consumes self and succeeds when connected
        assert!(mock.stop_session().await.is_ok());
    }

    #[tokio::test]
    async fn test_mock_messenger_operations() {
        let mut messenger = create_test_messenger();

        // Test all operations succeed with MockAdapter
        assert!(messenger.start_session().await.is_ok());

        // Test subscribe first
        let subscription = messenger
            .subscribe("test/topic", SubscriberQoS::Standard)
            .await;
        assert!(subscription.is_ok());
        let mut subscription = subscription.unwrap();

        // Test publish
        let message = Message::new("test/topic", b"test payload");
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
        assert_eq!(received_msg.topic, message.topic);
        assert_eq!(received_msg.payload, message.payload);

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
            .subscribe("test/topic", SubscriberQoS::Standard)
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
            .subscribe("topic/1", SubscriberQoS::Standard)
            .await;
        let sub2 = messenger
            .subscribe("topic/2", SubscriberQoS::HighThroughput)
            .await;
        let sub3 = messenger
            .subscribe("topic/3", SubscriberQoS::HighThroughput)
            .await;

        assert!(sub1.is_ok());
        assert!(sub2.is_ok());
        assert!(sub3.is_ok());
    }

    #[tokio::test]
    async fn test_mock_messenger_publish_multiple_messages() {
        let mut messenger = create_test_messenger();

        // Test publishing before start_session should fail
        let early_message = Message::new("topic/early", b"too_early");
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
            Message::new("topic/1", b"payload1"),
            Message::new("topic/2", b"payload2"),
            Message::new("topic/3", b"payload3"),
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
}
