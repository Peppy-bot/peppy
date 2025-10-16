#[cfg(feature = "zenoh")]
#[test]
fn test_with_zenoh_feature() {
    use pmi::{
        Message, PeppyMessagingInterfaceError, SubscriberQoS, ZenohClientConfigTemplate,
        ZenohNetProtocol,
    };

    assert!(cfg!(feature = "zenoh"), "zenoh feature should be enabled");

    let message = Message::new("test/topic", b"test payload");
    assert_eq!(message.topic, "test/topic");
    assert_eq!(&message.payload[..], b"test payload");

    let qos = SubscriberQoS::Standard;
    assert_eq!(qos.channel_size(), 128);

    let client_template = ZenohClientConfigTemplate {
        host: "127.0.0.1".into(),
        port: config::consts::DEFAULT_ZENOH_PORT,
        protocol: ZenohNetProtocol::Tcp,
    };
    assert_eq!(client_template.protocol, ZenohNetProtocol::Tcp);

    let err = PeppyMessagingInterfaceError::UnsupportedEngine;
    assert_eq!(format!("{err}"), "UnsupportedEngine");

    let messenger_type = std::any::type_name::<pmi::Messenger>();
    assert!(
        messenger_type.ends_with("Messenger"),
        "Messenger type should be exported"
    );
}

#[cfg(not(feature = "zenoh"))]
#[test]
fn test_without_zenoh_feature() {
    use pmi::{Message, PeppyMessagingInterfaceError, SubscriberQoS};

    assert!(
        !cfg!(feature = "zenoh"),
        "zenoh feature should be disabled for this test"
    );

    let message = Message::new("test/topic", b"test payload");
    assert_eq!(message.topic, "test/topic");
    assert_eq!(&message.payload[..], b"test payload");

    let qos = SubscriberQoS::HighThroughput;
    assert_eq!(qos.channel_size(), 1024);

    let err = PeppyMessagingInterfaceError::UnsupportedEngine;
    assert_eq!(format!("{err}"), "UnsupportedEngine");

    let messenger_type = std::any::type_name::<pmi::Messenger>();
    assert!(
        messenger_type.ends_with("Messenger"),
        "Messenger type should be exported"
    );
}
