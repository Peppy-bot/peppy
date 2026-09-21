//! Generation of Zenoh session and router configs.
//!
//! This is the single source of truth that replaced the former set of per-shape
//! Askama templates (one each for the fail-fast client, the reconnecting client,
//! the watchdog probe, and the router). A typed [`ZenohConfigSpec`] keeps the
//! mode / connect / listen / scouting / timestamping settings in one place so
//! adding a knob touches one struct instead of four string templates.
//!
//! ## Discovery model: gossip-only, multicast off
//!
//! Every generated config disables multicast scouting and relies on gossip
//! seeded by the configured connect endpoints (the router). Nodes open a `peer`
//! session that connects to the router, learn each other's locators via gossip,
//! and then form direct peer-to-peer links so data no longer relays through the
//! router. Multicast is left off on purpose: on a shared host it bridges
//! otherwise-independent peer groups (and would cross-link unrelated test runs),
//! and with a known seed it adds nothing gossip does not already cover.

use crate::router_id::RouterId;
use crate::zenohd::ZenohNetProtocol;
use config::namespace::Namespace;
use serde_json::json;
use std::path::PathBuf;

/// TLS material for a `tls/` (or `quic/`) session, rendered into the zenoh
/// `transport.link.tls` block. One type serves both roles: a router/listener
/// sets `listen_certificate`/`listen_private_key` (its server identity); a
/// client sets `root_ca_certificate` (to verify that server) and
/// `verify_name_on_connect`. Links are one-way TLS throughout: the server
/// proves its identity, the client proves nothing at the transport, and no
/// client certificate is ever presented. The keys map 1:1 to zenoh 1.10's
/// `transport.link.tls.*` (verified against its `DEFAULT_CONFIG.json5`); unset
/// path fields are omitted so a non-TLS config renders byte-identical to before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsConfig {
    /// CA used to validate the peer's certificate. Required on the client side
    /// for a private CA (dev/internal); omit to fall back to system WebPKI.
    pub root_ca_certificate: Option<PathBuf>,
    /// Listener (router/server) certificate chain.
    pub listen_certificate: Option<PathBuf>,
    /// Listener (router/server) private key.
    pub listen_private_key: Option<PathBuf>,
    /// Verify the server cert's name matches the dialed host (client side).
    /// zenoh defaults this to `true`; keep it on unless a test needs otherwise.
    pub verify_name_on_connect: bool,
}

impl Default for TlsConfig {
    fn default() -> Self {
        Self {
            root_ca_certificate: None,
            listen_certificate: None,
            listen_private_key: None,
            verify_name_on_connect: true,
        }
    }
}

