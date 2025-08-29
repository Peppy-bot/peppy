use pmi::messaging::{Message, Messenger, MessengerBackend, ThroughputMode};
use pmi::types::MessagingEngineContext;

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_local_zenoh_messaging() {
    // Use a random port to avoid conflicts
    let port = 8000 + (std::process::id() % 1000);
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

    // Write config to a temporary file
    let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
    let config_path = temp_dir.path().join("test_zenoh_config.json5");
    std::fs::write(&config_path, config_content).expect("Failed to write test config");

    let context = MessagingEngineContext::new("zenoh".to_string(), Some(config_path));
    let mut messenger = Messenger::new(context).expect("Failed to create messenger");

    // Start the router
    messenger.init().await.expect("Failed to start router");

    // Subscribe to multiple topics
    let mut sub1 = messenger
        .subscribe("test/topic1", ThroughputMode::LowThroughput)
        .await
        .expect("Failed to subscribe to topic1");
    let mut sub2 = messenger
        .subscribe("test/topic2", ThroughputMode::HighThroughput)
        .await
        .expect("Failed to subscribe to topic2");

    // Publish messages to different topics
    let msg1 = Message::new("test/topic1", b"Hello from topic1");
    let msg2 = Message::new("test/topic2", b"Hello from topic2");
    let msg3 = Message::new("test/topic1", b"Second message on topic1");

    messenger
        .publish(msg1.clone())
        .await
        .expect("Failed to publish to topic1");
    messenger
        .publish(msg2.clone())
        .await
        .expect("Failed to publish to topic2");
    messenger
        .publish(msg3.clone())
        .await
        .expect("Failed to publish second message to topic1");

    // Verify subscribers receive the correct messages
    // Topic1 should receive two messages
    let received1_1 = sub1
        .rx
        .recv()
        .await
        .expect("Failed to receive message 1 on topic1");
    assert_eq!(received1_1.topic, "test/topic1");
    assert_eq!(received1_1.payload, msg1.payload);

    let received1_2 = sub1
        .rx
        .recv()
        .await
        .expect("Failed to receive message 2 on topic1");
    assert_eq!(received1_2.topic, "test/topic1");
    assert_eq!(received1_2.payload, msg3.payload);

    // Topic2 should receive one message
    let received2 = sub2
        .rx
        .recv()
        .await
        .expect("Failed to receive message on topic2");
    assert_eq!(received2.topic, "test/topic2");
    assert_eq!(received2.payload, msg2.payload);

    // Test subscribing after messages have been published
    let mut late_sub = messenger
        .subscribe("test/topic3", ThroughputMode::LowThroughput)
        .await
        .expect("Failed to create late subscription");

    // Publish a new message to the late subscriber's topic
    let late_msg = Message::new("test/topic3", b"Hello late subscriber");
    messenger
        .publish(late_msg.clone())
        .await
        .expect("Failed to publish to topic3");

    // Late subscriber should receive the new message
    let late_received = late_sub
        .rx
        .recv()
        .await
        .expect("Failed to receive message on topic3");
    assert_eq!(late_received.topic, "test/topic3");
    assert_eq!(late_received.payload, late_msg.payload);

    // Shutdown the messaging system
    messenger.shutdown().await.expect("Failed to shutdown");
}
