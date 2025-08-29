use super::super::super::error::{Error, Result};
use super::super::types::{Message, MessengerBackend, Subscription, ThroughputMode};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

pub struct MockAdapter {
    is_connected: bool,
    is_router_started: bool,
    // Store published messages by topic
    messages: Arc<Mutex<HashMap<String, Vec<Message>>>>,
    // Store active subscriptions
    subscriptions: Arc<Mutex<HashMap<String, Vec<mpsc::Sender<Message>>>>>,
}

impl Default for MockAdapter {
    fn default() -> Self {
        Self {
            is_connected: false,
            is_router_started: false,
            messages: Arc::new(Mutex::new(HashMap::new())),
            subscriptions: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl MessengerBackend for MockAdapter {
    async fn init(&mut self) -> Result<()> {
        self.is_router_started = true;
        self.is_connected = true;
        Ok(())
    }

    async fn publish(&mut self, message: Message) -> Result<()> {
        if !self.is_connected {
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

        // Send to all subscribers of this topic
        let senders = {
            let subscriptions = self.subscriptions.lock().unwrap();
            subscriptions.get(&message.topic).cloned()
        };

        if let Some(senders) = senders {
            for sender in senders {
                // Ignore send errors (subscriber might have dropped)
                let _ = sender.send(message.clone()).await;
            }
        }

        Ok(())
    }

    async fn subscribe(
        &self,
        topic: &str,
        throughput_mode: ThroughputMode,
    ) -> Result<Subscription> {
        if !self.is_connected {
            return Err(Error::SubscribeError {
                topic: topic.to_string(),
            });
        }

        let (tx, rx) = mpsc::channel(throughput_mode.channel_size());

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
            messages.get(topic).cloned()
        };

        if let Some(existing_messages) = existing_messages {
            for msg in existing_messages {
                let _ = tx.send(msg).await;
            }
        }

        // Create a dummy task that does nothing, just to get an abort handle
        // This maintains compatibility with the Subscription type
        let join_handle = tokio::spawn(async {});
        let abort_handle = join_handle.abort_handle();

        Ok(Subscription::new(rx, abort_handle))
    }

    async fn shutdown(mut self) -> Result<()> {
        if !self.is_connected {
            return Err(Error::ShutdownError);
        }

        self.is_connected = false;
        self.is_router_started = false;

        // Clear all data
        self.messages.lock().unwrap().clear();
        self.subscriptions.lock().unwrap().clear();

        Ok(())
    }
}

// Those tests purpose is to test the behaviour of a real messaging system and check if they map to the behaviour of the mock
#[cfg(test)]
mod tests {
    use crate::messaging::{Message, Messenger, MessengerBackend, ThroughputMode};
    use crate::types::MessagingEngineContext;

    fn create_test_messenger() -> Messenger {
        let context = MessagingEngineContext::new("mock".to_string(), None);
        Messenger::new(context).unwrap()
    }

    #[tokio::test]
    async fn test_build_messenger() {
        let mut messenger = create_test_messenger();

        // Must start router before connecting
        assert!(messenger.init().await.is_ok());
        assert!(messenger.shutdown().await.is_ok());
    }

    #[tokio::test]
    async fn test_shutdown_without_init_fails() {
        let messenger = create_test_messenger();

        // Shutdown should fail if init() hasn't been called
        assert!(messenger.shutdown().await.is_err());
    }

    #[tokio::test]
    async fn test_all_operations() {
        let mut messenger = create_test_messenger();

        // Test all operations succeed with MockAdapter
        assert!(messenger.init().await.is_ok());

        // Test subscribe first
        let subscription = messenger
            .subscribe("test/topic", ThroughputMode::LowThroughput)
            .await;
        assert!(subscription.is_ok());
        let mut subscription = subscription.unwrap();

        // Test publish
        let message = Message::new("test/topic", b"test payload");
        assert!(messenger.publish(message.clone()).await.is_ok());

        // Verify subscription receives the published message
        let received = subscription.rx.recv().await;
        assert!(received.is_some());
        let received_msg = received.unwrap();
        assert_eq!(received_msg.topic, message.topic);
        assert_eq!(received_msg.payload, message.payload);

        // Test shutdown
        assert!(messenger.shutdown().await.is_ok());
    }

    #[tokio::test]
    async fn test_subscription_returns_valid_channel() {
        let mut messenger = create_test_messenger();

        // Must start router and connect first
        assert!(messenger.init().await.is_ok());

        // Test that subscription returns a valid channel
        let mut subscription = messenger
            .subscribe("test/topic", ThroughputMode::LowThroughput)
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
    async fn test_multiple_subscriptions() {
        let mut messenger = create_test_messenger();

        // Must start router and connect first
        assert!(messenger.init().await.is_ok());

        // Test multiple subscriptions to different topics
        let sub1 = messenger
            .subscribe("topic/1", ThroughputMode::LowThroughput)
            .await;
        let sub2 = messenger
            .subscribe("topic/2", ThroughputMode::HighThroughput)
            .await;
        let sub3 = messenger
            .subscribe("topic/3", ThroughputMode::LowThroughput)
            .await;

        assert!(sub1.is_ok());
        assert!(sub2.is_ok());
        assert!(sub3.is_ok());
    }

    #[tokio::test]
    async fn test_publish_multiple_messages() {
        let mut messenger = create_test_messenger();

        // Must start router and connect first
        assert!(messenger.init().await.is_ok());

        // Test publishing multiple messages
        let messages = vec![
            Message::new("topic/1", b"payload1"),
            Message::new("topic/2", b"payload2"),
            Message::new("topic/3", b"payload3"),
        ];

        for message in messages {
            assert!(messenger.publish(message).await.is_ok());
        }
    }

    #[tokio::test]
    async fn test_factory_creates_mock_correctly() {
        // Test that factory correctly creates MockAdapter when Mock engine is specified
        let mut messenger = create_test_messenger();

        // These should all succeed if MockAdapter was created properly
        assert!(messenger.init().await.is_ok());
        assert!(messenger.shutdown().await.is_ok());
    }
}
