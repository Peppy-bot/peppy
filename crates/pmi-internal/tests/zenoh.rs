#[cfg(feature = "zenoh")]
mod zenoh_tests {
    use pmi::{
        Message, Messenger, MessengerBackend, PublisherQoS, SubscriberQoS,
        zenohd_support::{pick_free_tcp_port, prepare_zenohd_test_router},
    };
    use std::{fs, path::PathBuf};

    /// Helper function to create a configured messenger with a unique port
    fn create_test_messenger() -> (Messenger, tempfile::TempDir, PathBuf) {
        let (messenger, temp_dir, config_path, _, _) =
            prepare_zenohd_test_router("127.0.0.1", None)
                .expect("Failed to prepare zenoh test messenger");
        (messenger, temp_dir, config_path)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_session_fails_with_wrong_router_port() {
        let (mut messenger, _temp_dir, zenohd_config_path) = create_test_messenger();

        let config_str =
            fs::read_to_string(&zenohd_config_path).expect("Failed to read config file");
        let parsed_config: serde_json::Value =
            serde_json5::from_str(&config_str).expect("Failed to parse config");
        let original_endpoint = parsed_config["listen"]["endpoints"]["router"][0]
            .as_str()
            .expect("Router endpoint missing from config");

        let (protocol, host_port) = original_endpoint
            .split_once('/')
            .expect("Invalid endpoint format");
        let (host, original_port) = host_port.split_once(':').expect("Invalid host:port format");
        let original_port: u16 = original_port
            .parse()
            .expect("Failed to parse original port");

        let wrong_port = loop {
            let candidate = pick_free_tcp_port();
            if candidate != original_port {
                break candidate;
            }
        };

        let updated_config = format!(
            r#"{{
                  "listen": {{
                    "endpoints": {{
                      "router": ["{protocol}/{host}:{wrong_port}"]
                    }}
                  }}
                }}"#
        );
        fs::write(&zenohd_config_path, updated_config).expect("Failed to overwrite config");

        messenger
            .start_router()
            .await
            .expect("Router should start with updated config");

        let session_err = messenger
            .start_session()
            .await
            .expect_err("Session start should fail when ports mismatch");
        assert!(
            matches!(
                session_err,
                pmi::PeppyMessagingInterfaceError::BackendError(_)
            ),
            "Expected backend error when ports mismatch, got: {:?}",
            session_err
        );

        let publish_err = messenger
            .publish(
                Message::new("test/topic", b"port mismatch should fail"),
                PublisherQoS::Standard,
            )
            .await
            .expect_err("Publish should fail without an active session");
        assert!(
            matches!(
                publish_err,
                pmi::PeppyMessagingInterfaceError::MessagingSessionError(_)
            ),
            "Expected messaging session error without session, got: {:?}",
            publish_err
        );

        messenger
            .stop_router()
            .await
            .expect("Failed to stop router");
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
        let (mut messenger, _temp_dir, _) = create_test_messenger();

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
        let (mut messenger, _temp_dir, _) = create_test_messenger();

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
