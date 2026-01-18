#[cfg(feature = "zenoh")]
mod zenoh_tests {
    use bytes::Bytes;
    use pmi::{
        Message, Messenger, MessengerBackend, PublisherQoS, SubscriberQoS,
        zenohd_support::prepare_zenohd_test_router,
    };
    use std::{path::PathBuf, time::Duration};

    const INSTANCE_ID: &str = "test-instance";
    const MASTER_NODE: &str = "test-master";

    /// Creates a valid key expression with the expected format for TopicMessage.
    /// Format: target_master/caller_master/target_instance/caller_instance/topic
    /// TopicMessage extracts: instance_id from index 3, master_node from index 1
    fn make_key_expr(topic: &str) -> String {
        format!(
            "target_master/{}/target_instance/{}/{}",
            MASTER_NODE, INSTANCE_ID, topic
        )
    }

    /// Small delay to allow Zenoh's subscriber discovery to propagate.
    /// This is necessary because Zenoh pub/sub matching takes time to establish.
    async fn wait_for_subscriber_discovery() {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    /// Helper function to create a configured messenger with a unique port
    fn create_test_messenger() -> (Messenger, tempfile::TempDir, PathBuf) {
        let (messenger, temp_dir, config_path, _, _) =
            prepare_zenohd_test_router("127.0.0.1", None)
                .expect("Failed to prepare zenoh test messenger");
        (messenger, temp_dir, config_path)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_publish_before_start_session_fails() {
        let (mut messenger, _temp_dir, _) = create_test_messenger();

        // Start the router but not the session
        messenger
            .start_router()
            .await
            .expect("Failed to start router");

        // Attempt to publish without starting session - should fail
        let msg = Message::new(
            &format!("test/topic/<INSTANCE_ID:{}>", INSTANCE_ID),
            Bytes::from_static(b"This should fail"),
        );
        let result = messenger.publish(msg, PublisherQoS::Standard).await;
        assert!(
            result.is_err(),
            "Publishing before start_session should fail"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_basic_publish_subscribe() {
        let (mut messenger, _temp_dir, _) = create_test_messenger();

        messenger
            .start_router()
            .await
            .expect("Failed to start router");

        messenger
            .start_session()
            .await
            .expect("Failed to start session");

        // Subscribe to a topic pattern that matches the key expression format
        let mut sub = messenger
            .subscribe("target_master/**/test_topic", SubscriberQoS::Standard)
            .await
            .expect("Failed to subscribe");

        // Wait for subscriber discovery to propagate
        wait_for_subscriber_discovery().await;

        // Publish a message using the correct key expression format
        let key_expr = make_key_expr("test_topic");
        let msg = Message::new(&key_expr, Bytes::from_static(b"Hello World"));
        messenger
            .publish(msg.clone(), PublisherQoS::Standard)
            .await
            .expect("Failed to publish");

        // Verify subscriber receives the message
        let received = sub.rx.recv().await.expect("Failed to receive message");
        assert_eq!(received.instance_id(), INSTANCE_ID);
        assert_eq!(received.master_node(), MASTER_NODE);
        assert_eq!(received.payload(), msg.payload());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_multiple_topics() {
        let (mut messenger, _temp_dir, _) = create_test_messenger();

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
            .subscribe("target_master/**/topic1", SubscriberQoS::Standard)
            .await
            .expect("Failed to subscribe to topic1");
        let mut sub2 = messenger
            .subscribe("target_master/**/topic2", SubscriberQoS::HighThroughput)
            .await
            .expect("Failed to subscribe to topic2");

        // Wait for subscriber discovery to propagate
        wait_for_subscriber_discovery().await;

        // Publish to different topics using correct key expression format
        let key_expr1 = make_key_expr("topic1");
        let key_expr2 = make_key_expr("topic2");
        let msg1 = Message::new(&key_expr1, Bytes::from_static(b"Message for topic1"));
        let msg2 = Message::new(&key_expr2, Bytes::from_static(b"Message for topic2"));

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
        assert_eq!(received1.instance_id(), INSTANCE_ID);
        assert_eq!(received1.master_node(), MASTER_NODE);
        assert_eq!(received1.payload(), msg1.payload());

        let received2 = sub2.rx.recv().await.expect("Failed to receive on topic2");
        assert_eq!(received2.instance_id(), INSTANCE_ID);
        assert_eq!(received2.master_node(), MASTER_NODE);
        assert_eq!(received2.payload(), msg2.payload());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_multiple_messages_same_topic() {
        let (mut messenger, _temp_dir, _) = create_test_messenger();

        messenger
            .start_router()
            .await
            .expect("Failed to start router");

        messenger
            .start_session()
            .await
            .expect("Failed to start session");

        let mut sub = messenger
            .subscribe("target_master/**/test_topic", SubscriberQoS::Standard)
            .await
            .expect("Failed to subscribe");

        // Wait for subscriber discovery to propagate
        wait_for_subscriber_discovery().await;

        // Publish multiple messages to the same topic using correct key expression format
        let key_expr = make_key_expr("test_topic");
        let msg1 = Message::new(&key_expr, Bytes::from_static(b"First message"));
        let msg2 = Message::new(&key_expr, Bytes::from_static(b"Second message"));
        let msg3 = Message::new(&key_expr, Bytes::from_static(b"Third message"));

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
        assert_eq!(received1.payload(), msg1.payload());

        let received2 = sub.rx.recv().await.expect("Failed to receive msg2");
        assert_eq!(received2.payload(), msg2.payload());

        let received3 = sub.rx.recv().await.expect("Failed to receive msg3");
        assert_eq!(received3.payload(), msg3.payload());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_late_subscription() {
        let (mut messenger, _temp_dir, _) = create_test_messenger();

        messenger
            .start_router()
            .await
            .expect("Failed to start router");

        messenger
            .start_session()
            .await
            .expect("Failed to start session");

        let key_expr = make_key_expr("test_topic");

        // Publish a message before any subscription
        let early_msg = Message::new(&key_expr, Bytes::from_static(b"Early message"));
        messenger
            .publish(early_msg.clone(), PublisherQoS::Standard)
            .await
            .expect("Failed to publish early message");

        // Create subscription after the message was published
        let mut late_sub = messenger
            .subscribe("target_master/**/test_topic", SubscriberQoS::Standard)
            .await
            .expect("Failed to create late subscription");

        // Wait for subscriber discovery to propagate
        wait_for_subscriber_discovery().await;

        // Publish a new message
        let new_msg = Message::new(
            &key_expr,
            Bytes::from_static(b"New message for late subscriber"),
        );
        messenger
            .publish(new_msg.clone(), PublisherQoS::Standard)
            .await
            .expect("Failed to publish new message");

        // Late subscriber should only receive the new message, not the early one
        let received = late_sub.rx.recv().await.expect("Failed to receive message");
        assert_eq!(received.instance_id(), INSTANCE_ID);
        assert_eq!(received.master_node(), MASTER_NODE);
        assert_eq!(received.payload(), new_msg.payload());
    }
}
