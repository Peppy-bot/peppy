//! The four-way transport matrix for the shared-memory data path: the matrix
//! test runs its scenarios under all `TransportProfile::ALL` legs (peer/router
//! × shm on/off), with POSITIVE assertions that SHM was actually used when on
//! — `Payload::is_shm_backed` on the received message — so a
//! silently-degraded TCP delivery cannot pass as green. The `router+shm` legs
//! additionally prove the locally-built zenohd forwards SHM buffers (both
//! hops of node→zenohd→node go through the segment).
//!
//! ## Locked-memory discipline
//!
//! zenoh `mlock`s every SHM segment it creates or maps, and caches consumer
//! mappings for the life of the process — so on hosts with a small
//! `RLIMIT_MEMLOCK` (8 MiB is a common default) every extra publisher session
//! that touches SHM permanently spends budget. The matrix test therefore uses
//! ONE publisher/subscriber pair per leg for ALL its scenarios; adding more
//! SHM-publishing sessions to this binary can exhaust the budget and surface
//! as silent rx-mapping failures (delivery timeouts), not clean errors.

#![cfg(feature = "build_zenoh")]

mod common;

mod shm_tests {
    use crate::common::{
        RECV_TIMEOUT, ZENOH_SERIAL, test_node_target, wait_for_subscriber_discovery,
    };
    use bytes::Bytes;
    use config::peppy_config::TransportProfile;
    use pmi::{
        MessengerBackend, Payload, PublisherQoS, SHM_PUBLISH_THRESHOLD_BYTES, SHM_SEGMENT_BYTES,
        SubscriberBufferSizes, SubscriberQoS, TopicWireReceiver, TopicWireSender, ZenohAdapter,
        ZenohNetProtocol,
    };

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

    /// A recognizable non-uniform fill so a content mismatch (wrong length,
    /// wrong offset, stale buffer reuse) cannot pass by accident.
    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    /// Opens a session against the router at `host:port` with the leg's
    /// gossip/shm settings applied.
    async fn connect(profile: TransportProfile, host: &str, port: u16) -> ZenohAdapter {
        let mut adapter = ZenohAdapter::connect_to_with_discovery(
            ZenohNetProtocol::Tcp,
            host,
            port,
            Vec::new(),
            profile.gossip(),
            profile.shm,
            SubscriberBufferSizes::default(),
        )
        .expect("adapter construction");
        adapter.start_session().await.expect("session should start");
        adapter
    }

    async fn recv_or_timeout(
        rx: &mut flume::Receiver<pmi::TopicMessage>,
        label: &str,
    ) -> pmi::TopicMessage {
        tokio::time::timeout(RECV_TIMEOUT, rx.recv_async())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for message on {label}"))
            .unwrap_or_else(|_| panic!("channel closed before message on {label}"))
    }

    /// The whole matrix in one test: for each leg, ONE router + ONE
    /// subscriber + ONE publisher session run every scenario —
    ///
    /// 1. plain publish above the threshold (tier 1): delivered intact and
    ///    SHM-backed exactly when the leg has shm on;
    /// 2. plain publish below the threshold: delivered, never SHM-backed;
    /// 3. loaned publish (tier 2): the loan itself is SHM-backed iff shm on,
    ///    identical caller code in all four legs, delivery SHM-backed iff on;
    /// 4. a truncated loan: only the filled prefix travels (the contract
    ///    capnp's phase-2 over-allocation relies on), still SHM when on.
    ///
    /// Scenarios share the leg's sessions deliberately — see the module notes
    /// on the locked-memory budget.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn four_way_matrix_delivers_and_uses_shm_iff_enabled() {
        let large = pattern(SHM_PUBLISH_THRESHOLD_BYTES + 1);
        let small = pattern(16);
        assert!(small.len() < SHM_PUBLISH_THRESHOLD_BYTES);
        let prefix = pattern(SHM_PUBLISH_THRESHOLD_BYTES + 1234);