/// Verify that a TLS endpoint at `host:port` is reachable AND that its
/// certificate actually validates against the trust configured in `tls`,
/// completing a real TLS handshake within `timeout` *total* — the TCP connect
/// and the TLS handshake share one deadline, so the whole call is bounded by
/// `timeout` (not `timeout` per phase). The caller relies on this single bound
/// to keep the probe inside its federation ack budget.
///
/// ## Why a raw handshake and not `zenoh::open`
///
/// In zenoh *client* mode `zenoh::open` returns `Ok` as soon as the local
/// session is created — even if the configured connect endpoint cannot be
/// reached or its certificate cannot be validated. The link failure is
/// asynchronous and silent (no data ever flows, but nothing errors), so a
/// successful `open` is NOT a usable "the federation link validates" signal.
/// The only deterministic way to know the link is good is to perform the TLS
/// handshake ourselves and observe the result. This probe does exactly that
/// with rustls directly (no zenoh session involved): a TCP connect followed by
/// a full TLS handshake that trusts the configured roots and verifies the
/// server name. `Ok(())` is returned iff the chain is trusted and the server
/// name matches; any other outcome returns a human-readable `Err` naming
/// `host:port` (it is surfaced into a user-facing CLI error).
///
/// Name verification is intentionally left ON (rustls's default) regardless of
/// `tls.verify_name_on_connect`: this probe is the authoritative "does the link
/// validate" check, so it always validates fully.
pub async fn probe_tls_reachable(
    host: &str,
    port: u16,
    tls: &TlsConfig,
    timeout: std::time::Duration,
) -> Result<(), String> {
    use std::sync::Arc;
    use tokio_rustls::TlsConnector;
    use tokio_rustls::rustls::pki_types::ServerName;
    use tokio_rustls::rustls::{ClientConfig, RootCertStore};

    // Build the trust anchors: either an explicit private CA, or the OS roots.
    let mut roots = RootCertStore::empty();
    match &tls.root_ca_certificate {
        Some(path) => {
            let bytes = std::fs::read(path)
                .map_err(|e| format!("read root CA `{}` failed: {e}", path.display()))?;
            let mut added = 0usize;
            for cert in rustls_pemfile::certs(&mut &bytes[..]) {
                let cert =
                    cert.map_err(|e| format!("parse root CA `{}` failed: {e}", path.display()))?;
                roots
                    .add(cert)
                    .map_err(|e| format!("add root CA `{}` failed: {e}", path.display()))?;
                added += 1;
            }
            if added == 0 {
                return Err(format!(
                    "root CA `{}` contained no certificates",
                    path.display()
                ));
            }
        }
        None => {
            // rustls-native-certs 0.8 returns a `CertificateResult`; take all of
            // its `.certs` (any per-cert load errors are non-fatal as long as at
            // least one root ends up trusted).
            let native = rustls_native_certs::load_native_certs();
            for cert in native.certs {
                let _ = roots.add(cert);
            }
            if roots.is_empty() {
                return Err(
                    "system trust store is empty (no native roots could be loaded)".to_string(),
                );
            }
        }
    }

    // Select the crypto backend explicitly rather than relying on rustls's
    // process-default provider. In this dependency tree rustls is built with BOTH
    // its `ring` and `aws-lc-rs` features enabled (zenoh's stack pulls `aws-lc-rs`
    // via tokio-rustls's default, while quinn/tls-listener pull `ring`), so rustls
    // cannot auto-determine a single default provider and a bare
    // `ClientConfig::builder()` would panic. tokio-rustls's default features
    // guarantee `ring` is available, so we name it directly.
    let config = ClientConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| format!("TLS provider setup failed: {e}"))?
    .with_root_certificates(roots)
    .with_no_client_auth();

    let server_name = ServerName::try_from(host.to_string())
        .map_err(|e| format!("invalid server name `{host}`: {e}"))?;

    // One deadline for the whole probe (TCP connect + TLS handshake), so a peer
    // that accepts the TCP connection but stalls the handshake cannot stretch the
    // call to ~2x `timeout` — the total stays bounded by `timeout`.
    let deadline = tokio::time::Instant::now() + timeout;

    let tcp = match tokio::time::timeout_at(deadline, tokio::net::TcpStream::connect((host, port)))
        .await
    {
        Err(_) => {
            return Err(format!(
                "connect to {host}:{port} timed out after {timeout:?}"
            ));
        }
        Ok(Err(e)) => return Err(format!("connect to {host}:{port} failed: {e}")),
        Ok(Ok(tcp)) => tcp,
    };

    // Finish the handshake under the same deadline. tokio-rustls surfaces
    // validation failures (UnknownIssuer / unknown CA / bad server name) as an
    // io::Error whose message contains the rustls reason, so `{e}` carries the
    // cause.
    let connector = TlsConnector::from(Arc::new(config));
    match tokio::time::timeout_at(deadline, connector.connect(server_name, tcp)).await {
        Err(_) => Err(format!(
            "TLS handshake to {host}:{port} timed out after {timeout:?}"
        )),
        Ok(Err(e)) => Err(format!("TLS handshake to {host}:{port} failed: {e}")),
        Ok(Ok(_stream)) => Ok(()),
    }
}

impl TlsConfig {
    /// Server (router/listener) identity: a leaf certificate chain + its key.
    pub fn server(certificate: PathBuf, private_key: PathBuf) -> Self {
        Self {
            listen_certificate: Some(certificate),
            listen_private_key: Some(private_key),
            ..Self::default()
        }
    }

    /// Client trust: the CA that signed the router's certificate. Name
    /// verification stays on.
    pub fn client(root_ca_certificate: PathBuf) -> Self {
        Self {
            root_ca_certificate: Some(root_ca_certificate),
            ..Self::default()
        }
    }
}

/// The Zenoh roles this codebase generates configs for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionMode {
    /// A node/daemon session: connects to a seed (the router), then forms direct
    /// peer-to-peer links with peers discovered via gossip.
    Peer,
    /// A short-lived liveness probe: connects to exactly one router endpoint and
    /// never discovers peers, so "is *our* router up?" stays deterministic.
    Client,
    /// The zenohd router itself.
    Router,
}

