#![cfg(feature = "build_zenoh")]

mod common;

mod zenoh_tests {
    use crate::common::{
        RECV_TIMEOUT, ZENOH_SERIAL, receiver, sender, wait_for_subscriber_discovery,
    };
    use bytes::Bytes;
    use pmi::{
        MessengerBackend, Payload, PresenceScope, PublisherQoS, RouterId, SubscriberBufferSizes,
        SubscriberQoS, ZenohAdapter, ZenohNetProtocol,
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
            // Only a post-restart payload proves recovery: a stale `before-restart`
            // sample redelivered through the reconnecting session must not count.
            if let Ok(Ok(msg)) =
                tokio::time::timeout(Duration::from_millis(800), rx.recv_async()).await
                && msg.payload().as_bytes().starts_with(b"after-restart-")
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

    /// Proves peer-to-peer data flow: two `peer` sessions that share a router
    /// discover each other via gossip and form a direct link, and topic
    /// delivery between them survives the router being stopped. If data still
    /// relayed through the router, stopping it would cut delivery; that delivery
    /// continues shows the router hop is gone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn peers_keep_delivering_after_router_stops() {
        const TOPIC: &str = "direct_link_topic";
        let _lock = ZENOH_SERIAL.lock().await;

        let mut instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
            .await
            .expect("Failed to start zenohd process");
        let host = instance.host.clone();
        let port = instance.port;

        // Two non-reconnecting peers seeded by the router: a subscriber and a
        // publisher. `connect_to` opens peer-mode sessions.
        let mut subscriber = ZenohAdapter::connect_to(ZenohNetProtocol::Tcp, &host, port)
            .expect("subscriber adapter");
        subscriber
            .start_session()
            .await
            .expect("subscriber start_session");
        let mut subscription = subscriber
            .subscribe_topic(&receiver(TOPIC), SubscriberQoS::Standard)
            .await
            .expect("subscribe");

        let mut publisher = ZenohAdapter::connect_to(ZenohNetProtocol::Tcp, &host, port)
            .expect("publisher adapter");
        publisher
            .start_session()
            .await
            .expect("publisher start_session");

        // Baseline delivery, then give gossip ample time to establish the direct
        // peer-to-peer link before the router is removed.
        wait_for_subscriber_discovery().await;
        publisher
            .publish_topic(
                &sender(TOPIC),
                Payload::from_bytes(Bytes::from_static(b"before-stop")),
                PublisherQoS::Standard,
                true,
            )
            .await
            .expect("baseline publish");
        let baseline = recv_or_timeout(&mut subscription.rx, "baseline").await;
        assert_eq!(baseline.payload(), &Bytes::from_static(b"before-stop"));
        tokio::time::sleep(Duration::from_secs(3)).await;

        // Remove the router. Any delivery from here on is over the direct link.
        instance
            .messenger()
            .stop_router()
            .await
            .expect("stop_router");
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Poll-publish until a post-stop payload arrives (or give up). Only an
        // `after-stop-*` payload counts; a stale relayed `before-stop` must not.
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut attempt = 0u32;
        let mut delivered = false;
        while Instant::now() < deadline {
            attempt += 1;
            let body = Bytes::from(format!("after-stop-{attempt}"));
            let _ = publisher
                .publish_topic(
                    &sender(TOPIC),
                    Payload::from_bytes(body),
                    PublisherQoS::Standard,
                    true,
                )
                .await;
            if let Ok(Ok(msg)) =
                tokio::time::timeout(Duration::from_millis(500), subscription.rx.recv_async()).await
                && msg.payload().as_bytes().starts_with(b"after-stop-")
            {
                delivered = true;
                break;
            }
        }

        assert!(
            delivered,
            "no message was delivered after the router was stopped: data was relaying through the \
             router instead of flowing peer-to-peer"
        );
    }

    /// Router mode (gossip off → plain client sessions) still delivers, with the
    /// traffic relayed through the running zenohd router. Positive companion to
    /// `peers_keep_delivering_after_router_stops`, which proves the peer path;
    /// here the router is required and kept alive for the whole test.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn router_mode_delivers_through_router() {
        const TOPIC: &str = "router_mode_topic";
        let _lock = ZENOH_SERIAL.lock().await;

        let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
            .await
            .expect("Failed to start zenohd process");
        let host = instance.host.clone();
        let port = instance.port;

