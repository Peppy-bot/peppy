//! End-to-end TLS transport tests: a real `zenohd` router listening on `tls/`
//! and a `connect_to_tls` client completing a TLS handshake and a pub/sub
//! round-trip. This is the *authoritative* check that the `transport.link.tls`
//! block pmi renders is actually accepted by zenoh — a wrong key would be
//! silently dropped at config parse, so only a live handshake proves it.
//!
//! Gated behind `build_zenoh` like the other integration tests (it needs the
//! compiled `zenohd` binary). Fixture certs live in `tests/fixtures/` and were
//! lifted verbatim from zenoh 1.10's own `tests/authentication.rs` (a `minica`
//! CA, a `localhost` server leaf, and a client leaf signed by the same CA for
//! the mutual-TLS cases); see that file for provenance.

#![cfg(feature = "build_zenoh")]

mod common;

mod zenoh_tls_tests {
    use crate::common::{
        RECV_TIMEOUT, ZENOH_SERIAL, receiver, sender, wait_for_subscriber_discovery,
    };
    use bytes::Bytes;
    use pmi::{
        ConnectIdentity, Messenger, MessengerAdapter, MessengerBackend, Payload, PublisherQoS,
        RouterId, SubscriberBufferSizes, SubscriberQoS, TlsConfig, ZenohAdapter, ZenohNetProtocol,
        probe_tls_reachable,
    };
    use std::io::Write;
    use std::path::PathBuf;
    use std::time::Duration;

    const CA_PEM: &[u8] = include_bytes!("fixtures/minica_ca.pem");
    const SERVER_CERT_PEM: &[u8] = include_bytes!("fixtures/server_localhost.pem");
    const SERVER_KEY_PEM: &[u8] = include_bytes!("fixtures/server_localhost.key");
    const CLIENT_CERT_PEM: &[u8] = include_bytes!("fixtures/client_side.pem");
    const CLIENT_KEY_PEM: &[u8] = include_bytes!("fixtures/client_side.key");

    /// Cert files materialized into a tempdir for a single test. zenoh's TLS
    /// config takes filesystem paths, so the embedded fixtures are written out.
    struct Certs {
        // Held only for its `Drop` (cleans up the tempdir at end of test).
        #[allow(dead_code)]
        dir: tempfile::TempDir,
        ca: PathBuf,
        cert: PathBuf,
        key: PathBuf,
        client_cert: PathBuf,
        client_key: PathBuf,
    }

    fn write_certs() -> Certs {
        let dir = tempfile::tempdir().expect("create cert tempdir");
        let put = |name: &str, bytes: &[u8]| {
            let path = dir.path().join(name);
            let mut f = std::fs::File::create(&path).expect("create cert file");
            f.write_all(bytes).expect("write cert file");
            path
        };
        let ca = put("ca.pem", CA_PEM);
        let cert = put("server.pem", SERVER_CERT_PEM);
        let key = put("server.key", SERVER_KEY_PEM);
        let client_cert = put("client.pem", CLIENT_CERT_PEM);
        let client_key = put("client.key", CLIENT_KEY_PEM);
        Certs {
            dir,
            ca,
            cert,
            key,
            client_cert,
            client_key,
        }
    }

    /// The server leaf's SAN is `localhost`, but we dial `127.0.0.1` (no DNS
    /// resolution ambiguity), so name verification is off — exactly how zenoh's
    /// own TLS test uses these fixtures. The CA-trust check stays on, which is
    /// what the negative test below exercises.
    fn trusting_client_tls(certs: &Certs) -> TlsConfig {
        TlsConfig {
            verify_name_on_connect: false,
            ..TlsConfig::client(certs.ca.clone())
        }
    }

    /// The client identity the mutual-TLS cases present: a leaf signed by the
    /// same `minica` CA the router trusts, with name verification off for the
    /// same reason as [`trusting_client_tls`].
    fn identified_client_tls(certs: &Certs) -> TlsConfig {
        TlsConfig {
            verify_name_on_connect: false,
            ..TlsConfig::mtls_client(
                certs.ca.clone(),
                ConnectIdentity {
                    certificate: certs.client_cert.clone(),
                    private_key: certs.client_key.clone(),
                },
            )
        }
    }

    /// Starts a `zenohd` router listening on `tls/127.0.0.1:<port>` with the
    /// server leaf/key. Returns the owning `Messenger` (drop it to stop zenohd)
    /// and the port. `gossip = false`: the router seeds nothing extra here.
    async fn start_tls_router(certs: &Certs) -> (Messenger, u16) {
        start_router_with_tls(TlsConfig::server(certs.cert.clone(), certs.key.clone())).await
    }