/// Inputs for [`build_zenoh_config`].
pub(crate) struct ZenohConfigSpec {
    pub mode: SessionMode,
    /// Seed endpoints to dial (`<proto>/<host>:<port>`). Empty for the router.
    pub connect_endpoints: Vec<String>,
    /// Endpoints to listen on. Empty leaves Zenoh's default (clients do not
    /// listen). Peers listen on a loopback ephemeral port by default.
    pub listen_endpoints: Vec<String>,
    /// Retry the connection forever (`timeout_ms: -1`, `exit_on_failure: false`)
    /// instead of failing fast. Used by long-lived sessions so a router restart
    /// is recovered transparently.
    pub reconnect: bool,
    /// Enable gossip scouting so peers sharing a seed discover each other and
    /// form direct links. Multicast scouting is always off (see module docs).
    pub gossip: bool,
    /// TLS material for a `tls/` endpoint. `None` for plaintext transports
    /// (`tcp/`, …), in which case no `transport.link.tls` block is rendered and
    /// the output is byte-identical to a pre-TLS config.
    pub tls: Option<TlsConfig>,
    /// Workspace namespace for an application session. Rendered into the
    /// session-level `namespace` field for
    /// `Peer`/`Client` sessions only; `None` for the router and for liveness
    /// probes. Zenoh prepends `<ns>/` to every declared key on egress and strips
    /// it on ingress, so two sessions interoperate iff their namespaces match.
    pub namespace: Option<Namespace>,
    /// The transport identity to pin as the config's `id`. `Some` for every
    /// router (see [`router_spec`]), `None` for the peer/client/probe sessions,
    /// which are short-lived or process-scoped and have nothing to correlate
    /// across restarts. Unpinned, zenoh mints a fresh random zid on every start.
    pub id: Option<RouterId>,
}

/// Builds the JSON5-equivalent config value for a session or the router.
pub(crate) fn build_zenoh_config(spec: &ZenohConfigSpec) -> serde_json::Value {
    let mode = match spec.mode {
        SessionMode::Peer => "peer",
        SessionMode::Client => "client",
        SessionMode::Router => "router",
    };

    let mut config = json!({
        "mode": mode,
        "scouting": {
            "multicast": { "enabled": false },
            "gossip": { "enabled": spec.gossip }
        }
    });

    // The pinned transport identity. Without it zenoh mints a fresh random zid
    // per start, so nothing observing this process's sessions (another router's
    // session list, this one's own logs) can tie two runs together. Only routers
    // pin one; see `ZenohConfigSpec::id`.
    if let Some(id) = &spec.id {
        config["id"] = json!(id.as_str());
    }

    if !spec.connect_endpoints.is_empty() {
        let mut connect = json!({ "endpoints": spec.connect_endpoints });
        if spec.reconnect {
            connect["timeout_ms"] = json!(-1);
            connect["exit_on_failure"] = json!(false);
            connect["retry"] = json!({
                "period_init_ms": 1000,
                "period_max_ms": 4000,
                "period_increase_factor": 2.0
            });
        }
        config["connect"] = connect;
    }

    if !spec.listen_endpoints.is_empty() {
        // Zenoh reads `listen.endpoints` against the session's OWN mode, so the
        // key must match `mode` above — the same per-role-map rule as
        // `timestamping` below. Deriving it exhaustively (rather than
        // `else => "peer"`) keeps a client's endpoints from being silently
        // misfiled under `peer` if one ever listens.
        let role = match spec.mode {
            SessionMode::Router => "router",
            SessionMode::Peer => "peer",
            SessionMode::Client => "client",
        };
        config["listen"] = json!({ "endpoints": { role: spec.listen_endpoints } });
    }

    // Stamp data at the producer so consumers can measure real delivery latency.
    // Zenoh matches `enabled` against the session's OWN mode, so the key must
    // match `mode` above — a `peer` session ignores an `enabled.client` entry.
    // Stamping at the source keeps peer mode (no router in the direct path) on
    // par with router mode, where the client/router already stamps.
    config["timestamping"] = match spec.mode {
        SessionMode::Router => {
            json!({ "enabled": { "router": true }, "drop_future_timestamp": false })
        }
        SessionMode::Peer => json!({ "enabled": { "peer": true }, "drop_future_timestamp": false }),
        SessionMode::Client => {
            json!({ "enabled": { "client": true }, "drop_future_timestamp": false })
        }
    };

    // Workspace routing namespace. zenoh's `namespace` is a
    // session-level field: it is applied to the application session opened
    // against a router, where egress prepends `<ns>/` to every declared key and
    // ingress strips it — so two sessions interoperate iff their namespaces
    // match. It is rendered only for `Peer`/`Client` application sessions, never
    // for the router: a router only forwards between transport faces and never
    // opens an application session, so a router-level `namespace` would NOT
    // prefix forwarded/federated traffic. `router_spec` and `render_probe_config`
    // therefore pass `namespace: None` — readiness probes are deliberately
    // namespace-free (they only check "is our router up?").
    if let Some(namespace) = &spec.namespace
        && !matches!(spec.mode, SessionMode::Router)
    {
        config["namespace"] = json!(namespace.as_str());
    }

    // TLS material maps to zenoh's `transport.link.tls`. Rendered only when
    // present so a plaintext config is byte-identical to before. The output is a
    // pure function of [`TlsConfig`]: every field renders, and nothing else does.
    // Path fields are omitted when unset (zenoh treats a missing key as `null`);
    // `verify_name_on_connect` always renders, having no "absent" meaning.
    //
    // Nothing emits `enable_mtls` or the `connect_certificate`/`connect_private_key`
    // pair: mTLS is not a mode this codebase has, so there is no field behind those
    // keys and no literal standing in for one. zenoh leaves mTLS off when the key is
    // absent, which is the behaviour we want.
    if let Some(tls) = &spec.tls {
        let mut tls_json = json!({
            "verify_name_on_connect": tls.verify_name_on_connect,
        });
        let mut put_path = |key: &str, value: &Option<PathBuf>| {
            if let Some(path) = value {
                tls_json[key] = json!(path.to_string_lossy());
            }
        };
        put_path("root_ca_certificate", &tls.root_ca_certificate);
        put_path("listen_certificate", &tls.listen_certificate);
        put_path("listen_private_key", &tls.listen_private_key);
        config["transport"] = json!({ "link": { "tls": tls_json } });
    }

    config
}