        // gossip=false → plain client sessions with no peer listener, so all
        // traffic routes through the central router.
        let mut subscriber = ZenohAdapter::connect_to_with_discovery(
            ZenohNetProtocol::Tcp,
            &host,
            port,
            Vec::new(),
            false,
            SubscriberBufferSizes::default(),
            None,
        )
        .expect("subscriber adapter");
        subscriber
            .start_session()
            .await
            .expect("subscriber start_session");
        let mut subscription = subscriber
            .subscribe_topic(&receiver(TOPIC), SubscriberQoS::Standard)
            .await
            .expect("subscribe");

        let mut publisher = ZenohAdapter::connect_to_with_discovery(
            ZenohNetProtocol::Tcp,
            &host,
            port,
            Vec::new(),
            false,
            SubscriberBufferSizes::default(),
            None,
        )
        .expect("publisher adapter");
        publisher
            .start_session()
            .await
            .expect("publisher start_session");

        wait_for_subscriber_discovery().await;
        publisher
            .publish_topic(
                &sender(TOPIC),
                Payload::from_bytes(Bytes::from_static(b"router-mode")),
                PublisherQoS::Standard,
                true,
            )
            .await
            .expect("publish");
        let msg = recv_or_timeout(&mut subscription.rx, "router-mode").await;
        assert_eq!(msg.payload(), &Bytes::from_static(b"router-mode"));

