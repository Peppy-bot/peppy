use super::super::types::{Message, MessengerBackend, Subscription};
use crate::{Error, Result};
use async_trait::async_trait;
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

impl MockAdapter {
    fn is_connected(&self) -> bool {
        self.is_connected
    }

    fn is_router_started(&self) -> bool {
        self.is_router_started
    }

    fn get_messages(&self, topic: &str) -> Vec<Message> {
        self.messages
            .lock()
            .unwrap()
            .get(topic)
            .cloned()
            .unwrap_or_default()
    }
}

#[async_trait]
impl MessengerBackend for MockAdapter {
    async fn start_router(&mut self) -> Result<()> {
        self.is_router_started = true;
        Ok(())
    }

    async fn connect(&mut self) -> Result<()> {
        if !self.is_router_started {
            return Err(Error::ConnectionError);
        }
        self.is_connected = true;
        Ok(())
    }

    async fn publish(&self, message: Message) -> Result<()> {
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
                .or_insert_with(Vec::new)
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

    async fn subscribe(&self, topic: &str) -> Result<Subscription> {
        if !self.is_connected {
            return Err(Error::SubscribeError {
                topic: topic.to_string(),
            });
        }

        let (tx, rx) = mpsc::channel(128);

        // Store the sender for this subscription
        {
            let mut subscriptions = self.subscriptions.lock().unwrap();
            subscriptions
                .entry(topic.to_string())
                .or_insert_with(Vec::new)
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

        Ok(Subscription { rx })
    }

    async fn shutdown(&mut self) -> Result<()> {
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
    use crate::commands::serve::messaging::{
        Engine, Message, MessagingConfiguration, Messenger, MessengerBackend,
    };

    #[tokio::test]
    async fn test_build_messenger() {
        let config = MessagingConfiguration::new("localhost", 7447).with_engine(Engine::Mock);
        let mut messenger = Messenger::from_config(config);

        // Must start router before connecting
        assert!(messenger.start_router().await.is_ok());
        assert!(messenger.connect().await.is_ok());
        assert!(messenger.shutdown().await.is_ok());
    }

    #[tokio::test]
    async fn test_fail_build_messenger() {
        let config = MessagingConfiguration::new("localhost", 7447).with_engine(Engine::Mock);
        let mut messenger = Messenger::from_config(config);

        // Attempt to connect before starting the router
        assert!(!messenger.connect().await.is_ok());
        assert!(messenger.start_router().await.is_ok());
        assert!(!messenger.shutdown().await.is_ok());
    }

    #[tokio::test]
    async fn test_all_operations() {
        let config = MessagingConfiguration::new("localhost", 8080).with_engine(Engine::Mock);
        let mut messenger = Messenger::from_config(config);

        // Test all operations succeed with MockAdapter
        assert!(messenger.start_router().await.is_ok());
        assert!(messenger.connect().await.is_ok());

        // Test publish
        let message = Message::new("test/topic", b"test payload");
        assert!(messenger.publish(message).await.is_ok());

        // Test subscribe
        let subscription = messenger.subscribe("test/topic").await;
        assert!(subscription.is_ok());

        // Test shutdown
        assert!(messenger.shutdown().await.is_ok());
    }

    #[tokio::test]
    async fn test_different_configurations() {
        // Test MockAdapter with different host and port configurations
        let configs = vec![
            MessagingConfiguration::new("192.168.1.1", 9999).with_engine(Engine::Mock),
            MessagingConfiguration::new("example.com", 443).with_engine(Engine::Mock),
            MessagingConfiguration::new("0.0.0.0", 0).with_engine(Engine::Mock),
            MessagingConfiguration::new("localhost", 65535).with_engine(Engine::Mock),
        ];

        for config in configs {
            let mut messenger = Messenger::from_config(config);
            // Mock should always succeed regardless of configuration
            assert!(messenger.start_router().await.is_ok());
            assert!(messenger.connect().await.is_ok());
        }
    }

    #[tokio::test]
    async fn test_subscription_returns_valid_channel() {
        let config = MessagingConfiguration::new("localhost", 7447).with_engine(Engine::Mock);
        let mut messenger = Messenger::from_config(config);

        // Must start router and connect first
        assert!(messenger.start_router().await.is_ok());
        assert!(messenger.connect().await.is_ok());

        // Test that subscription returns a valid channel
        let mut subscription = messenger.subscribe("test/topic").await.unwrap();

        // The receiver should be created even if no messages are sent
        // Try to receive with a timeout to check if channel is empty
        let result =
            tokio::time::timeout(std::time::Duration::from_millis(10), subscription.rx.recv())
                .await;
        assert!(result.is_err()); // Should timeout since no messages
    }

    #[tokio::test]
    async fn test_multiple_subscriptions() {
        let config = MessagingConfiguration::new("localhost", 7447).with_engine(Engine::Mock);
        let mut messenger = Messenger::from_config(config);

        // Must start router and connect first
        assert!(messenger.start_router().await.is_ok());
        assert!(messenger.connect().await.is_ok());

        // Test multiple subscriptions to different topics
        let sub1 = messenger.subscribe("topic/1").await;
        let sub2 = messenger.subscribe("topic/2").await;
        let sub3 = messenger.subscribe("topic/3").await;

        assert!(sub1.is_ok());
        assert!(sub2.is_ok());
        assert!(sub3.is_ok());
    }

    #[tokio::test]
    async fn test_publish_multiple_messages() {
        let config = MessagingConfiguration::new("localhost", 7447).with_engine(Engine::Mock);
        let mut messenger = Messenger::from_config(config);

        // Must start router and connect first
        assert!(messenger.start_router().await.is_ok());
        assert!(messenger.connect().await.is_ok());

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
        let config = MessagingConfiguration::new("test.local", 12345).with_engine(Engine::Mock);
        let mut messenger = Messenger::from_config(config);

        // These should all succeed if MockAdapter was created properly
        assert!(messenger.start_router().await.is_ok());
        assert!(messenger.connect().await.is_ok());
        assert!(messenger.shutdown().await.is_ok());
    }
}