/// Serializes a spec to a JSON string (valid JSON5) for the zenohd config file.
pub(crate) fn render_config_string(spec: &ZenohConfigSpec) -> String {
    serde_json::to_string(&build_zenoh_config(spec)).expect("zenoh config value serializes to JSON")
}

/// Renders a spec into a parsed [`zenoh::config::Config`] for opening a session.
pub(crate) fn render_config(spec: &ZenohConfigSpec) -> zenoh::config::Config {
    zenoh::config::Config::from_json5(&render_config_string(spec))
        .expect("generated zenoh config parses")
}

/// Rewrites the unroutable wildcard bind address to a connectable loopback host.
pub(crate) fn connectable_host(host: &str) -> String {
    if host == "0.0.0.0" {
        "127.0.0.1".to_string()
    } else {
        host.to_string()
    }
}

/// The liveness-probe config shared by the router watchdog and the
/// ephemeral-router readiness check: a plain client targeting one router
/// endpoint, no peer discovery. Only the `router` paths probe, so this is
/// `router`-gated (a `zenoh`-without-`router` consumer — e.g. the backend, which
/// only renders configs — would otherwise see it as dead code).
#[cfg(feature = "router")]
pub(crate) fn render_probe_config(
    protocol: ZenohNetProtocol,
    host: &str,
    port: u16,
    tls: Option<TlsConfig>,
) -> zenoh::config::Config {
    render_config(&ZenohConfigSpec {
        mode: SessionMode::Client,
        connect_endpoints: vec![format!("{protocol}/{}:{port}", connectable_host(host))],
        listen_endpoints: Vec::new(),
        reconnect: false,
        gossip: false,
        // A `tls/` router needs the probe to speak TLS too; plaintext callers pass
        // `None` (no `transport.link.tls` block, byte-identical to before).
        tls,
        // Readiness probes are deliberately namespace-free: a probe only checks
        // "is our router up?", which a namespace would only get in the way of.
        namespace: None,
        // A probe session is opened, used once, and dropped, so it has no
        // identity worth correlating across restarts.
        id: None,
    })
}

/// The config spec for a zenohd router listening on `protocol/host:port`. Shared
/// by the in-process spawn path ([`crate::zenohd::router_config_path`]) and the
/// out-of-process render path ([`render_router_config`]) so both produce an
/// identical router config. `gossip` seeds the peer mesh (the daemon wants it on;
/// an isolated per-user router wants it off so routers cannot mesh).
///
/// `connect_endpoints` makes this router *federate* to other zenohd routers it
/// dials (`<proto>/<host>:<port>` each) — distinct from gossip, which is peer
/// auto-discovery. Empty is the standalone router (today's behavior); a non-empty
/// list (e.g. the daemon's local router dialing a remote `tls/` router) turns the
/// retry/keep-alive on (`reconnect`) so an unreachable or restarted upstream is
/// recovered transparently and never stops the local router serving its own
/// nodes. The connect-side trust for that link rides in `tls` (a
/// [`TlsConfig::client`], which only names the CA to validate the upstream
/// against); it is ignored on a plaintext listen endpoint.
///
/// `id` pins the router's transport identity, and is required rather than
/// optional on purpose: an `Option` would leave two behaviours to reason about
/// and test, and would let a caller silently fall back to zenoh's fresh-random
/// zid, which makes the router anonymous to anything observing its sessions
/// (see [`RouterId`]).
pub(crate) fn router_spec(
    protocol: ZenohNetProtocol,
    host: &str,
    port: u16,
    gossip: bool,
    connect_endpoints: Vec<String>,
    tls: Option<TlsConfig>,
    id: &RouterId,
) -> ZenohConfigSpec {
    ZenohConfigSpec {
        mode: SessionMode::Router,
        reconnect: !connect_endpoints.is_empty(),
        connect_endpoints,
        listen_endpoints: vec![format!("{protocol}/{host}:{port}")],
        gossip,
        tls,
        // A router is never namespaced: it only forwards between transport faces
        // and never opens an application session, so a router-level namespace
        // would not prefix forwarded/federated traffic. Isolation is enforced on
        // the application sessions instead.
        namespace: None,
        id: Some(id.clone()),
    }
}