        // Keep the router alive until the end: client-mode delivery depends on it.
        drop(instance);
    }

    /// Source timestamps must be present on delivered samples in BOTH peer and
    /// router mode. The benchmark's `delivery` measurement reads
    /// `source_timestamp_nanos()` off each sample, so a session that fails to
    /// stamp its outgoing data silently zeroes those rows. `gossip=true`
    /// exercises the direct peer path (the regression); `gossip=false` the
    /// router relay.
    async fn assert_topic_carries_source_timestamp(gossip: bool) {
        const TOPIC: &str = "source_timestamp_topic";
        let _lock = ZENOH_SERIAL.lock().await;

        let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
            .await
            .expect("Failed to start zenohd process");
        let host = instance.host.clone();
        let port = instance.port;

        let mut subscriber = ZenohAdapter::connect_to_with_discovery(
            ZenohNetProtocol::Tcp,
            &host,
            port,
            Vec::new(),
            gossip,
            SubscriberBufferSizes::default(),
            None,
        )
        .expect("subscriber adapter");
        subscriber
            .start_session()
            .await
            .expect("subscriber start_session");
        let mut subscription = subscriber
            .subscribe_topic(&receiver(TOPIC), SubscriberQoS::Standard)
            .await
            .expect("subscribe");

        let mut publisher = ZenohAdapter::connect_to_with_discovery(
            ZenohNetProtocol::Tcp,
            &host,
            port,
            Vec::new(),
            gossip,
            SubscriberBufferSizes::default(),
            None,
        )
        .expect("publisher adapter");
        publisher
            .start_session()
            .await
            .expect("publisher start_session");

        wait_for_subscriber_discovery().await;
        publisher
            .publish_topic(
                &sender(TOPIC),
                Payload::from_bytes(Bytes::from_static(b"stamped")),
                PublisherQoS::Standard,
                true,
            )
            .await
            .expect("publish");
        let msg = recv_or_timeout(&mut subscription.rx, "stamped").await;
        assert_eq!(msg.payload(), &Bytes::from_static(b"stamped"));
        assert!(
            msg.source_timestamp_nanos().is_some(),
            "delivered sample must carry a source timestamp (gossip={gossip}): the producing \
             session did not stamp its outgoing data, so delivery latency can't be measured"
        );

        // Keep the router alive until the end: client-mode delivery depends on it.
        drop(instance);
    }

    /// Peer mode (gossip on → direct peer links) stamps outgoing samples. This is
    /// the regression guard for the peer-mode timestamping fix: before it, the
    /// peer session enabled timestamping under the wrong role and samples arrived
    /// unstamped.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn peer_mode_samples_carry_source_timestamp() {
        assert_topic_carries_source_timestamp(true).await;
    }

    /// Router mode (gossip off → client sessions relayed through zenohd) stamps
    /// outgoing samples too. Companion to the peer-mode guard, pinning parity.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn router_mode_samples_carry_source_timestamp() {
        assert_topic_carries_source_timestamp(false).await;
    }

    /// A peer session built with custom (tiny) subscriber buffers still delivers
    /// end-to-end. This pins that the buffer sizes are threaded through
    /// `connect_to_with_discovery` without breaking delivery; exact-capacity
    /// backpressure is covered by the `SubscriberBufferSizes` unit tests.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn peer_session_uses_custom_buffer_sizes() {
        const TOPIC: &str = "custom_buffer_topic";
        let _lock = ZENOH_SERIAL.lock().await;

        let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
            .await
            .expect("Failed to start zenohd process");
        let host = instance.host.clone();
        let port = instance.port;

        let tiny = SubscriberBufferSizes {
            standard: 2,
            high_throughput: 2,
        };
        let mut subscriber = ZenohAdapter::connect_to_with_discovery(
            ZenohNetProtocol::Tcp,
            &host,
            port,
            Vec::new(),
            true,
            tiny,
            None,
        )
        .expect("subscriber adapter");
        subscriber
            .start_session()
            .await
            .expect("subscriber start_session");
        let mut subscription = subscriber
            .subscribe_topic(&receiver(TOPIC), SubscriberQoS::Standard)
            .await
            .expect("subscribe");

        let mut publisher = ZenohAdapter::connect_to(ZenohNetProtocol::Tcp, &host, port)
            .expect("publisher adapter");
        publisher
            .start_session()
            .await
            .expect("publisher start_session");

        wait_for_subscriber_discovery().await;
        // Publish and drain one at a time so the tiny buffer never overflows.
        for i in 0..5 {
            let body = Bytes::from(format!("msg-{i}"));
            publisher
                .publish_topic(
                    &sender(TOPIC),
                    Payload::from_bytes(body.clone()),
                    PublisherQoS::Standard,
                    true,
                )
                .await
                .expect("publish");
            let msg = recv_or_timeout(&mut subscription.rx, "custom-buffer").await;
            assert_eq!(msg.payload(), &body);
        }
        drop(instance);
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

    /// Action-producer liveliness over real zenoh: a token declared by the
    /// producer session replays as an initial `Alive` to a late watch
    /// (history), a one-shot probe sees it, and closing the producer session
    /// — the in-process stand-in for hard producer death, since tokens are
    /// removed identically on close and on transport loss — surfaces as a
    /// `Gone` event and an absent probe.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn action_liveliness_token_observed_across_sessions() {
        use pmi::{ActionWireReceiver, ActionWireSender, LivelinessEvent, SenderTarget};

        let _lock = ZENOH_SERIAL.lock().await;
        let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
            .await
            .expect("Failed to start zenohd process");
        let host = instance.host.clone();
        let port = instance.port;

        let mut producer =
            ZenohAdapter::connect_to(ZenohNetProtocol::Tcp, &host, port).expect("producer adapter");
        producer
            .start_session()
            .await
            .expect("producer start_session");
        let mut consumer =
            ZenohAdapter::connect_to(ZenohNetProtocol::Tcp, &host, port).expect("consumer adapter");
        consumer
            .start_session()
            .await
            .expect("consumer start_session");

        let target = SenderTarget::node("arm", "v1").expect("node target");
        let receiver =
            ActionWireReceiver::new("server_core", "server_inst", target.clone(), "move")
                .expect("valid receiver");
        let sender = ActionWireSender::new(
            "caller_core",
            "caller_inst",
            Some(&pmi::ProducerRef::new("server_core", "server_inst")),
            target,
            "move",
        )
        .expect("valid sender");

        let _token = producer
            .declare_action_liveliness(&receiver)
            .await
            .expect("token should declare");
        wait_for_subscriber_discovery().await;

        // Late watch: the pre-existing token must replay as an initial Alive.
        let watch = consumer
            .watch_action_producer(&sender)
            .await
            .expect("watch should declare");
        let initial = tokio::time::timeout(RECV_TIMEOUT, watch.rx.recv_async())
            .await
            .expect("timed out waiting for the initial liveliness event")
            .expect("liveliness watch closed unexpectedly");
        assert_eq!(initial, LivelinessEvent::Alive(()));

        let probe = consumer
            .probe_action_producer(&sender, Duration::from_secs(2))
            .await
            .expect("probe should issue");
        assert!(probe.resolve().await, "token should be observed alive");

        // Producer death: closing the session removes the token.
        producer
            .stop_session()
            .await
            .expect("producer stop_session");
        let gone = tokio::time::timeout(RECV_TIMEOUT, watch.rx.recv_async())
            .await
            .expect("timed out waiting for the Gone liveliness event")
            .expect("liveliness watch closed unexpectedly");
        assert_eq!(gone, LivelinessEvent::Gone(()));

        let probe = consumer
            .probe_action_producer(&sender, Duration::from_secs(2))
            .await
            .expect("probe should issue");
        assert!(!probe.resolve().await, "token should be observed gone");
    }

    /// Core-node presence over real zenoh: a token declared on one session is
    /// replayed to a late watcher and returned by a collecting list query on a
    /// second session. Closing the declaring session emits the identity-bearing
    /// `Gone` event and removes it from subsequent lists.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn core_node_presence_lifecycle_observed_across_sessions() {
        use pmi::{CoreNodePresence, LivelinessEvent, Segment};

        let _lock = ZENOH_SERIAL.lock().await;
        let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
            .await
            .expect("Failed to start zenohd process");
        let host = instance.host.clone();
        let port = instance.port;

        let mut producer =
            ZenohAdapter::connect_to(ZenohNetProtocol::Tcp, &host, port).expect("producer adapter");
        producer
            .start_session()
            .await
            .expect("producer start_session");
        let mut observer =
            ZenohAdapter::connect_to(ZenohNetProtocol::Tcp, &host, port).expect("observer adapter");
        observer
            .start_session()
            .await
            .expect("observer start_session");

        let core_node = Segment::try_from("daemon_a").expect("valid core-node segment");
        let instance_id = Segment::try_from("generation_1").expect("valid instance segment");
        let expected = CoreNodePresence::new("daemon_a", "generation_1");
        let _token = producer
            .declare_core_node_presence(&core_node, &instance_id)
            .await
            .expect("presence token should declare");
        wait_for_subscriber_discovery().await;

        let watch = observer
            .watch_core_node_presence(Some(&core_node))
            .await
            .expect("presence watch should declare");
        let initial = tokio::time::timeout(RECV_TIMEOUT, watch.rx.recv_async())
            .await
            .expect("timed out waiting for initial presence event")
            .expect("presence watch closed unexpectedly");
        assert_eq!(initial, LivelinessEvent::Alive(expected.clone()));
        assert_eq!(
            observer
                .list_core_node_presence(PresenceScope::Session, None, Duration::from_secs(2))
                .await
                .expect("presence list should issue")
                .collect()
                .await
                .expect("presence list should succeed"),
            vec![expected.clone()]
        );

        producer
            .stop_session()
            .await
            .expect("producer stop_session");
        let gone = tokio::time::timeout(RECV_TIMEOUT, watch.rx.recv_async())
            .await
            .expect("timed out waiting for Gone presence event")
            .expect("presence watch closed unexpectedly");
        assert_eq!(gone, LivelinessEvent::Gone(expected));
        assert!(
            observer
                .list_core_node_presence(PresenceScope::Session, None, Duration::from_secs(2))
                .await
                .expect("presence list should issue after token removal")
                .collect()
                .await
                .expect("presence list should succeed after token removal")
                .is_empty()
        );
    }

    /// Probe hardening over real zenoh: `ServiceQueryKind::Probe` queries
    /// are answered by the adapter's query dispatch (one Response-kind
    /// reply; sized probes get the requested byte count) and never reach
    /// the producer's endpoint channel — so a producer whose task is busy
    /// in user code (nobody draining the channel) still answers discovery
    /// and liveness probes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn probes_are_answered_in_dispatch_and_never_enqueued() {
        use pmi::{
            SenderTarget, ServiceKind, ServiceQueryKind, ServiceReplyKind, ServiceWireReceiver,
            ServiceWireSender,
        };

        let _lock = ZENOH_SERIAL.lock().await;
        let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
            .await
            .expect("Failed to start zenohd process");
        let host = instance.host.clone();
        let port = instance.port;

        let mut producer =
            ZenohAdapter::connect_to(ZenohNetProtocol::Tcp, &host, port).expect("producer adapter");
        producer
            .start_session()
            .await
            .expect("producer start_session");
        let mut consumer =
            ZenohAdapter::connect_to(ZenohNetProtocol::Tcp, &host, port).expect("consumer adapter");
        consumer
            .start_session()
            .await
            .expect("consumer start_session");

        let target = SenderTarget::node("camera", "v1").expect("node target");
        let receiver = ServiceWireReceiver::new(
            "server_core",
            "server_inst",
            target.clone(),
            "ping",
            ServiceKind::Service,
        )
        .expect("valid receiver");
        let sender = ServiceWireSender::new(
            "caller_core",
            "caller_inst",
            None, // wildcard target: the discovery probe shape
            target,
            "ping",
            ServiceKind::Service,
        )
        .expect("valid sender");

        let queryable = producer
            .listen_service(&receiver)
            .await
            .expect("queryable declare should succeed");
        // Nobody drains `queryable.rx` — the producer is "busy" in user code.
        wait_for_subscriber_discovery().await;

        // Plain (empty-body) probe: one empty Response-kind reply.
        let mut reply_stream = consumer
            .call_service(
                &sender,
                Payload::from_bytes(Bytes::new()),
                ServiceQueryKind::Probe,
                Some(RECV_TIMEOUT),
            )
            .await
            .expect("probe call should succeed");
        let reply = tokio::time::timeout(RECV_TIMEOUT, reply_stream.rx.recv())
            .await
            .expect("probe must be answered while the endpoint loop is busy")
            .expect("probe reply stream closed unexpectedly");
        assert_eq!(reply.kind(), ServiceReplyKind::Response);
        assert!(reply.message().payload().is_empty());

        // Benchmark sized probe: the reply carries the requested size.
        let mut reply_stream = consumer
            .call_service(
                &sender,
                Payload::from_bytes(pmi::build_sized_probe_request(64, 4096)),
                ServiceQueryKind::Probe,
                Some(RECV_TIMEOUT),
            )
            .await
            .expect("sized probe call should succeed");
        let reply = tokio::time::timeout(RECV_TIMEOUT, reply_stream.rx.recv())
            .await
            .expect("sized probe must be answered while the endpoint loop is busy")
            .expect("sized probe reply stream closed unexpectedly");
        assert_eq!(reply.kind(), ServiceReplyKind::Response);
        assert_eq!(reply.message().payload().len(), 4096);

        // Neither probe leaked into the endpoint channel.
        assert!(
            queryable.rx.try_recv().is_err(),
            "probes must never reach the endpoint channel"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_with_router_creates_adapter_with_router() {
        let _lock = ZENOH_SERIAL.lock().await;
        use pmi::{Messenger, MessengerAdapter, ZenohNetProtocol};

        // Reserve a port first to ensure we have an available one
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let adapter = ZenohAdapter::with_router(
            ZenohNetProtocol::Tcp,
            "127.0.0.1",
            port,
            true,
            SubscriberBufferSizes::default(),
            Vec::new(),
            None,
            RouterId::generate(),
        )
        .unwrap();
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

    // ---- Workspace namespace isolation ----

    /// Opens a non-reconnecting peer session under `namespace`, retrying briefly
    /// while the router settles.
    async fn open_namespaced(host: &str, port: u16, namespace: &str) -> ZenohAdapter {
        let ns = pmi::Namespace::parse(namespace).expect("valid namespace");
        for _ in 0..40 {
            let mut adapter = ZenohAdapter::connect_to(ZenohNetProtocol::Tcp, host, port)
                .expect("adapter")
                .with_namespace(Some(ns.clone()));
            if adapter.start_session().await.is_ok() {
                return adapter;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        panic!("could not open a namespaced session against {host}:{port}");
    }

    /// Asserts the subscription receives nothing within a short window.
    async fn assert_no_delivery(rx: &mut flume::Receiver<pmi::TopicMessage>, label: &str) {
        let isolated = tokio::time::timeout(Duration::from_millis(1500), rx.recv_async())
            .await
            .is_err();
        assert!(
            isolated,
            "a cross-namespace publish must not reach the subscriber ({label})"
        );
    }

    /// The core workspace-isolation guarantee: two sessions under the SAME namespace deliver
    /// pub/sub through the router, while a publisher under a DIFFERENT namespace
    /// never reaches the subscriber. This is routing-layer isolation, not merely
    /// an ingress drop: the subscriber's key is rewritten on the wire to
    /// `<ns>/<key>`, which a different workspace's `<other>/<key>` never intersects.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn same_namespace_delivers_and_different_is_isolated() {
        const TOPIC: &str = "workspace_ns_topic";
        let _lock = ZENOH_SERIAL.lock().await;
        let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
            .await
            .expect("router");
        let host = instance.host.clone();
        let port = instance.port;

        let subscriber = open_namespaced(&host, port, "workspace-a").await;
        let mut subscription = subscriber
            .subscribe_topic(&receiver(TOPIC), SubscriberQoS::Standard)
            .await
            .expect("subscribe");

        // Same namespace ⇒ delivered.
        {
            let mut pub_a = open_namespaced(&host, port, "workspace-a").await;
            wait_for_subscriber_discovery().await;
            pub_a
                .publish_topic(
                    &sender(TOPIC),
                    Payload::from_bytes(Bytes::from_static(b"same-namespace")),
                    PublisherQoS::Standard,
                    true,
                )
                .await
                .expect("same-namespace publish");
            let got = recv_or_timeout(&mut subscription.rx, "same-namespace").await;
            assert_eq!(got.payload(), &Bytes::from_static(b"same-namespace"));
        }

        // Different namespace ⇒ the subscriber receives NOTHING.
        {
            let mut pub_b = open_namespaced(&host, port, "workspace-b").await;
            wait_for_subscriber_discovery().await;
            pub_b
                .publish_topic(
                    &sender(TOPIC),
                    Payload::from_bytes(Bytes::from_static(b"other-namespace")),
                    PublisherQoS::Standard,
                    true,
                )
                .await
                .expect("other-namespace publish");
            assert_no_delivery(&mut subscription.rx, "different workspace").await;
        }
    }

    /// Session namespaces apply to liveliness declarations and queries just as
    /// they do ordinary pub/sub keys. Each workspace therefore enumerates
    /// only its own core-node tokens even when sessions share one router.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn core_node_presence_is_isolated_by_namespace() {
        use pmi::{CoreNodePresence, Segment};

        let _lock = ZENOH_SERIAL.lock().await;
        let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
            .await
            .expect("router");
        let host = instance.host.clone();
        let port = instance.port;

        let workspace_a = open_namespaced(&host, port, "workspace-a").await;
        let workspace_b = open_namespaced(&host, port, "workspace-b").await;
        let daemon_a = Segment::try_from("daemon_a").expect("valid core-node segment");
        let daemon_b = Segment::try_from("daemon_b").expect("valid core-node segment");
        let generation_a = Segment::try_from("generation_a").expect("valid instance segment");
        let generation_b = Segment::try_from("generation_b").expect("valid instance segment");
        let _token_a = workspace_a
            .declare_core_node_presence(&daemon_a, &generation_a)
            .await
            .expect("workspace-a token should declare");
        let _token_b = workspace_b
            .declare_core_node_presence(&daemon_b, &generation_b)
            .await
            .expect("workspace-b token should declare");
        wait_for_subscriber_discovery().await;

        assert_eq!(
            workspace_a
                .list_core_node_presence(PresenceScope::Session, None, Duration::from_secs(2))
                .await
                .expect("workspace-a presence list should issue")
                .collect()
                .await
                .expect("workspace-a presence list should succeed"),
            vec![CoreNodePresence::new("daemon_a", "generation_a")]
        );
        assert_eq!(
            workspace_b
                .list_core_node_presence(PresenceScope::Session, None, Duration::from_secs(2))
                .await
                .expect("workspace-b presence list should issue")
                .collect()
                .await
                .expect("workspace-b presence list should succeed"),
            vec![CoreNodePresence::new("daemon_b", "generation_b")]
        );
    }

    /// The assumption the platform's workspace listing rests on: a
    /// **namespace-free** session sees liveliness tokens declared by namespaced
    /// sessions, with the workspace prefix intact on the wire, and a
    /// namespace-scoped query returns exactly that workspace's tokens.
    ///
    /// This is the transport-level counterpart to the `zenoh_format` unit
    /// tests. Those pin the strings; only a live router proves that zenoh
    /// applies session namespaces to liveliness declarations at all, that the
    /// prefix survives to a namespace-free observer, and that the scoped query
    /// matches on chunk boundaries rather than string prefixes. A workspace
    /// named `workspace-a` and one named `workspace-ab` are both present here
    /// for exactly that reason.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_namespace_free_observer_enumerates_one_workspace_at_a_time() {
        use pmi::{CoreNodePresence, Segment};

        let _lock = ZENOH_SERIAL.lock().await;
        let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
            .await
            .expect("router");
        let host = instance.host.clone();
        let port = instance.port;

        let daemon = Segment::try_from("daemon_a").expect("valid core-node segment");
        let generation = Segment::try_from("generation_a").expect("valid instance segment");
        let neighbour_daemon = Segment::try_from("daemon_b").expect("valid core-node segment");
        let neighbour_generation =
            Segment::try_from("generation_b").expect("valid instance segment");

        let workspace_a = open_namespaced(&host, port, "workspace-a").await;
        // `workspace-a` is a string prefix of `workspace-ab`. A selector that
        // matched on string prefixes would leak this daemon into the other
        // workspace's roster.
        let workspace_ab = open_namespaced(&host, port, "workspace-ab").await;
        let _token_a = workspace_a
            .declare_core_node_presence(&daemon, &generation)
            .await
            .expect("workspace-a token should declare");
        let _token_ab = workspace_ab
            .declare_core_node_presence(&neighbour_daemon, &neighbour_generation)
            .await
            .expect("workspace-ab token should declare");

        // The observer: a plain session with no namespace of its own, exactly
        // as the platform backend opens one.
        let mut observer =
            ZenohAdapter::connect_to(ZenohNetProtocol::Tcp, &host, port).expect("observer adapter");
        observer
            .start_session()
            .await
            .expect("observer session should open");
        assert!(
            observer.is_linked().await,
            "an observer that opened against a live router must report a link, \
             otherwise an empty result cannot be told from a dead transport"
        );
        wait_for_subscriber_discovery().await;

        let list_in = async |namespace: &str| {
            let namespace = pmi::Namespace::parse(namespace).expect("valid namespace");
            observer
                .list_core_node_presence(
                    PresenceScope::Namespace(&namespace),
                    None,
                    Duration::from_secs(2),
                )
                .await
                .expect("observer presence list should issue")
                .collect()
                .await
                .expect("observer presence list should succeed")
        };

        assert_eq!(
            list_in("workspace-a").await,
            vec![CoreNodePresence::new("daemon_a", "generation_a")],
            "the observer must see workspace-a's daemon, and only it"
        );
        assert_eq!(
            list_in("workspace-ab").await,
            vec![CoreNodePresence::new("daemon_b", "generation_b")],
            "workspace-ab must not inherit workspace-a's daemon through a string prefix"
        );
        assert!(
            list_in("workspace-c").await.is_empty(),
            "a workspace with no live daemon must enumerate empty, not fall back to another's"
        );
    }

    /// `is_linked` is the signal that separates "nothing is live" from "the
    /// transport is down", so it must be false before a session is opened and
    /// false again once it is closed. Without that, a reconnecting observer
    /// reports every core node offline while it is simply disconnected.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn is_linked_tracks_the_session_rather_than_the_query_result() {
        let _lock = ZENOH_SERIAL.lock().await;
        let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
            .await
            .expect("router");

        let mut observer =
            ZenohAdapter::connect_to(ZenohNetProtocol::Tcp, &instance.host, instance.port)
                .expect("observer adapter");
        assert!(
            !observer.is_linked().await,
            "an adapter with no session yet holds no link"
        );

        observer
            .start_session()
            .await
            .expect("observer session should open");
        assert!(observer.is_linked().await, "an open session holds a link");

        observer
            .stop_session()
            .await
            .expect("observer session should close");
        assert!(
            !observer.is_linked().await,
            "a closed session holds no link, so an empty presence list means nothing"
        );
    }

    /// Pins the asymmetry documented on
    /// [`ZenohAdapter::with_session_reconnect`]: against an endpoint nothing is
    /// serving, the reconnecting config makes `start_session` return
    /// immediately for a **peer** session and not return at all for a
    /// **client** one, until a router shows up at that endpoint.
    ///
    /// This is not a preference being recorded, it is a trap being fenced. Both
    /// sessions ask for the same thing (`connect.timeout_ms: -1`,
    /// `exit_on_failure: false`), and zenoh honours `exit_on_failure` only on
    /// the peer path (`connect_peers_multiply_links` spawns a background
    /// connector) while the client path (`connect_peers_single_link`) retries
    /// in the foreground under a `Duration::MAX` budget. So a caller that opens
    /// a reconnecting client session to a remote router on its startup path
    /// hangs the whole process for as long as that router is down, which is the
    /// opposite of what "reconnecting" reads like it buys.
    ///
    /// The client half runs on a detached `std::thread` rather than a spawned
    /// task, because the block is not a pending future: `zenoh::open` resolves
    /// through `Wait`, whose `IntoFuture` is `ready(self.wait())` over a
    /// `block_in_place`, so it occupies its thread synchronously. Nothing can
    /// cancel it, `tokio::time::timeout` included, and a task left in that
    /// state would stall this runtime's shutdown. A detached thread just dies
    /// with the process if the router never arrives.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reconnecting_client_open_blocks_until_the_router_accepts() {
        // Long enough that "returned immediately" is not a race won, and that
        // several of the config's 1s..4s retries have gone by unsatisfied.
        const BLOCKED_FOR: Duration = Duration::from_secs(5);
        // Generous: this only has to cover one retry period plus the session
        // handshake once the router is listening.
        const UNBLOCKS_WITHIN: Duration = Duration::from_secs(20);
        let _lock = ZENOH_SERIAL.lock().await;

        // A port the OS just handed out and nothing holds: dead now, and
        // bindable by the router below.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("reserve a port")
            .local_addr()
            .expect("reserved port address")
            .port();

        let mut peer = ZenohAdapter::connect_to(ZenohNetProtocol::Tcp, "127.0.0.1", port)
            .expect("peer adapter")
            .with_session_reconnect();
        peer.start_session()
            .await
            .expect("a reconnecting peer session must open against a dead endpoint");
        assert!(
            !peer.is_linked().await,
            "the peer session returned without a link, which is the point: it \
             dials on behind the session rather than holding up the caller"
        );

        let client = ZenohAdapter::connect_to_with_discovery(
            ZenohNetProtocol::Tcp,
            "127.0.0.1",
            port,
            Vec::new(),
            // Gossip off is what makes this a client session; it is the only
            // difference from the peer above, and `connect_to_tls` sets it.
            false,
            SubscriberBufferSizes::default(),
            None,
        )
        .expect("client adapter")
        .with_session_reconnect();
        let (opened_tx, mut opened_rx) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let mut client = client;
            // `block_in_place` panics under a current-thread scheduler, so this
            // thread needs a multi-thread runtime of its own.
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .expect("thread runtime");
            let opened = runtime.block_on(client.start_session());
            let _ = opened_tx.send(opened.is_ok());
        });

        assert!(
            tokio::time::timeout(BLOCKED_FOR, &mut opened_rx)
                .await
                .is_err(),
            "a reconnecting client session must NOT return from start_session \
             while its endpoint is dead; if this now returns, zenoh honours \
             exit_on_failure on the client path too and the callers that drive \
             this open off their startup path can stop"
        );

        let _router = ZenohAdapter::start_router_ephemeral("127.0.0.1", Some(port))
            .await
            .expect("start a router on the reserved port");

        assert!(
            matches!(
                tokio::time::timeout(UNBLOCKS_WITHIN, opened_rx).await,
                Ok(Ok(true))
            ),
            "the blocked client open must complete once a router accepts it, \
             which is what makes the retry worth keeping"
        );
    }

    /// A logged-out (`local`) session and an workspace session sharing one router are
    /// routing-isolated: `local`'s keys are rewritten to `local/<key>` and the
    /// workspace's to `<workspace>/<key>`, which never intersect. Closes the LAN cross-tenant
    /// leak a "no namespace when logged out" model would leave open.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn local_and_workspace_namespaces_are_isolated() {
        const TOPIC: &str = "local_vs_workspace_topic";
        let _lock = ZENOH_SERIAL.lock().await;
        let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
            .await
            .expect("router");
        let host = instance.host.clone();
        let port = instance.port;

        let subscriber = open_namespaced(&host, port, pmi::Namespace::local().as_str()).await;
        let mut subscription = subscriber
            .subscribe_topic(&receiver(TOPIC), SubscriberQoS::Standard)
            .await
            .expect("subscribe");

        // Baseline: a `local` publisher reaches the `local` subscriber.
        {
            let mut local_pub = open_namespaced(&host, port, "local").await;
            wait_for_subscriber_discovery().await;
            local_pub
                .publish_topic(
                    &sender(TOPIC),
                    Payload::from_bytes(Bytes::from_static(b"local-payload")),
                    PublisherQoS::Standard,
                    true,
                )
                .await
                .expect("local publish");
            let got = recv_or_timeout(&mut subscription.rx, "local").await;
            assert_eq!(got.payload(), &Bytes::from_static(b"local-payload"));
        }

        // An workspace publisher must NOT reach the `local` subscriber.
        {
            let mut workspace_pub = open_namespaced(&host, port, "workspace-x").await;
            wait_for_subscriber_discovery().await;
            workspace_pub
                .publish_topic(
                    &sender(TOPIC),
                    Payload::from_bytes(Bytes::from_static(b"workspace-payload")),
                    PublisherQoS::Standard,
                    true,
                )
                .await
                .expect("workspace publish");
            assert_no_delivery(&mut subscription.rx, "workspace vs local").await;
        }
    }
}
