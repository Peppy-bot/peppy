#[cfg(feature = "zenoh")]
#[test]
fn test_with_zenoh_feature() {
    use pmi::{Message, MessagingEngineContext, Messenger, SubscriberQoS};

    // Verify core types are available
    let _msg = Message::new("test/topic", b"test payload");
    let _qos = SubscriberQoS::Standard;
    assert_eq!(_qos.channel_size(), 128);

    // Verify Messenger can be created with Zenoh backend
    let zenoh_context = MessagingEngineContext {
        engine: "zenoh".to_string(),
        config_path: None,
    };
    let _zenoh_messenger =
        Messenger::new(zenoh_context).expect("Should create Messenger with Zenoh");

    // Verify Mock backend is also available even with zenoh feature
    let mock_context = MessagingEngineContext {
        engine: "mock".to_string(),
        config_path: None,
    };
    let _mock_messenger = Messenger::new(mock_context).expect("Should create Messenger with Mock");

    // Verify context type is available
    assert!(
        std::any::type_name::<MessagingEngineContext>().contains("MessagingEngineContext"),
        "MessagingEngineContext should be available with zenoh feature"
    );
}

#[cfg(not(feature = "zenoh"))]
#[test]
fn test_without_zenoh_feature() {
    use pmi::{Message, MessagingEngineContext, Messenger, SubscriberQoS};

    // Verify core types are available (same as with zenoh)
    let _msg = Message::new("test/topic", b"test payload");
    let _qos = SubscriberQoS::HighThroughput;
    assert_eq!(_qos.channel_size(), 1024);

    // Verify Messenger can be created with Mock backend (now the default)
    let mock_context = MessagingEngineContext {
        engine: "mock".to_string(),
        config_path: None,
    };
    let _messenger = Messenger::new(mock_context).expect("Should create Messenger with Mock");

    // Verify context type is available
    assert!(
        std::any::type_name::<MessagingEngineContext>().contains("MessagingEngineContext"),
        "MessagingEngineContext should be available without zenoh feature (using mock)"
    );
}
