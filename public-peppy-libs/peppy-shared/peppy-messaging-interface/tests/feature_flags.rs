#[cfg(feature = "zenoh")]
#[test]
fn test_with_zenoh_feature() {
    use bytes::Bytes;
    use pmi::{
        Payload, PeppyMessagingInterfaceError, SubscriberBufferSizes, SubscriberQoS,
        ZenohNetProtocol,
    };

    const { assert!(cfg!(feature = "zenoh"), "zenoh feature should be enabled") };

    let payload = Payload::from_bytes(Bytes::from_static(b"test payload"));
    assert_eq!(payload.len(), b"test payload".len());
    assert_eq!(payload.as_bytes().as_ref(), b"test payload");

    let qos = SubscriberQoS::Standard;
    assert_eq!(SubscriberBufferSizes::default().size_for(qos), 128);

    assert_eq!(ZenohNetProtocol::default(), ZenohNetProtocol::Tcp);

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
    use bytes::Bytes;
    use pmi::{Payload, PeppyMessagingInterfaceError, SubscriberBufferSizes, SubscriberQoS};

    assert!(
        !cfg!(feature = "zenoh"),
        "zenoh feature should be disabled for this test"
    );

    let payload = Payload::from_bytes(Bytes::from_static(b"test payload"));
    assert_eq!(payload.len(), b"test payload".len());
    assert_eq!(payload.as_bytes().as_ref(), b"test payload");

    let qos = SubscriberQoS::HighThroughput;
    assert_eq!(SubscriberBufferSizes::default().size_for(qos), 1024);

    let err = PeppyMessagingInterfaceError::UnsupportedEngine;
    assert_eq!(format!("{err}"), "UnsupportedEngine");

    let messenger_type = std::any::type_name::<pmi::Messenger>();
    assert!(
        messenger_type.ends_with("Messenger"),
        "Messenger type should be exported"
    );
}
