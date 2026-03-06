#[cfg(feature = "build_zenoh")]
mod zenoh_tests {
    use bytes::Bytes;
    use pmi::{Message, MessengerBackend, PublisherQoS, SubscriberQoS, ZenohAdapter};
    use std::time::Duration;
    use tokio::sync::Mutex;

    const INSTANCE_ID: &str = "test-instance";
    const CORE_NODE: &str = "test-core-node";

    /// Each test spawns a zenohd process. Under parallel execution the combined
    /// startup load can cause transient handshake failures.  Serializing with a
    /// mutex eliminates the flakiness without adding time-based probes.
    static ZENOH_SERIAL: Mutex<()> = Mutex::const_new(());

    /// Creates a valid key expression with the expected format for TopicMessage.
    /// Format: target_core_node/caller_core_node/target_instance/caller_instance/topic
    /// TopicMessage extracts: instance_id from index 3, core_node from index 1
    fn make_key_expr(topic: &str) -> String {
        format!(
            "target_core_node/{}/target_instance/{}/{}",
            CORE_NODE, INSTANCE_ID, topic
        )
    }

    /// Small delay to allow Zenoh's subscriber discovery to propagate.
    /// This is necessary because Zenoh pub/sub matching takes time to establish.
    async fn wait_for_subscriber_discovery() {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_publish_before_start_session_fails() {
        let _lock = ZENOH_SERIAL.lock().await;
        let mut instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
            .await
            .expect("Failed to start zenohd process");

        // Attempt to publish without starting session - should fail
        let msg = Message::new(
            &format!("test/topic/<INSTANCE_ID:{}>", INSTANCE_ID),
            Bytes::from_static(b"This should fail"),
        );
        let result = instance
            .messenger()
            .publish(msg, PublisherQoS::Standard)
            .await;
        assert!(
            result.is_err(),
            "Publishing before start_session should fail"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_basic_publish_subscribe() {
        let _lock = ZENOH_SERIAL.lock().await;
        let mut instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
            .await
            .expect("Failed to start zenohd process");

        instance
            .messenger()
            .start_session()
            .await
            .expect("Failed to start session");

        // Subscribe to a topic pattern that matches the key expression format
        let mut sub = instance
            .messenger()
            .subscribe("target_core_node/**/test_topic", SubscriberQoS::Standard)
            .await
            .expect("Failed to subscribe");

        // Wait for subscriber discovery to propagate
        wait_for_subscriber_discovery().await;

        // Publish a message using the correct key expression format
        let key_expr = make_key_expr("test_topic");
        let msg = Message::new(&key_expr, Bytes::from_static(b"Hello World"));
        instance
            .messenger()
            .publish(msg.clone(), PublisherQoS::Standard)
            .await
            .expect("Failed to publish");

        // Verify subscriber receives the message
        let received = sub.rx.recv().await.expect("Failed to receive message");
        assert_eq!(received.instance_id(), INSTANCE_ID);
        assert_eq!(received.core_node(), CORE_NODE);
        assert_eq!(received.payload(), msg.payload());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_multiple_topics() {
        let _lock = ZENOH_SERIAL.lock().await;
        let mut instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
            .await
            .expect("Failed to start zenohd process");

        instance
            .messenger()
            .start_session()
            .await
            .expect("Failed to start session");

        // Subscribe to multiple topics with different throughput modes
        let mut sub1 = instance
            .messenger()
            .subscribe("target_core_node/**/topic1", SubscriberQoS::Standard)
            .await
            .expect("Failed to subscribe to topic1");
        let mut sub2 = instance
            .messenger()
            .subscribe("target_core_node/**/topic2", SubscriberQoS::HighThroughput)
            .await
            .expect("Failed to subscribe to topic2");

        // Wait for subscriber discovery to propagate
        wait_for_subscriber_discovery().await;

        // Publish to different topics using correct key expression format
        let key_expr1 = make_key_expr("topic1");
        let key_expr2 = make_key_expr("topic2");
        let msg1 = Message::new(&key_expr1, Bytes::from_static(b"Message for topic1"));
        let msg2 = Message::new(&key_expr2, Bytes::from_static(b"Message for topic2"));

        instance
            .messenger()
            .publish(msg1.clone(), PublisherQoS::Standard)
            .await
            .expect("Failed to publish to topic1");
        instance
            .messenger()
            .publish(msg2.clone(), PublisherQoS::Standard)
            .await
            .expect("Failed to publish to topic2");

        // Verify each subscriber receives only its topic's message
        let received1 = sub1.rx.recv().await.expect("Failed to receive on topic1");
        assert_eq!(received1.instance_id(), INSTANCE_ID);
        assert_eq!(received1.core_node(), CORE_NODE);
        assert_eq!(received1.payload(), msg1.payload());

        let received2 = sub2.rx.recv().await.expect("Failed to receive on topic2");
        assert_eq!(received2.instance_id(), INSTANCE_ID);
        assert_eq!(received2.core_node(), CORE_NODE);
        assert_eq!(received2.payload(), msg2.payload());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_multiple_messages_same_topic() {
        let _lock = ZENOH_SERIAL.lock().await;
        let mut instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
            .await
            .expect("Failed to start zenohd process");

        instance
            .messenger()
            .start_session()
            .await
            .expect("Failed to start session");

        let mut sub = instance
            .messenger()
            .subscribe("target_core_node/**/test_topic", SubscriberQoS::Standard)
            .await
            .expect("Failed to subscribe");

        // Wait for subscriber discovery to propagate
        wait_for_subscriber_discovery().await;

        // Publish multiple messages to the same topic using correct key expression format
        let key_expr = make_key_expr("test_topic");
        let msg1 = Message::new(&key_expr, Bytes::from_static(b"First message"));
        let msg2 = Message::new(&key_expr, Bytes::from_static(b"Second message"));
        let msg3 = Message::new(&key_expr, Bytes::from_static(b"Third message"));

        instance
            .messenger()
            .publish(msg1.clone(), PublisherQoS::Standard)
            .await
            .expect("Failed to publish msg1");
        instance
            .messenger()
            .publish(msg2.clone(), PublisherQoS::Standard)
            .await
            .expect("Failed to publish msg2");
        instance
            .messenger()
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
        let _lock = ZENOH_SERIAL.lock().await;
        let mut instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
            .await
            .expect("Failed to start zenohd process");

        instance
            .messenger()
            .start_session()
            .await
            .expect("Failed to start session");

        let key_expr = make_key_expr("test_topic");

        // Publish a message before any subscription
        let early_msg = Message::new(&key_expr, Bytes::from_static(b"Early message"));
        instance
            .messenger()
            .publish(early_msg.clone(), PublisherQoS::Standard)
            .await
            .expect("Failed to publish early message");

        // Create subscription after the message was published
        let mut late_sub = instance
            .messenger()
            .subscribe("target_core_node/**/test_topic", SubscriberQoS::Standard)
            .await
            .expect("Failed to create late subscription");

        // Wait for subscriber discovery to propagate
        wait_for_subscriber_discovery().await;

        // Publish a new message
        let new_msg = Message::new(
            &key_expr,
            Bytes::from_static(b"New message for late subscriber"),
        );
        instance
            .messenger()
            .publish(new_msg.clone(), PublisherQoS::Standard)
            .await
            .expect("Failed to publish new message");

        // Late subscriber should only receive the new message, not the early one
        let received = late_sub.rx.recv().await.expect("Failed to receive message");
        assert_eq!(received.instance_id(), INSTANCE_ID);
        assert_eq!(received.core_node(), CORE_NODE);
        assert_eq!(received.payload(), new_msg.payload());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_with_router_creates_adapter_with_router() {
        let _lock = ZENOH_SERIAL.lock().await;
        use pmi::{Messenger, MessengerAdapter, ZenohNetProtocol};

        // Reserve a port first to ensure we have an available one
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        // Create adapter with router
        let adapter = ZenohAdapter::with_router(ZenohNetProtocol::Tcp, "127.0.0.1", port).unwrap();

        // Verify client endpoint is configured correctly
        let (host, adapter_port) = adapter.client_endpoint();
        assert_eq!(host, "127.0.0.1");
        assert_eq!(adapter_port, port);

        // Create messenger and start/stop router to verify it works
        let mut messenger = Messenger::new(MessengerAdapter::Zenoh(adapter));
        messenger
            .start_router()
            .await
            .expect("Failed to start router");
        messenger
            .stop_router()
            .await
            .expect("Failed to stop router");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_connect_to_existing_router() {
        let _lock = ZENOH_SERIAL.lock().await;
        use pmi::{Messenger, MessengerAdapter, ZenohNetProtocol};

        // Start a router using start_router_ephemeral
        let mut router_instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
            .await
            .expect("Failed to start router");

        let router_host = router_instance.host.clone();
        let router_port = router_instance.port;

        // Create a separate adapter that connects to the existing router
        let client_adapter =
            ZenohAdapter::connect_to(ZenohNetProtocol::Tcp, &router_host, router_port).unwrap();

        // Verify client endpoint matches the router
        let (host, port) = client_adapter.client_endpoint();
        assert_eq!(host, router_host);
        assert_eq!(port, router_port);

        // Create messenger and start session to verify connection works
        let mut client_messenger = Messenger::new(MessengerAdapter::Zenoh(client_adapter));
        client_messenger
            .start_session()
            .await
            .expect("Failed to start client session");

        // Start session on router side too
        router_instance
            .messenger()
            .start_session()
            .await
            .expect("Failed to start router session");

        // Subscribe on router, publish from client to verify connectivity
        let mut sub = router_instance
            .messenger()
            .subscribe("target_core_node/**/connect_test", SubscriberQoS::Standard)
            .await
            .expect("Failed to subscribe");

        wait_for_subscriber_discovery().await;

        let key_expr = make_key_expr("connect_test");
        let msg = Message::new(&key_expr, Bytes::from_static(b"Hello from client"));
        client_messenger
            .publish(msg.clone(), PublisherQoS::Standard)
            .await
            .expect("Failed to publish from client");

        let received = sub.rx.recv().await.expect("Failed to receive message");
        assert_eq!(received.payload(), msg.payload());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_start_router_ephemeral_with_specific_port() {
        let _lock = ZENOH_SERIAL.lock().await;
        // Reserve a port first to ensure we have an available one
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        // Start router with specific port
        let mut instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", Some(port))
            .await
            .expect("Failed to start router with specific port");

        // Verify the instance uses the requested port
        assert_eq!(instance.port, port);
        assert_eq!(instance.host, "127.0.0.1");

        // Verify the router is functional
        instance
            .messenger()
            .start_session()
            .await
            .expect("Failed to start session");

        let mut sub = instance
            .messenger()
            .subscribe("target_core_node/**/port_test", SubscriberQoS::Standard)
            .await
            .expect("Failed to subscribe");

        wait_for_subscriber_discovery().await;

        let key_expr = make_key_expr("port_test");
        let msg = Message::new(&key_expr, Bytes::from_static(b"Test with specific port"));
        instance
            .messenger()
            .publish(msg.clone(), PublisherQoS::Standard)
            .await
            .expect("Failed to publish");

        let received = sub.rx.recv().await.expect("Failed to receive message");
        assert_eq!(received.payload(), msg.payload());
    }
}