/// Renders a zenohd router config to a JSON5 string, for callers that run the
/// router out of process (e.g. a container) rather than spawning it via
/// [`crate::ZenohAdapter::with_router`]. With `protocol = Tls` and a server
/// [`TlsConfig`] this emits the `transport.link.tls` listener block; a non-empty
/// `connect_endpoints` emits a `connect` block so the router federates to those
/// upstreams (see [`router_spec`]). Available under the base `zenoh` feature (no
/// `router`/zenohd binary needed) because rendering a config is independent of
/// spawning a process.
///
/// `id` is the router's pinned transport identity and is required; see
/// [`router_spec`] for why it is not an `Option`.
pub fn render_router_config(
    protocol: ZenohNetProtocol,
    host: &str,
    port: u16,
    gossip: bool,
    connect_endpoints: Vec<String>,
    tls: Option<TlsConfig>,
    id: &RouterId,
) -> String {
    render_config_string(&router_spec(
        protocol,
        host,
        port,
        gossip,
        connect_endpoints,
        tls,
        id,
    ))
}

/// The wildcard ephemeral listen endpoint a peer binds. Zenoh 1.10 gossips
/// only non-loopback locators (`LinkState` filters loopback addresses out of
/// the advertised set), so a loopback-only listener is never advertised and
/// direct peer links can never form: peers would sit behind the router with
/// no route to each other. Binding the wildcard address makes zenoh advertise
/// every routable interface address instead, which co-located peers can dial
/// (a host can always reach its own interface addresses) and remote peers
/// could too, should a mesh ever span hosts. The trade is that the listener
/// leaves loopback: it is reachable from the network. That widens an existing
/// exposure class rather than opening a new one — the zenoh transport is
/// unauthenticated, and the router's own listener is already bound to
/// `0.0.0.0` — but a deployment that must not accept inbound peer links can
/// set `gossip: false` in its discovery config to keep every session a
/// relay-only client with no listener at all.
pub(crate) fn peer_listen_endpoint(protocol: ZenohNetProtocol) -> String {
    format!("{protocol}/0.0.0.0:0")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed router identity for the render tests, so an assertion on the
    /// emitted `id` pins a literal rather than whatever was just minted.
    fn test_router_id() -> RouterId {
        RouterId::parse("7f3a9c1e").expect("a valid router id literal")
    }

    fn peer_spec(reconnect: bool, gossip: bool) -> ZenohConfigSpec {
        ZenohConfigSpec {
            mode: SessionMode::Peer,
            connect_endpoints: vec!["tcp/127.0.0.1:7448".to_string()],
            listen_endpoints: vec![peer_listen_endpoint(ZenohNetProtocol::Tcp)],
            reconnect,
            gossip,
            tls: None,
            namespace: None,
            id: None,
        }
    }

    #[test]
    fn peer_config_has_peer_mode_listen_and_gossip_only_scouting() {
        let cfg = build_zenoh_config(&peer_spec(false, true));

        assert_eq!(cfg["mode"], "peer");
        assert_eq!(cfg["connect"]["endpoints"][0], "tcp/127.0.0.1:7448");
        // Peers listen on a wildcard ephemeral port under the per-mode key, so
        // zenoh 1.10+ gossips the host's routable addresses for direct links.
        assert_eq!(cfg["listen"]["endpoints"]["peer"][0], "tcp/0.0.0.0:0");
        // Discovery is gossip-only.
        assert_eq!(cfg["scouting"]["multicast"]["enabled"], false);
        assert_eq!(cfg["scouting"]["gossip"]["enabled"], true);
        // A peer session stamps under its own role, not `client` — Zenoh reads
        // `enabled` against the session's mode, so an `enabled.client` entry here
        // would be silently ignored and leave samples unstamped.
        assert_eq!(cfg["timestamping"]["enabled"]["peer"], true);
        assert_eq!(cfg["timestamping"]["drop_future_timestamp"], false);
        assert!(cfg["timestamping"]["enabled"].get("client").is_none());
        // Fail-fast: no infinite-retry connect block.
        assert!(cfg["connect"].get("timeout_ms").is_none());
    }

    #[test]
    fn reconnecting_peer_config_retries_forever() {
        let cfg = build_zenoh_config(&peer_spec(true, true));

        assert_eq!(cfg["mode"], "peer");
        assert_eq!(cfg["connect"]["timeout_ms"], -1);
        assert_eq!(cfg["connect"]["exit_on_failure"], false);
        assert_eq!(cfg["connect"]["retry"]["period_init_ms"], 1000);
        assert_eq!(cfg["connect"]["retry"]["period_max_ms"], 4000);
    }

    #[test]
    fn gossip_can_be_disabled_to_force_router_relay() {
        let cfg = build_zenoh_config(&peer_spec(false, false));
        assert_eq!(cfg["scouting"]["gossip"]["enabled"], false);
    }

    #[test]
    fn probe_config_is_a_multicast_free_client() {
        let cfg = build_zenoh_config(&ZenohConfigSpec {
            mode: SessionMode::Client,
            connect_endpoints: vec!["tcp/127.0.0.1:7448".to_string()],
            listen_endpoints: Vec::new(),
            reconnect: false,
            gossip: false,
            tls: None,
            namespace: None,
            id: None,
        });

        assert_eq!(cfg["mode"], "client");
        assert_eq!(cfg["scouting"]["multicast"]["enabled"], false);
        // A client stamps under its own role so its outgoing data carries a
        // source timestamp without depending on the router to add one.
        assert_eq!(cfg["timestamping"]["enabled"]["client"], true);
        // A client never listens for inbound peers.
        assert!(cfg.get("listen").is_none());
    }

    #[test]
    fn client_listen_endpoints_land_under_the_client_key() {
        // Clients do not listen today, so this guards the per-role-map rule
        // directly: were a client ever given a listen endpoint, it must land
        // under its own `client` key, not be silently misfiled under `peer`
        // (the same mismatch that left peer-mode samples unstamped).
        let cfg = build_zenoh_config(&ZenohConfigSpec {
            mode: SessionMode::Client,
            connect_endpoints: vec!["tcp/127.0.0.1:7448".to_string()],
            listen_endpoints: vec!["tcp/127.0.0.1:0".to_string()],
            reconnect: false,
            gossip: false,
            tls: None,
            namespace: None,
            id: None,
        });

        assert_eq!(cfg["mode"], "client");
        assert_eq!(cfg["listen"]["endpoints"]["client"][0], "tcp/127.0.0.1:0");
        assert!(cfg["listen"]["endpoints"].get("peer").is_none());
    }

    #[test]
    fn router_config_listens_under_router_key_with_router_timestamping() {
        let cfg = build_zenoh_config(&ZenohConfigSpec {
            mode: SessionMode::Router,
            connect_endpoints: Vec::new(),
            listen_endpoints: vec!["tcp/0.0.0.0:7448".to_string()],
            reconnect: false,
            gossip: true,
            tls: None,
            namespace: None,
            id: Some(test_router_id()),
        });

        assert_eq!(cfg["mode"], "router");
        assert_eq!(cfg["listen"]["endpoints"]["router"][0], "tcp/0.0.0.0:7448");
        assert_eq!(cfg["timestamping"]["enabled"]["router"], true);
        assert_eq!(cfg["timestamping"]["drop_future_timestamp"], false);
        // Routers do not dial out.
        assert!(cfg.get("connect").is_none());
    }

    /// The wire shape the whole network-status feature rests on: a rendered
    /// router config carries *exactly* the id it was given, under zenoh's own
    /// `id` key, and zenoh accepts it. If this drifts, every router silently
    /// falls back to a fresh random zid per start and the platform's session
    /// list becomes anonymous again.
    #[test]
    fn a_rendered_router_config_pins_exactly_the_id_it_was_given() {
        let id = test_router_id();
        let rendered = render_router_config(
            ZenohNetProtocol::Tcp,
            "0.0.0.0",
            7448,
            true,
            Vec::new(),
            None,
            &id,
        );

        let cfg: serde_json::Value =
            serde_json::from_str(&rendered).expect("a rendered router config is JSON");
        assert_eq!(cfg["id"], serde_json::json!(id.as_str()));
        // And zenoh reads it back as the very id it will run under. Its config
        // struct is `deny_unknown_fields`, so this also proves `id` is a real key
        // rather than one silently ignored.
        let parsed = zenoh::config::Config::from_json5(&rendered)
            .expect("a rendered router config with a pinned id parses");
        let stored: serde_json::Value = serde_json::from_str(
            &parsed
                .get_json("id")
                .expect("zenoh exposes the configured id under its own `id` key"),
        )
        .expect("zenoh returns the id as JSON");
        assert_eq!(
            stored,
            serde_json::json!(id.as_str()),
            "zenoh must run under the very id that was rendered, or the platform's \
             session list cannot be matched against it"
        );
    }

    /// Only routers pin an identity. A peer/client session is process-scoped and
    /// identified by its declarations, so rendering an `id` for one would pin an
    /// identity nothing reads.
    #[test]
    fn session_configs_pin_no_id() {
        assert!(
            build_zenoh_config(&peer_spec(true, true))
                .get("id")
                .is_none()
        );
        assert!(
            build_zenoh_config(&ZenohConfigSpec {
                mode: SessionMode::Client,
                connect_endpoints: vec!["tcp/127.0.0.1:7448".to_string()],
                listen_endpoints: Vec::new(),
                reconnect: false,
                gossip: false,
                tls: None,
                namespace: None,
                id: None,
            })
            .get("id")
            .is_none()
        );
    }

    #[test]
    fn generated_session_configs_parse_as_zenoh_config() {
        // Guards the JSON5 schema: a malformed block would otherwise only
        // surface as a panic at session/router open.
        render_config(&peer_spec(true, true));
        render_config(&peer_spec(false, true));
        render_probe_config(ZenohNetProtocol::Tcp, "0.0.0.0", 7448, None);
    }

    // ---- TLS rendering (the Phase-A breaking change) ----

    /// Regression guard: with `tls: None` the TLS feature is inert — no
    /// `transport` block is emitted, so a plaintext config is unchanged.
    /// (We assert structurally rather than byte-for-byte because serde_json's
    /// object key order depends on whether the `preserve_order` feature is
    /// unified in by some dependency, which would make an exact-string golden
    /// flaky across builds.)
    #[test]
    fn non_tls_spec_emits_no_transport_block() {
        let value = build_zenoh_config(&peer_spec(false, true));
        assert!(
            value.get("transport").is_none(),
            "a non-TLS spec must not render a transport block: {value}"
        );
        render_config(&peer_spec(false, true)); // and still parses
    }

    #[test]
    fn tls_router_config_emits_listen_cert_and_key() {
        let spec = router_spec(
            ZenohNetProtocol::Tls,
            "0.0.0.0",
            7447,
            false,
            Vec::new(),
            Some(TlsConfig::server(
                PathBuf::from("/certs/leaf.pem"),
                PathBuf::from("/certs/leaf.key"),
            )),
            &test_router_id(),
        );
        let cfg = build_zenoh_config(&spec);

        // The endpoint scheme is `tls/`, driven by the protocol's Display.
        assert_eq!(cfg["listen"]["endpoints"]["router"][0], "tls/0.0.0.0:7447");
        // A standalone (non-federated) router never dials out.
        assert!(cfg.get("connect").is_none());
        let tls = &cfg["transport"]["link"]["tls"];
        assert_eq!(tls["listen_certificate"], "/certs/leaf.pem");
        assert_eq!(tls["listen_private_key"], "/certs/leaf.key");
        assert_eq!(tls["verify_name_on_connect"], true);
        // Server identity only. Asserting the exact key set (rather than a list of
        // absences) is what makes this a guard: any key we did not mean to emit,
        // `enable_mtls` and the connect-side pair included, fails here.
        assert_eq!(
            tls_keys(tls),
            [
                "listen_certificate",
                "listen_private_key",
                "verify_name_on_connect"
            ]
        );
    }

    #[test]
    fn tls_client_config_emits_root_ca_and_verify_name() {
        let cfg = build_zenoh_config(&ZenohConfigSpec {
            mode: SessionMode::Client,
            connect_endpoints: vec!["tls/router.example:7443".to_string()],
            listen_endpoints: Vec::new(),
            reconnect: false,
            gossip: false,
            tls: Some(TlsConfig::client(PathBuf::from("/certs/ca.pem"))),
            namespace: None,
            id: None,
        });
        let tls = &cfg["transport"]["link"]["tls"];
        assert_eq!(tls["root_ca_certificate"], "/certs/ca.pem");
        assert_eq!(tls["verify_name_on_connect"], true);
        // Client trust only: no listener identity, and nothing the client would
        // present as its own certificate.
        assert_eq!(
            tls_keys(tls),
            ["root_ca_certificate", "verify_name_on_connect"]
        );
    }

    /// The keys of a rendered `transport.link.tls` block, sorted, so a test can
    /// pin the exact surface instead of enumerating absences.
    fn tls_keys(tls: &serde_json::Value) -> Vec<&str> {
        let mut keys: Vec<&str> = tls
            .as_object()
            .expect("the tls block is a JSON object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        keys
    }

    #[test]
    fn rendered_tls_router_config_parses_as_zenoh_config() {
        // The authoritative schema check: zenoh must accept the rendered
        // `transport.link.tls` block (a wrong key would be silently dropped, so
        // the real validation is the handshake integration test in tests/zenoh.rs).
        let s = render_router_config(
            ZenohNetProtocol::Tls,
            "0.0.0.0",
            7447,
            false,
            Vec::new(),
            Some(TlsConfig::server(
                PathBuf::from("/certs/leaf.pem"),
                PathBuf::from("/certs/leaf.key"),
            )),
            &test_router_id(),
        );
        zenoh::config::Config::from_json5(&s).expect("rendered tls router config parses");
        assert!(s.contains("tls/0.0.0.0:7447"));
    }

    #[test]
    fn federated_router_config_emits_connect_block_and_connect_tls() {
        // The daemon's local router: a plaintext `tcp/` listener for its own
        // nodes, PLUS a `tls/` connect endpoint federating it to a remote router,
        // trusting that router via a client CA. This is the peppy-side shape of
        // the per-user-router design.
        let s = render_router_config(
            ZenohNetProtocol::Tcp,
            "0.0.0.0",
            7448,
            true,
            vec!["tls/cap.zenoh.localhost:7443".to_string()],
            Some(TlsConfig::client(PathBuf::from("/certs/ca.pem"))),
            &test_router_id(),
        );
        let cfg: serde_json::Value =
            serde_json::from_str(&s).expect("rendered federated router config is JSON");

        assert_eq!(cfg["mode"], "router");
        // Local nodes still reach the router over plaintext loopback/LAN.
        assert_eq!(cfg["listen"]["endpoints"]["router"][0], "tcp/0.0.0.0:7448");
        // It federates out to the remote router over TLS, and keeps retrying so a
        // remote restart/reprovision is recovered (reconnect ⇒ `timeout_ms: -1`).
        assert_eq!(
            cfg["connect"]["endpoints"][0],
            "tls/cap.zenoh.localhost:7443"
        );
        assert_eq!(cfg["connect"]["timeout_ms"], -1);
        assert_eq!(cfg["connect"]["exit_on_failure"], false);
        // Connect-side trust only (verify the remote router's cert); no listener
        // identity, since the local listener is plaintext.
        let tls = &cfg["transport"]["link"]["tls"];
        assert_eq!(tls["root_ca_certificate"], "/certs/ca.pem");
        assert_eq!(tls["verify_name_on_connect"], true);
        assert!(tls.get("listen_certificate").is_none());

        // And the whole thing is a config zenoh accepts.
        zenoh::config::Config::from_json5(&s).expect("federated router config parses");
    }

    // ---- Workspace session namespace rendering ----

    /// The `namespace` key is rendered for application sessions (`Peer`/`Client`)
    /// and only for them: it is a session-level field, so a router (which only
    /// forwards between faces) must never carry one. A namespaced spec must also
    /// still parse as a real zenoh config.
    #[test]
    fn namespace_rendered_for_sessions_never_for_router() {
        let ns = Namespace::parse("550e8400-e29b-41d4-a716-446655440000").unwrap();

        // Peer session: namespace present with the workspace value.
        let peer = build_zenoh_config(&ZenohConfigSpec {
            namespace: Some(ns.clone()),
            ..peer_spec(true, true)
        });
        assert_eq!(peer["namespace"], ns.as_str());
        render_config(&ZenohConfigSpec {
            namespace: Some(ns.clone()),
            ..peer_spec(true, true)
        });

        // Client session: namespace present.
        let client = build_zenoh_config(&ZenohConfigSpec {
            mode: SessionMode::Client,
            connect_endpoints: vec!["tcp/127.0.0.1:7448".to_string()],
            listen_endpoints: Vec::new(),
            reconnect: false,
            gossip: false,
            tls: None,
            namespace: Some(ns.clone()),
            id: None,
        });
        assert_eq!(client["namespace"], ns.as_str());

        // Router: even when a namespace is supplied it is NOT rendered, because a
        // router never opens an application session.
        let router = build_zenoh_config(&ZenohConfigSpec {
            mode: SessionMode::Router,
            connect_endpoints: Vec::new(),
            listen_endpoints: vec!["tcp/0.0.0.0:7448".to_string()],
            reconnect: false,
            gossip: true,
            tls: None,
            namespace: Some(ns.clone()),
            id: Some(test_router_id()),
        });
        assert!(
            router.get("namespace").is_none(),
            "a router must never render a namespace: {router}"
        );

        // A session with no namespace omits the key entirely (back-compat).
        let bare = build_zenoh_config(&peer_spec(false, true));
        assert!(bare.get("namespace").is_none());
    }
}