    /// A listener that requires every client to present a certificate chained
    /// to the `minica` CA: the shape of a platform project router.
    fn mtls_server_tls(certs: &Certs) -> TlsConfig {
        TlsConfig::mtls_server(certs.cert.clone(), certs.key.clone(), certs.ca.clone())
    }

    /// Like [`start_tls_router`], but with [`mtls_server_tls`].
    async fn start_mtls_router(certs: &Certs) -> (Messenger, u16) {
        start_router_with_tls(mtls_server_tls(certs)).await
    }

    async fn start_router_with_tls(tls: TlsConfig) -> (Messenger, u16) {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve port");
        let port = listener.local_addr().expect("local addr").port();
        drop(listener);

        let adapter = ZenohAdapter::with_router(
            ZenohNetProtocol::Tls,
            "127.0.0.1",
            port,
            false,
            SubscriberBufferSizes::default(),
            Vec::new(),
            Some(tls),
            RouterId::generate(),
        )
        .expect("build tls router adapter");
        let mut messenger = Messenger::new(MessengerAdapter::Zenoh(adapter));
        messenger
            .start_router()
            .await
            .expect("start tls zenohd router");
        (messenger, port)
    }

    /// Opens a `tls/` client session, retrying briefly while the router's TLS
    /// listener finishes settling after the TCP socket starts accepting.
    async fn open_tls_client(port: u16, tls: &TlsConfig) -> ZenohAdapter {
        for _ in 0..40 {
            if let Ok(mut adapter) = ZenohAdapter::connect_to_tls("127.0.0.1", port, tls.clone())
                && adapter.start_session().await.is_ok()
            {
                return adapter;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        panic!("could not open a tls client session on 127.0.0.1:{port}");
    }

    /// Starts a `zenohd` router that serves local clients over plaintext `tcp/`
    /// AND *federates* to a remote `tls/` router at `remote_port`, dialing it
    /// with `upstream_tls`. This is the peppy daemon's shape when the machine is
    /// enrolled in a platform project: local nodes speak plaintext loopback, and
    /// only the inter-router hop is encrypted.
    async fn start_federated_router(remote_port: u16, upstream_tls: TlsConfig) -> (Messenger, u16) {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve port");
        let port = listener.local_addr().expect("local addr").port();
        drop(listener);

        let adapter = ZenohAdapter::with_router(
            ZenohNetProtocol::Tcp,
            "127.0.0.1",
            port,
            false,
            SubscriberBufferSizes::default(),
            vec![format!("tls/127.0.0.1:{remote_port}")],
            Some(upstream_tls),
            RouterId::generate(),
        )
        .expect("build federated router adapter");
        let mut messenger = Messenger::new(MessengerAdapter::Zenoh(adapter));
        messenger
            .start_router()
            .await
            .expect("start federated zenohd router");
        (messenger, port)
    }

    /// Opens a plaintext `tcp/` client session to a local router, retrying while
    /// the listener settles.
    async fn open_plaintext_client(port: u16) -> ZenohAdapter {
        for _ in 0..40 {
            if let Ok(mut adapter) =
                ZenohAdapter::connect_to(ZenohNetProtocol::Tcp, "127.0.0.1", port)
                && adapter.start_session().await.is_ok()
            {
                return adapter;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        panic!("could not open a plaintext client session on 127.0.0.1:{port}");
    }

    /// Positive path: a TLS router + two TLS clients complete the handshake and
    /// deliver a message end-to-end. Proves the rendered `transport.link.tls`
    /// block is valid and the encrypted transport actually carries traffic.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn tls_router_and_client_round_trip() {
        const TOPIC: &str = "tls_round_trip";
        let _lock = ZENOH_SERIAL.lock().await;
        let certs = write_certs();
        let (_router, port) = start_tls_router(&certs).await;
        let client_tls = trusting_client_tls(&certs);

        let subscriber = open_tls_client(port, &client_tls).await;
        let subscription = subscriber
            .subscribe_topic(&receiver(TOPIC), SubscriberQoS::Standard)
            .await
            .expect("subscribe over tls");

        let mut publisher = open_tls_client(port, &client_tls).await;
        wait_for_subscriber_discovery().await;
        publisher
            .publish_topic(
                &sender(TOPIC),
                Payload::from_bytes(Bytes::from_static(b"tls-hello")),
                PublisherQoS::Standard,
                true,
            )
            .await
            .expect("publish over tls");

        let msg = tokio::time::timeout(RECV_TIMEOUT, subscription.rx.recv_async())
            .await
            .expect("timed out waiting for tls message")
            .expect("tls subscription channel closed");
        assert_eq!(msg.payload(), &Bytes::from_static(b"tls-hello"));

        drop(_router); // stop zenohd
    }

    /// Negative path: a client that does NOT trust the router's CA (no
    /// `root_ca_certificate`, so zenoh falls back to system WebPKI roots, which
    /// do not include the private `minica` CA) cannot establish a usable link —
    /// it must receive nothing, while a properly-trusting publisher's message
    /// flows. Proves cert validation is actually enforced (not bypassed).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn tls_client_with_untrusted_ca_receives_nothing() {
        const TOPIC: &str = "tls_untrusted";
        let _lock = ZENOH_SERIAL.lock().await;
        let certs = write_certs();
        let (_router, port) = start_tls_router(&certs).await;

        // Untrusted: no CA provided → WebPKI roots → the minica server cert is
        // not trusted → the TLS link to the router cannot be validated.
        let untrusted = TlsConfig {
            verify_name_on_connect: false,
            ..TlsConfig::default()
        };

        // Confirm the router's TLS listener is actually up *first* (a trusting
        // client opens, retrying while it settles, then is dropped). Otherwise a
        // `start_session` error below could be the router still starting rather
        // than the untrusted CA being rejected — making a readiness failure look
        // like the intended negative result. Same retry pattern as the trusted
        // and plaintext opens in this file.
        drop(open_tls_client(port, &trusting_client_tls(&certs)).await);

        let mut subscriber =
            ZenohAdapter::connect_to_tls("127.0.0.1", port, untrusted).expect("build adapter");
        // In client mode `zenoh::open` succeeds even if the link can't be
        // validated (the failure is async — no data ever flows). The router is
        // now known to be up, so an error here can only be the untrusted CA — a
        // strictly stronger negative result.
        if subscriber.start_session().await.is_err() {
            return;
        }
        let subscription = subscriber
            .subscribe_topic(&receiver(TOPIC), SubscriberQoS::Standard)
            .await
            .expect("subscribe (local declare succeeds even with a dead link)");

        // A correctly-trusting publisher proves traffic *is* flowing on the
        // router — so a no-delivery result is the untrusted link's fault, not a
        // dead test.
        let mut publisher = open_tls_client(port, &trusting_client_tls(&certs)).await;
        wait_for_subscriber_discovery().await;
        publisher
            .publish_topic(
                &sender(TOPIC),
                Payload::from_bytes(Bytes::from_static(b"should-not-arrive")),
                PublisherQoS::Standard,
                true,
            )
            .await
            .expect("publish over trusted tls");

        let delivered = tokio::time::timeout(Duration::from_secs(3), subscription.rx.recv_async())
            .await
            .is_ok();
        assert!(
            !delivered,
            "an untrusted-CA subscriber must not receive any message"
        );

        drop(_router);
    }

    /// How long the inter-router link gets to come up. A link that comes up
    /// does so in tens of milliseconds; a link that the remote router refuses
    /// never does, so this is also how long the negative case waits.
    const LINK_TIMEOUT: Duration = Duration::from_secs(5);

    /// The federated topology: a *remote* router with `remote_tls`, and a
    /// *local* router (plaintext for its own nodes) that dials it with
    /// `upstream_tls`. `linked` is whether the local router's admin space
    /// reported the inter-router link established within [`LINK_TIMEOUT`],
    /// which is the signal the daemon itself waits on.
    struct Federation {
        _remote: Messenger,
        remote_port: u16,
        _local: Messenger,
        local_port: u16,
        linked: bool,
    }

    async fn federate(remote_tls: TlsConfig, upstream_tls: TlsConfig) -> Federation {
        let (remote, remote_port) = start_router_with_tls(remote_tls).await;
        let (local, local_port) = start_federated_router(remote_port, upstream_tls).await;
        let linked = local
            .router_links_probe()
            .expect("a federated router dials an upstream")
            .wait_established(LINK_TIMEOUT)
            .await;
        Federation {
            _remote: remote,
            remote_port,
            _local: local,
            local_port,
            linked,
        }
    }

    /// Publishes `payload` on `topic` into the REMOTE router over TLS with
    /// `publisher_tls`, and returns what a subscriber on the LOCAL router
    /// (attached over plaintext loopback) received within `RECV_TIMEOUT`.
    async fn relay(
        federation: &Federation,
        publisher_tls: &TlsConfig,
        topic: &str,
        payload: &'static [u8],
    ) -> Option<Bytes> {
        let subscriber = open_plaintext_client(federation.local_port).await;
        let subscription = subscriber
            .subscribe_topic(&receiver(topic), SubscriberQoS::Standard)
            .await
            .expect("subscribe on the local router");

        let mut publisher = open_tls_client(federation.remote_port, publisher_tls).await;
        // The subscription must propagate local-router -> (tls federation) ->
        // remote-router before the publish; a single discovery wait is too short
        // for the cross-router hop, so allow a couple of rounds.
        wait_for_subscriber_discovery().await;
        wait_for_subscriber_discovery().await;
        publisher
            .publish_topic(
                &sender(topic),
                Payload::from_bytes(Bytes::from_static(payload)),
                PublisherQoS::Standard,
                true,
            )
            .await
            .expect("publish on the remote router");

        tokio::time::timeout(RECV_TIMEOUT, subscription.rx.recv_async())
            .await
            .ok()
            .map(|received| {
                received
                    .expect("subscription channel open")
                    .payload()
                    .to_bytes()
            })
    }

    /// The federated topology end-to-end: a *local* router (plaintext for its
    /// own nodes) federated over `tls/` to a *remote* router. A subscriber on
    /// the LOCAL router receives a message a publisher sends into the REMOTE
    /// router, proving the two zenohd routers join one network (messages cross
    /// transparently) and that only the inter-router hop is TLS-encrypted. A
    /// plain client-to-router connection could not bridge the local router's
    /// nodes to the remote network.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn federated_routers_relay_across_the_tls_link() {
        let _lock = ZENOH_SERIAL.lock().await;
        let certs = write_certs();

        let federation = federate(
            TlsConfig::server(certs.cert.clone(), certs.key.clone()),
            trusting_client_tls(&certs),
        )
        .await;
        assert!(
            federation.linked,
            "the local router linked to the remote one"
        );

        let received = relay(
            &federation,
            &trusting_client_tls(&certs),
            "federation_round_trip",
            b"across-the-federation",
        )
        .await;
        assert_eq!(received, Some(Bytes::from_static(b"across-the-federation")));
    }

    /// The platform shape end-to-end: the remote router requires client
    /// certificates, and the local router federates to it presenting the
    /// identity a `TlsConfig::mtls_client` names. A message published into the
    /// remote router reaches a subscriber on the local one, proving the rendered
    /// `connect_certificate`/`connect_private_key`/`enable_mtls` keys are the
    /// ones zenoh reads and that the mutual handshake carries traffic.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mtls_federated_routers_relay_across_the_link() {
        let _lock = ZENOH_SERIAL.lock().await;
        let certs = write_certs();

        let federation = federate(mtls_server_tls(&certs), identified_client_tls(&certs)).await;
        assert!(
            federation.linked,
            "the identified local router linked to the mTLS remote"
        );

        let received = relay(
            &federation,
            &identified_client_tls(&certs),
            "mtls_federation_round_trip",
            b"across-mtls",
        )
        .await;
        assert_eq!(received, Some(Bytes::from_static(b"across-mtls")));
    }

