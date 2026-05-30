#![cfg(feature = "build_zenoh")]

mod common;

mod zenoh_tests {
    use crate::common::{
        RECV_TIMEOUT, ZENOH_SERIAL, test_node_target, wait_for_subscriber_discovery,
    };
    use bytes::Bytes;
    use pmi::{
        MessengerBackend, Payload, PublisherQoS, SubscriberQoS, TopicWireReceiver, TopicWireSender,
        ZenohAdapter, ZenohNetProtocol,
    };
    use std::time::{Duration, Instant};

    /// Awaits a single message on `rx` or fails the test on timeout. The
    /// `label` is included in both timeout and channel-closed panics so test
    /// failures in CI pinpoint which subscription stalled.
    async fn recv_or_timeout(
        rx: &mut flume::Receiver<pmi::TopicMessage>,
        label: &str,
    ) -> pmi::TopicMessage {
        tokio::time::timeout(RECV_TIMEOUT, rx.recv_async())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for message on {label}"))
            .unwrap_or_else(|_| panic!("channel closed before message on {label}"))
    }

    fn sender(as_topic_name: &str) -> TopicWireSender {
        TopicWireSender::new(
            "test_core_node",
            "test_instance",
            test_node_target("test_node"),
            None,
            as_topic_name,
        )
        .expect("valid wire fields")
    }

    fn receiver(to_topic: &str) -> TopicWireReceiver {
        TopicWireReceiver::new(
            "test_core_node",
            "test_instance",
            None,
            None,
            Some(test_node_target("test_node")),
            None,
            to_topic,
        )
        .expect("valid wire fields")
    }

