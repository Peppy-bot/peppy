//! Unit tests for [`pmi::probe_tls_reachable`]: a raw rustls handshake against a
//! tokio-rustls TLS server built from the same fixtures as `tests/zenoh_tls.rs`
//! (`minica_ca.pem` CA, `server_localhost.pem` leaf with SAN `localhost`, and its
//! key). Deliberately needs neither `build_zenoh` nor a compiled `zenohd`: the
//! probe is a pure TLS handshake, so the test stands up its own bare TLS acceptor
//! rather than a zenoh router.

#![cfg(feature = "zenoh")]

mod probe_tls_tests {
    use pmi::{TlsConfig, probe_tls_reachable};
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio_rustls::TlsAcceptor;
    use tokio_rustls::rustls::ServerConfig;

    const CA_PEM: &[u8] = include_bytes!("fixtures/minica_ca.pem");
    const SERVER_CERT_PEM: &[u8] = include_bytes!("fixtures/server_localhost.pem");
    const SERVER_KEY_PEM: &[u8] = include_bytes!("fixtures/server_localhost.key");

    const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

    /// Writes the embedded CA PEM to a tempfile and returns it. `TlsConfig::client`
    /// takes a filesystem path, so the fixture bytes are materialized. The returned
    /// `TempPath` keeps the file alive until dropped at end of test.
    fn ca_tempfile() -> tempfile::TempPath {
        let mut f = tempfile::NamedTempFile::new().expect("create CA tempfile");
        f.write_all(CA_PEM).expect("write CA tempfile");
        f.flush().expect("flush CA tempfile");
        f.into_temp_path()
    }

    /// Builds a tokio-rustls `TlsAcceptor` serving the `localhost` server leaf and
    /// its key (loaded from the embedded fixtures).
    fn server_acceptor() -> TlsAcceptor {
        let chain = rustls_pemfile::certs(&mut &SERVER_CERT_PEM[..])
            .collect::<Result<Vec<_>, _>>()
            .expect("parse server cert chain");
        let key = rustls_pemfile::private_key(&mut &SERVER_KEY_PEM[..])
            .expect("read server key")
            .expect("server key present");
        // Name the `ring` crypto provider explicitly: rustls is built with both
        // its `ring` and `aws-lc-rs` features in this tree, so it cannot pick a
        // process-default provider and a bare `ServerConfig::builder()` panics.
        let config = ServerConfig::builder_with_provider(Arc::new(
            tokio_rustls::rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("default protocol versions")
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .expect("build server config");
        TlsAcceptor::from(Arc::new(config))
    }

    /// Binds a TLS acceptor on `127.0.0.1:0`, spawns an accept loop that performs
    /// the TLS handshake on each connection (then drops it), and returns the bound
    /// port. The loop task lives for the duration of the test process.
    async fn start_tls_server() -> u16 {
        let acceptor = server_acceptor();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind tls test listener");
        let port = listener.local_addr().expect("local addr").port();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    // Complete the handshake, then drop — the probe only needs the
                    // handshake to finish to consider the link validated.
                    let _ = acceptor.accept(stream).await;
                });
            }
        });
        port
    }

    /// Correct CA + matching server name (`localhost` SAN) → the handshake
    /// validates. We dial `localhost`, which resolves to the `127.0.0.1` listener
    /// while still matching the leaf's SAN.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn correct_ca_and_name_succeeds() {
        let port = start_tls_server().await;
        let ca = ca_tempfile();
        let tls = TlsConfig::client(PathBuf::from(&*ca));

        let result = probe_tls_reachable("localhost", port, &tls, PROBE_TIMEOUT).await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");
    }

    /// No CA → system roots, which do not trust the private `minica` CA, so the
    /// chain cannot be validated.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn untrusted_system_roots_fails() {
        let port = start_tls_server().await;
        let tls = TlsConfig::default();

        let result = probe_tls_reachable("localhost", port, &tls, PROBE_TIMEOUT).await;
        assert!(
            result.is_err(),
            "expected Err (untrusted CA), got {result:?}"
        );
    }

    /// Correct CA but a server name (`127.0.0.1`) that does not match the leaf's
    /// SAN (`localhost`) → name verification (always on) rejects it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn name_mismatch_fails() {
        let port = start_tls_server().await;
        let ca = ca_tempfile();
        let tls = TlsConfig::client(PathBuf::from(&*ca));

        let result = probe_tls_reachable("127.0.0.1", port, &tls, PROBE_TIMEOUT).await;
        assert!(
            result.is_err(),
            "expected Err (name mismatch), got {result:?}"
        );
    }

    /// A peer that completes the TCP accept but never drives the TLS handshake
    /// must fail within ~`timeout` TOTAL (the connect + handshake share one
    /// deadline), not hang or stretch toward 2x. Guards the single-deadline bound
    /// the daemon's federation ack budget relies on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stalled_handshake_fails_within_total_timeout() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind stalling listener");
        let port = listener.local_addr().expect("local addr").port();
        // Accept connections but never perform the TLS handshake — hold the raw
        // TCP streams open so the probe's handshake phase stalls to its deadline.
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });

        let ca = ca_tempfile();
        let tls = TlsConfig::client(PathBuf::from(&*ca));

        let timeout = Duration::from_secs(1);
        let start = tokio::time::Instant::now();
        let result = probe_tls_reachable("localhost", port, &tls, timeout).await;
        let elapsed = start.elapsed();

        assert!(
            result.is_err(),
            "a stalled handshake must fail, got {result:?}"
        );
        // Single shared deadline ⇒ the whole probe stays within ~timeout (allowing
        // generous scheduling slack), never ~2x.
        assert!(
            elapsed < timeout + Duration::from_millis(800),
            "probe must be bounded by ~timeout total, took {elapsed:?}"
        );
    }

    /// A port with no listener → the connect (or handshake) fails within the
    /// timeout, returning `Err` rather than hanging.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unreachable_port_fails_within_timeout() {
        // Reserve then release a port so nothing is listening on it.
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("reserve port");
        let port = listener.local_addr().expect("local addr").port();
        drop(listener);

        let ca = ca_tempfile();
        let tls = TlsConfig::client(PathBuf::from(&*ca));

        let result = probe_tls_reachable("127.0.0.1", port, &tls, Duration::from_secs(2)).await;
        assert!(
            result.is_err(),
            "expected Err (unreachable port), got {result:?}"
        );
    }
}