    /// Negative path for mutual TLS: a local router that trusts the remote CA
    /// but presents no identity is refused by a listener that requires one, so
    /// no inter-router link is ever established, while an identified client
    /// proves the remote router is serving.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_router_requiring_client_certificates_rejects_an_identity_free_link() {
        let _lock = ZENOH_SERIAL.lock().await;
        let certs = write_certs();

        let federation = federate(mtls_server_tls(&certs), trusting_client_tls(&certs)).await;
        // The remote router is up and serves identified clients, so the absent
        // link below is the missing identity's doing, not a router still starting.
        drop(open_tls_client(federation.remote_port, &identified_client_tls(&certs)).await);
        assert!(
            !federation.linked,
            "a router without a client certificate must not join an mTLS router"
        );
    }

    /// The daemon's link probe presents the same identity the router does, so
    /// against a listener that requires client certificates it completes the
    /// handshake exactly as the router would.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn probe_tls_reachable_presents_the_client_identity() {
        let _lock = ZENOH_SERIAL.lock().await;
        let certs = write_certs();
        let (_remote, remote_port) = start_mtls_router(&certs).await;
        // Wait for the listener to settle, as the client opens above do.
        drop(open_tls_client(remote_port, &identified_client_tls(&certs)).await);

        let tls = TlsConfig::mtls_client(
            certs.ca.clone(),
            ConnectIdentity {
                certificate: certs.client_cert.clone(),
                private_key: certs.client_key.clone(),
            },
        );
        // The server leaf's SAN is `localhost`, which is what the probe verifies
        // the name against (it always verifies), so dial by name.
        probe_tls_reachable("localhost", remote_port, &tls, Duration::from_secs(5))
            .await
            .expect("an identified probe completes the mutual handshake");

        let untrusted = TlsConfig::mtls_client(
            certs.client_cert.clone(),
            ConnectIdentity {
                certificate: certs.client_cert.clone(),
                private_key: certs.client_key.clone(),
            },
        );
        probe_tls_reachable("localhost", remote_port, &untrusted, Duration::from_secs(5))
            .await
            .expect_err("a probe that does not trust the router's CA fails the handshake");

        drop(_remote);
    }
}