    /// Opens a fresh (non-reconnecting) publisher session against the router at
    /// `host:port`, retrying briefly in case the router is still settling after
    /// a respawn. Panics if it can't connect within the retry budget.
    async fn open_publisher(host: &str, port: u16) -> ZenohAdapter {
        for _ in 0..40 {
            if let Ok(mut adapter) = ZenohAdapter::connect_to(ZenohNetProtocol::Tcp, host, port)
                && adapter.start_session().await.is_ok()
            {
                return adapter;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        panic!("could not open a publisher session against {host}:{port}");
    }

    /// Repeatedly publishes to `topic` and waits briefly for the subscriber to
    /// receive, until something arrives or `budget` elapses. Returns `true` if a
    /// message was delivered. Used after a router respawn to give the
    /// reconnecting subscriber time to re-establish and re-declare.
    async fn poll_until_delivered(
        publisher: &mut ZenohAdapter,
        topic: &str,
        rx: &mut flume::Receiver<pmi::TopicMessage>,
        budget: Duration,
    ) -> bool {
        let deadline = Instant::now() + budget;
        let mut attempt = 0u32;
        while Instant::now() < deadline {
            attempt += 1;
            let body = Bytes::from(format!("after-restart-{attempt}"));
            // Ignore publish errors: the publisher's link may still be
            // re-establishing in the first moments after the respawn.
            let _ = publisher
                .publish_topic(
                    &sender(topic),
                    Payload::from_bytes(body),
                    PublisherQoS::Standard,
                    true,
                )
                .await;
            if tokio::time::timeout(Duration::from_millis(800), rx.recv_async())
                .await
                .is_ok_and(|r| r.is_ok())
            {
                return true;
            }
        }
        false
    }

    /// Proves the daemon's in-process recovery path: a reconnecting subscriber
    /// session keeps working after its router process is killed and respawned on
    /// the same port — i.e. the session reconnects *and re-declares* its
    /// subscription. This is the half of the router-watchdog fix that the unit
    /// tests can't cover.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reconnecting_subscriber_recovers_after_router_restart() {
        const TOPIC: &str = "reconnect_topic";
        let _lock = ZENOH_SERIAL.lock().await;

        // `instance` owns the zenohd process so we can respawn it on the same
        // port mid-test. We never open a session on it — only use it to drive
        // the router lifecycle (stop_router / start_router).
        let mut instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
            .await
            .expect("Failed to start zenohd process");
        let host = instance.host.clone();
        let port = instance.port;

        // The subscriber session uses the SAME reconnecting config the daemon
        // uses (`with_session_reconnect`) — this is the behaviour under test.
        let mut subscriber = ZenohAdapter::connect_to(ZenohNetProtocol::Tcp, &host, port)
            .expect("subscriber adapter")
            .with_session_reconnect();
        subscriber
            .start_session()
            .await
            .expect("subscriber start_session");
        let mut subscription = subscriber
            .subscribe_topic(&receiver(TOPIC), SubscriberQoS::Standard)
            .await
            .expect("subscribe");

        // Baseline: a fresh publisher reaches the subscriber through the router.
        {
            let mut publisher = open_publisher(&host, port).await;
            wait_for_subscriber_discovery().await;
            publisher
                .publish_topic(
                    &sender(TOPIC),
                    Payload::from_bytes(Bytes::from_static(b"before-restart")),
                    PublisherQoS::Standard,
                    true,
                )
                .await
                .expect("baseline publish");
            let got = recv_or_timeout(&mut subscription.rx, "baseline").await;
            assert_eq!(got.payload(), &Bytes::from_static(b"before-restart"));
        }

        // Respawn zenohd on the same port — exactly what the watchdog does when
        // it finds the router wedged.
        instance
            .messenger()
            .stop_router()
            .await
            .expect("stop_router");
        instance
            .messenger()
            .start_router()
            .await
            .expect("start_router");

        // The reconnecting subscriber must re-establish and re-declare its
        // subscription against the new router. Drive a fresh publisher and poll
        // until delivery (or give up after a generous budget).
        let mut publisher = open_publisher(&host, port).await;
        wait_for_subscriber_discovery().await;
        let recovered = poll_until_delivered(
            &mut publisher,
            TOPIC,
            &mut subscription.rx,
            Duration::from_secs(30),
        )
        .await;

        assert!(
            recovered,
            "reconnecting subscriber did not receive after the router was respawned: the session \
             did not reconnect + re-declare its subscription"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_publish_before_start_session_fails() {
        let _lock = ZENOH_SERIAL.lock().await;
        let mut instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
            .await
            .expect("Failed to start zenohd process");

        // No start_session — publish should fail.
        let payload = Payload::from_bytes(Bytes::from_static(b"This should fail"));
        let result = instance
            .messenger()
            .publish_topic(
                &sender("should_fail"),
                payload,
                PublisherQoS::Standard,
                true,
            )
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

        let mut sub = instance
            .messenger()
            .subscribe_topic(&receiver("basic_topic"), SubscriberQoS::Standard)
            .await
            .expect("Failed to subscribe");

        wait_for_subscriber_discovery().await;

        let body = Bytes::from_static(b"Hello World");
        instance
            .messenger()
            .publish_topic(
                &sender("basic_topic"),
                Payload::from_bytes(body.clone()),
                PublisherQoS::Standard,
                true,
            )
            .await
            .expect("Failed to publish");

        let received = recv_or_timeout(&mut sub.rx, "test_basic_publish_subscribe sub").await;
        assert_eq!(received.instance_id(), "test_instance");
        assert_eq!(received.core_node(), "test_core_node");
        assert_eq!(received.payload(), &body);
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

        let mut sub1 = instance
            .messenger()
            .subscribe_topic(&receiver("topic1"), SubscriberQoS::Standard)
            .await
            .expect("Failed to subscribe to topic1");
        let mut sub2 = instance
            .messenger()
            .subscribe_topic(&receiver("topic2"), SubscriberQoS::HighThroughput)
            .await
            .expect("Failed to subscribe to topic2");

        wait_for_subscriber_discovery().await;

        let body1 = Bytes::from_static(b"Message for topic1");
        let body2 = Bytes::from_static(b"Message for topic2");

        instance
            .messenger()
            .publish_topic(
                &sender("topic1"),
                Payload::from_bytes(body1.clone()),
                PublisherQoS::Standard,
                true,
            )
            .await
            .expect("Failed to publish to topic1");
        instance
            .messenger()
            .publish_topic(
                &sender("topic2"),
                Payload::from_bytes(body2.clone()),
                PublisherQoS::Standard,
                true,
            )
            .await
            .expect("Failed to publish to topic2");

        let received1 = recv_or_timeout(&mut sub1.rx, "test_multiple_topics sub1").await;
        assert_eq!(received1.payload(), &body1);

        let received2 = recv_or_timeout(&mut sub2.rx, "test_multiple_topics sub2").await;
        assert_eq!(received2.payload(), &body2);
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
            .subscribe_topic(&receiver("multi_topic"), SubscriberQoS::Standard)
            .await
            .expect("Failed to subscribe");

        wait_for_subscriber_discovery().await;

        let messages = [
            Bytes::from_static(b"First message"),
            Bytes::from_static(b"Second message"),
            Bytes::from_static(b"Third message"),
        ];

        for body in &messages {
            instance
                .messenger()
                .publish_topic(
                    &sender("multi_topic"),
                    Payload::from_bytes(body.clone()),
                    PublisherQoS::Standard,
                    true,
                )
                .await
                .expect("Failed to publish");
        }

        for expected in &messages {
            let received =
                recv_or_timeout(&mut sub.rx, "test_multiple_messages_same_topic sub").await;
            assert_eq!(received.payload(), expected);
        }
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

        let early_body = Bytes::from_static(b"Early message");
        instance
            .messenger()
            .publish_topic(
                &sender("late_topic"),
                Payload::from_bytes(early_body),
                PublisherQoS::Standard,
                true,
            )
            .await
            .expect("Failed to publish early message");

        let mut late_sub = instance
            .messenger()
            .subscribe_topic(&receiver("late_topic"), SubscriberQoS::Standard)
            .await
            .expect("Failed to create late subscription");

        wait_for_subscriber_discovery().await;

        let new_body = Bytes::from_static(b"New message for late subscriber");
        instance
            .messenger()
            .publish_topic(
                &sender("late_topic"),
                Payload::from_bytes(new_body.clone()),
                PublisherQoS::Standard,
                true,
            )
            .await
            .expect("Failed to publish new message");

        let received = recv_or_timeout(&mut late_sub.rx, "test_late_subscription late_sub").await;
        assert_eq!(received.payload(), &new_body);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_with_router_creates_adapter_with_router() {
        let _lock = ZENOH_SERIAL.lock().await;
        use pmi::{Messenger, MessengerAdapter, ZenohNetProtocol};

        // Reserve a port first to ensure we have an available one
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let adapter = ZenohAdapter::with_router(ZenohNetProtocol::Tcp, "127.0.0.1", port).unwrap();
        let (host, adapter_port) = adapter.client_endpoint();
        assert_eq!(host, "127.0.0.1");
        assert_eq!(adapter_port, port);

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

        let mut router_instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
            .await
            .expect("Failed to start router");

        let router_host = router_instance.host.clone();
        let router_port = router_instance.port;

        let client_adapter =
            ZenohAdapter::connect_to(ZenohNetProtocol::Tcp, &router_host, router_port).unwrap();
        let (host, port) = client_adapter.client_endpoint();
        assert_eq!(host, router_host);
        assert_eq!(port, router_port);

        let mut client_messenger = Messenger::new(MessengerAdapter::Zenoh(client_adapter));
        client_messenger
            .start_session()
            .await
            .expect("Failed to start client session");

        router_instance
            .messenger()
            .start_session()
            .await
            .expect("Failed to start router session");

        let mut sub = router_instance
            .messenger()
            .subscribe_topic(&receiver("connect_test"), SubscriberQoS::Standard)
            .await
            .expect("Failed to subscribe");

        wait_for_subscriber_discovery().await;

        let body = Bytes::from_static(b"Hello from client");
        client_messenger
            .publish_topic(
                &sender("connect_test"),
                Payload::from_bytes(body.clone()),
                PublisherQoS::Standard,
                true,
            )
            .await
            .expect("Failed to publish from client");

        let received = recv_or_timeout(&mut sub.rx, "test_connect_to_existing_router sub").await;
        assert_eq!(received.payload(), &body);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_start_router_ephemeral_with_specific_port() {
        let _lock = ZENOH_SERIAL.lock().await;
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let mut instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", Some(port))
            .await
            .expect("Failed to start router with specific port");

        assert_eq!(instance.port, port);
        assert_eq!(instance.host, "127.0.0.1");

        instance
            .messenger()
            .start_session()
            .await
            .expect("Failed to start session");

        let mut sub = instance
            .messenger()
            .subscribe_topic(&receiver("port_test"), SubscriberQoS::Standard)
            .await
            .expect("Failed to subscribe");

        wait_for_subscriber_discovery().await;

        let body = Bytes::from_static(b"Test with specific port");
        instance
            .messenger()
            .publish_topic(
                &sender("port_test"),
                Payload::from_bytes(body.clone()),
                PublisherQoS::Standard,
                true,
            )
            .await
            .expect("Failed to publish");

        let received = recv_or_timeout(
            &mut sub.rx,
            "test_start_router_ephemeral_with_specific_port sub",
        )
        .await;
        assert_eq!(received.payload(), &body);
    }
}
