#[cfg(feature = "zenoh")]
mod zenoh_tests {
    use pmi::MessagingEngineContext;
    use pmi::{Message, Messenger, MessengerBackend, PublisherQoS, SubscriberQoS};

    /// Helper function to create a configured messenger with a unique port
    async fn create_test_messenger() -> (Messenger, tempfile::TempDir) {
        use std::sync::atomic::{AtomicU32, Ordering};

        // Use atomic counter for unique port allocation across parallel tests
        static PORT_COUNTER: AtomicU32 = AtomicU32::new(0);

        // Try up to 10 times to find an available port
        for _ in 0..10 {
            let counter = PORT_COUNTER.fetch_add(1, Ordering::SeqCst);

            // Use a wider port range to reduce collisions
            // Each test gets a port spaced by 10 to avoid conflicts
            let port = 20000 + (counter * 10);

            // Ensure we don't exceed valid port range
            if port > 60000 {
                PORT_COUNTER.store(0, Ordering::SeqCst);
                continue;
            }

            let config_content = format!(
                r#"{{
                    "listen": {{
                        "endpoints": {{
                            "router": ["tcp/127.0.0.1:{}"]
                        }}
                    }}
                }}"#,
                port
            );

            // Create a unique temporary directory for each test
            let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");

            // Use a unique config filename within the temp directory
            let config_filename = format!("test_zenoh_config_{}.json5", port);
            let config_path = temp_dir.path().join(config_filename);
            std::fs::write(&config_path, config_content).expect("Failed to write test config");

            let context = MessagingEngineContext::new("zenoh".to_string(), Some(config_path));
            match Messenger::new(context) {
                Ok(messenger) => return (messenger, temp_dir),
                Err(_) => {
                    // Port might be in use, try next one
                    continue;
                }
            }
        }

        panic!("Failed to create test messenger after 10 attempts");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_publish_before_start_session_fails() {
        let (mut messenger, _temp_dir) = create_test_messenger().await;

        // Start the router but not the session
        messenger
            .start_router()
            .await
            .expect("Failed to start router");

        // Attempt to publish without starting session - should fail
        let msg = Message::new("test/topic", b"This should fail");
        let result = messenger.publish(msg, PublisherQoS::Standard).await;
        assert!(
            result.is_err(),
            "Publishing before start_session should fail"
        );

        // Shutdown the router
        messenger.stop_router().await.expect("Failed to shutdown");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_basic_publish_subscribe() {
        let (mut messenger, _temp_dir) = create_test_messenger().await;

        messenger
            .start_router()
            .await
            .expect("Failed to start router");

        messenger
            .start_session()
            .await
            .expect("Failed to start session");

        // Subscribe to a topic
        let mut sub = messenger
            .subscribe("test/topic", SubscriberQoS::Standard)
            .await
            .expect("Failed to subscribe");

        // Publish a message
        let msg = Message::new("test/topic", b"Hello World");
        messenger
            .publish(msg.clone(), PublisherQoS::Standard)
            .await
            .expect("Failed to publish");

        // Verify subscriber receives the message
        let received = sub.rx.recv().await.expect("Failed to receive message");
        assert_eq!(received.topic, "test/topic");
        assert_eq!(received.payload, msg.payload);

        messenger.stop_router().await.expect("Failed to shutdown");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_multiple_topics() {
        let (mut messenger, _temp_dir) = create_test_messenger().await;

        messenger
            .start_router()
            .await
            .expect("Failed to start router");

        messenger
            .start_session()
            .await
            .expect("Failed to start session");

        // Subscribe to multiple topics with different throughput modes
        let mut sub1 = messenger
            .subscribe("test/topic1", SubscriberQoS::Standard)
            .await
            .expect("Failed to subscribe to topic1");
        let mut sub2 = messenger
            .subscribe("test/topic2", SubscriberQoS::HighThroughput)
            .await
            .expect("Failed to subscribe to topic2");

        // Publish to different topics
        let msg1 = Message::new("test/topic1", b"Message for topic1");
        let msg2 = Message::new("test/topic2", b"Message for topic2");

        messenger
            .publish(msg1.clone(), PublisherQoS::Standard)
            .await
            .expect("Failed to publish to topic1");
        messenger
            .publish(msg2.clone(), PublisherQoS::Standard)
            .await
            .expect("Failed to publish to topic2");

        // Verify each subscriber receives only its topic's message
        let received1 = sub1.rx.recv().await.expect("Failed to receive on topic1");
        assert_eq!(received1.topic, "test/topic1");
        assert_eq!(received1.payload, msg1.payload);

        let received2 = sub2.rx.recv().await.expect("Failed to receive on topic2");
        assert_eq!(received2.topic, "test/topic2");
        assert_eq!(received2.payload, msg2.payload);

        messenger.stop_router().await.expect("Failed to shutdown");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_multiple_messages_same_topic() {
        let (mut messenger, _temp_dir) = create_test_messenger().await;

        messenger
            .start_router()
            .await
            .expect("Failed to start router");

        messenger
            .start_session()
            .await
            .expect("Failed to start session");

        let mut sub = messenger
            .subscribe("test/topic", SubscriberQoS::Standard)
            .await
            .expect("Failed to subscribe");

        // Publish multiple messages to the same topic
        let msg1 = Message::new("test/topic", b"First message");
        let msg2 = Message::new("test/topic", b"Second message");
        let msg3 = Message::new("test/topic", b"Third message");

        messenger
            .publish(msg1.clone(), PublisherQoS::Standard)
            .await
            .expect("Failed to publish msg1");
        messenger
            .publish(msg2.clone(), PublisherQoS::Standard)
            .await
            .expect("Failed to publish msg2");
        messenger
            .publish(msg3.clone(), PublisherQoS::Standard)
            .await
            .expect("Failed to publish msg3");

        // Verify all messages are received in order
        let received1 = sub.rx.recv().await.expect("Failed to receive msg1");
        assert_eq!(received1.payload, msg1.payload);

        let received2 = sub.rx.recv().await.expect("Failed to receive msg2");
        assert_eq!(received2.payload, msg2.payload);

        let received3 = sub.rx.recv().await.expect("Failed to receive msg3");
        assert_eq!(received3.payload, msg3.payload);

        messenger.stop_router().await.expect("Failed to shutdown");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_late_subscription() {
        let (mut messenger, _temp_dir) = create_test_messenger().await;

        messenger
            .start_router()
            .await
            .expect("Failed to start router");

        messenger
            .start_session()
            .await
            .expect("Failed to start session");

        // Publish a message before any subscription
        let early_msg = Message::new("test/topic", b"Early message");
        messenger
            .publish(early_msg.clone(), PublisherQoS::Standard)
            .await
            .expect("Failed to publish early message");

        // Create subscription after the message was published
        let mut late_sub = messenger
            .subscribe("test/topic", SubscriberQoS::Standard)
            .await
            .expect("Failed to create late subscription");

        // Publish a new message
        let new_msg = Message::new("test/topic", b"New message for late subscriber");
        messenger
            .publish(new_msg.clone(), PublisherQoS::Standard)
            .await
            .expect("Failed to publish new message");

        // Late subscriber should only receive the new message, not the early one
        let received = late_sub.rx.recv().await.expect("Failed to receive message");
        assert_eq!(received.topic, "test/topic");
        assert_eq!(received.payload, new_msg.payload);

        messenger.stop_router().await.expect("Failed to shutdown");
    }
}