        for profile in TransportProfile::ALL {
            let _lock = ZENOH_SERIAL.lock().await;
            let instance = ZenohAdapter::start_router_ephemeral_in_mode(
                "127.0.0.1",
                None,
                profile,
                SubscriberBufferSizes::default(),
            )
            .await
            .expect("router should start");

            let subscriber = connect(profile, &instance.host, instance.port).await;
            let mut subscription = subscriber
                .subscribe_topic(&receiver("shm_matrix"), SubscriberQoS::Standard)
                .await
                .expect("subscription");
            let mut publisher = connect(profile, &instance.host, instance.port).await;
            wait_for_subscriber_discovery().await;

            // 1. Tier 1: plain publish above the threshold. The
            //    `is_shm_backed` assertion is the guard against a
            //    silently-degraded TCP path passing as green.
            publisher
                .publish_topic(
                    &sender("shm_matrix"),
                    Payload::from_bytes(Bytes::from(large.clone())),
                    PublisherQoS::Standard,
                    true,
                )
                .await
                .expect("plain publish");
            let msg = recv_or_timeout(&mut subscription.rx, "plain large").await;
            assert_eq!(
                msg.payload().as_bytes().as_ref(),
                large.as_slice(),
                "leg {profile:?}: large payload corrupted"
            );
            assert_eq!(
                msg.payload().is_shm_backed(),
                profile.shm.enabled,
                "leg {profile:?}: plain large publish expected is_shm_backed == {}",
                profile.shm.enabled
            );
            drop(msg);

            // 2. Below the threshold the heap path runs untouched, even with
            //    shm on.
            publisher
                .publish_topic(
                    &sender("shm_matrix"),
                    Payload::from_bytes(Bytes::from(small.clone())),
                    PublisherQoS::Standard,
                    true,
                )
                .await
                .expect("small publish");
            let msg = recv_or_timeout(&mut subscription.rx, "plain small").await;
            assert_eq!(msg.payload().as_bytes().as_ref(), small.as_slice());
            assert!(
                !msg.payload().is_shm_backed(),
                "leg {profile:?}: sub-threshold payload must not be SHM-backed"
            );
            drop(msg);

            // 3. Tier 2: the loan is born in SHM iff the leg has shm on, and
            //    the subscriber reads the same physical buffer when on.
            let bound = publisher
                .declare_topic_publisher(&sender("shm_matrix"), PublisherQoS::Standard)
                .expect("declared publisher");
            let mut loan = bound.loan(large.len());
            assert_eq!(
                loan.is_shm(),
                profile.shm.enabled,
                "leg {profile:?}: expected loan.is_shm() == {}",
                profile.shm.enabled
            );
            loan.copy_from_slice(&large);
            bound.publish_loaned(loan).await.expect("publish_loaned");
            let msg = recv_or_timeout(&mut subscription.rx, "loaned").await;
            assert_eq!(msg.payload().as_bytes().as_ref(), large.as_slice());
            assert_eq!(msg.payload().is_shm_backed(), profile.shm.enabled);
            drop(msg);

            // 4. Over-allocate, fill a prefix, truncate: only the prefix
            //    travels, and a shrunk SHM loan stays SHM-backed.
            let mut loan = bound.loan(2 * prefix.len());
            loan[..prefix.len()].copy_from_slice(&prefix);
            loan.truncate(prefix.len());
            assert_eq!(loan.len(), prefix.len());
            assert_eq!(loan.is_shm(), profile.shm.enabled);
            bound.publish_loaned(loan).await.expect("publish prefix");
            let msg = recv_or_timeout(&mut subscription.rx, "truncated prefix").await;
            assert_eq!(msg.payload().as_bytes().as_ref(), prefix.as_slice());
            assert_eq!(msg.payload().is_shm_backed(), profile.shm.enabled);
        }
    }

    /// A loan no SHM pool can satisfy must degrade to a heap loan that still
    /// delivers — never block, never error. This exercises both fallbacks:
    /// the loan-time one (oversized alloc fails → heap buffer) and the
    /// publish-time one (tier-1 retry fails again → network path).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn oversized_loan_falls_back_to_heap_and_delivers() {
        let profile = TransportProfile::PEER_SHM;
        let _lock = ZENOH_SERIAL.lock().await;
        let instance = ZenohAdapter::start_router_ephemeral_in_mode(
            "127.0.0.1",
            None,
            profile,
            SubscriberBufferSizes::default(),
        )
        .await
        .expect("router should start");

        let subscriber = connect(profile, &instance.host, instance.port).await;
        let mut subscription = subscriber
            .subscribe_topic(&receiver("shm_oversized"), SubscriberQoS::Standard)
            .await
            .expect("subscription");
        let publisher = connect(profile, &instance.host, instance.port).await;
        wait_for_subscriber_discovery().await;

        // Larger than the whole segment (even at its full 32 MiB target):
        // allocation cannot succeed even after a garbage-collect pass.
        let len = SHM_SEGMENT_BYTES + 4096;
        let bound = publisher
            .declare_topic_publisher(&sender("shm_oversized"), PublisherQoS::Standard)
            .expect("declared publisher");
        let mut loan = bound.loan(len);
        assert!(
            !loan.is_shm(),
            "an oversized loan must fall back to the heap"
        );
        // Fill only the edges; a 32 MiB pattern compare would dominate test time.
        loan[..8].copy_from_slice(b"headmark");
        let tail = loan.len() - 8;
        loan[tail..].copy_from_slice(b"tailmark");
        bound.publish_loaned(loan).await.expect("publish_loaned");

        let msg = recv_or_timeout(&mut subscription.rx, "shm_oversized").await;
        let received = msg.payload().as_bytes();
        assert_eq!(received.len(), len);
        assert_eq!(&received[..8], b"headmark");
        assert_eq!(&received[len - 8..], b"tailmark");
        assert!(!msg.payload().is_shm_backed());
    }
}
