//! Zenoh-backed implementation of [`crate::MessengerBackend`].
//!
//! ## Why callback handlers, not FIFO
//!
//! Every receive-side zenoh API call in this module (`declare_subscriber`,
//! `declare_queryable`, `session.get`) uses `.callback(...)` rather than the
//! default FIFO reception handler. Zenoh's FIFO handler holds an internal
//! `flume::bounded` channel and logs
//! `zenoh::api::handlers::fifo: error=sending on a closed channel` at ERROR
//! whenever zenoh tries to deliver a sample/query/reply after the
//! receiver-side has been dropped — a routine event in this codebase (e.g. a
//! `QueryTarget::All` `call_service` keeps the query open until its
//! `NO_TIMEOUT_SENTINEL`, and sibling producers' late replies hit a
//! `ReplyStream` the consumer dropped after the first valid response).
//!
//! Callback handlers have no intermediate channel: each callback invocation
//! either forwards into our own `flume::bounded` channel (subscriber /
//! queryable, where blocking `send` preserves backpressure) or our own tokio
//! mpsc (`call_service`, where `try_send` silently drops on a closed/full
//! receiver because the caller only needs the first valid reply).
//!
//! The `tests/fifo_noise.rs` integration test pins this invariant: it
//! asserts zero `zenoh::api::handlers::fifo` ERROR events during a wildcard
//! service call with a late-replying sibling producer.

use crate::error::{Error, Result};
#[cfg(feature = "router")]
use crate::router_id::RouterId;
use crate::types::{
    ActionLivelinessProbe, CoreNodePresence, CoreNodePresenceList, IncomingRequest,
    LivelinessEvent, LivelinessToken, LivelinessWatch, Payload, PresenceScope, PublisherQoS,
    ReplyStream, ResponseToken, ServiceQueryable, ServiceReply, SubscriberBufferSizes,
    SubscriberQoS, TopicMessage, ZenohResponseToken, service_query_lifetime,
};
use crate::wire::zenoh_format::{ServiceReplyAttachment, TopicAttachment, ZenohWireFormat};
use crate::wire::{
    ActionWireReceiver, ActionWireSender, Segment, ServiceQueryKind, ServiceWireReceiver,
    ServiceWireSender, TopicWireReceiver, TopicWireSender,
};
use crate::zenoh_config::{
    SessionMode, TlsConfig, ZenohConfigSpec, connectable_host, peer_listen_endpoint, render_config,
};
use config::namespace::Namespace;
// `render_probe_config` and the `zenohd` module (facade/health/config-path) are
// only used by the router-management paths; a `zenoh`-without-`router` build (the
// backend, which only renders configs and opens client sessions) does not see them.
#[cfg(feature = "router")]
use crate::zenoh_config::render_probe_config;
#[cfg(feature = "router")]
use crate::zenohd;
use crate::zenohd::{ZenohEndpoint, ZenohNetProtocol};
#[cfg(feature = "router")]
use crate::{Messenger, MessengerAdapter};
use crate::{MessengerBackend, Subscription};

use std::net::SocketAddr;
#[cfg(feature = "router")]
use std::net::TcpListener;
use std::sync::Arc;
use tracing::info;

#[cfg(feature = "router")]
const ADOPTION_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Zenoh-specific QoS settings derived from a `PublisherQoS` level.
struct ZenohQoS {
    priority: Priority,
    congestion_control: CongestionControl,
    express: bool,
}

impl From<PublisherQoS> for ZenohQoS {
    fn from(qos: PublisherQoS) -> Self {
        match qos {
            PublisherQoS::BestEffort => Self {
                priority: Priority::DataLow,
                congestion_control: CongestionControl::Drop,
                express: true,
            },
            PublisherQoS::Standard => Self {
                priority: Priority::Data,
                congestion_control: CongestionControl::Drop,
                express: false,
            },
            PublisherQoS::Important => Self {
                priority: Priority::DataHigh,
                congestion_control: CongestionControl::Block,
                express: false,
            },
            PublisherQoS::Critical => Self {
                priority: Priority::RealTime,
                congestion_control: CongestionControl::Block,
                express: true,
            },
        }
    }
}

/// Reserves an ephemeral port by binding to port 0 and returning the assigned port.
/// The returned `TcpListener` holds the port until dropped.
#[cfg(feature = "router")]
fn reserve_ephemeral_port() -> std::io::Result<(u16, TcpListener)> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    Ok((port, listener))
}

/// Result of starting a zenohd router process.
///
/// The router is automatically stopped when this instance is dropped.
#[cfg(feature = "router")]
pub struct ZenohdInstance {
    messenger: Option<Messenger>,
    pub host: String,
    pub port: u16,
    /// Captured stdout and stderr of this router process.
    pub log_path: std::path::PathBuf,
}

#[cfg(feature = "router")]
impl ZenohdInstance {
    /// Returns a mutable reference to the messenger.
    pub fn messenger(&mut self) -> &mut Messenger {
        self.messenger
            .as_mut()
            .expect("messenger was already taken")
    }

    /// Takes ownership of the messenger, preventing automatic cleanup on drop.
    pub fn take_messenger(&mut self) -> Messenger {
        self.messenger.take().expect("messenger was already taken")
    }
}

#[cfg(feature = "router")]
impl Drop for ZenohdInstance {
    fn drop(&mut self) {
        let Some(mut messenger) = self.messenger.take() else {
            return;
        };
        let _ = std::thread::spawn(move || {
            if let Ok(rt) = tokio::runtime::Runtime::new() {
                let _ = rt.block_on(async move { messenger.stop_router().await });
            }
        })
        .join();
    }
}

use zenoh::qos::{CongestionControl, Priority};
use zenoh::sample::{Sample, SampleFields, SampleKind};

/// Resolved config for a node/daemon peer session, plus the inputs needed to
/// rebuild it (the reconnecting session is re-derived on every
/// [`start_session`](MessengerBackend::start_session)).
pub struct ZenohClientConfig {
    zenoh_config: zenoh::config::Config,
    host: String,
    port: u16,
    protocol: ZenohNetProtocol,
    /// Resolved gossip seed endpoints (defaults to the router endpoint).
    seed_peers: Vec<String>,
    /// Whether gossip-based direct peer linking is enabled for this session.
    gossip: bool,
    /// Per-QoS subscriber channel buffer sizes for this session.
    buffer_sizes: SubscriberBufferSizes,
    /// Client TLS material for a `tls/` endpoint (`None` for plaintext). Retained
    /// (not just baked into `zenoh_config`) so the reconnecting-session rebuild
    /// in `start_session` re-renders with the same TLS settings.
    tls: Option<TlsConfig>,
    /// Workspace namespace for this session.
    /// Retained (like `tls`) so the reconnecting-session rebuild in
    /// `start_session` re-applies it; lost otherwise on every router-restart
    /// reconnect. Applied via [`ZenohAdapter::with_namespace`]; `None` leaves the
    /// session namespace-free (probes, tests). zenoh captures the namespace once
    /// at session build, so there is no in-process swap -- a change needs a fresh
    /// session.
    namespace: Option<Namespace>,
}

pub struct ZenohAdapter {
    #[cfg(feature = "router")]
    zenohd: Option<zenohd::ZenohdFacade>,
    client_config: ZenohClientConfig,
    session: Option<Arc<zenoh::Session>>,
    /// When true, [`start_session`](MessengerBackend::start_session) opens a
    /// reconnecting session (see [`ZenohReconnectingClientConfigTemplate`]).
    reconnect_session: bool,
}

impl ZenohAdapter {
    /// Creates a ZenohAdapter that owns and manages a zenohd router. `gossip`
    /// selects the daemon session's own routing model (peer vs router-relay) and
    /// `buffer_sizes` its subscriber channel capacities. The adapter renders the
    /// router config, spawns zenohd, and reports an error if its port is busy.
    ///
    /// `connect_endpoints` *federates* the spawned router to upstream routers it
    /// dials (`<proto>/<host>:<port>` each) — e.g. the daemon's plaintext loopback
    /// router dialing a remote `tls/` router so the two zenohd routers join one
    /// network. Empty is a standalone router (today's behavior). The connect-side
    /// trust for a `tls/` upstream rides in `tls` (a [`TlsConfig::client`]); it is
    /// written into the zenohd config only and is inert for the daemon's own
    /// plaintext loopback session.
    ///
    /// `router_id` is the transport identity pinned into the rendered config and
    /// reused by every later [`refederate`](Self::refederate), so the router
    /// keeps one identity across restarts and federation changes. It is taken by
    /// value, not as an `Option`: see [`RouterId`].
    #[cfg(feature = "router")]
    #[allow(clippy::too_many_arguments)]
    pub fn with_router(
        protocol: ZenohNetProtocol,
        host: &str,
        port: u16,
        gossip: bool,
        buffer_sizes: SubscriberBufferSizes,
        connect_endpoints: Vec<String>,
        tls: Option<TlsConfig>,
        router_id: RouterId,
    ) -> Result<Self> {
        let zenohd_config_path = zenohd::router_config_path(
            protocol,
            host,
            port,
            connect_endpoints,
            tls.clone(),
            &router_id,
        )?;
        let facade = zenohd::ZenohdFacade::managed(zenohd_config_path, router_id)?;
        let client_config =
            Self::derive_client_config_from_zenohd(&facade, gossip, buffer_sizes, tls)?;

        Ok(Self {
            zenohd: Some(facade),
            client_config,
            session: None,
            reconnect_session: false,
        })
    }

    /// Creates a ZenohAdapter that adopts an already-running, operator-managed
    /// Zenoh router at `endpoint`.
    ///
    /// The endpoint is a dial locator in `tcp/<host>:<port>` form. Listen
    /// wildcards such as `tcp/0.0.0.0:7447` are rejected because clients cannot
    /// dial them. Peppy renders no router config, discovers no binary, and never
    /// starts, stops, restarts, or refederates this router. [`start_router`](MessengerBackend::start_router)
    /// verifies that the socket belongs to a responsive Zenoh router before
    /// marking it adopted.
    #[cfg(feature = "router")]
    pub fn with_external_router(
        endpoint: &str,
        gossip: bool,
        buffer_sizes: SubscriberBufferSizes,
    ) -> Result<Self> {
        let endpoint: ZenohEndpoint = endpoint.parse()?;
        endpoint.validate_external_tcp()?;
        let facade = zenohd::ZenohdFacade::external(endpoint);
        let client_config =
            Self::derive_client_config_from_zenohd(&facade, gossip, buffer_sizes, None)?;

        Ok(Self {
            zenohd: Some(facade),
            client_config,
            session: None,
            reconnect_session: false,
        })
    }

    /// Re-renders the owned router's zenohd config file *in place* with new
    /// federation `connect_endpoints` (+ connect-side `tls`) — same protocol /
    /// host / port as it was spawned with, only the upstream connect block (and
    /// its TLS trust) change. The new config takes effect on the next
    /// `stop_router` / `start_router`; this call does not itself restart zenohd.
    ///
    /// Used by the daemon to (de)federate its local router to the user's per-user
    /// cloud router when they log in / out, without a full daemon restart.
    ///
    /// Returns whether the config was actually rewritten: `Ok(true)` when a new
    /// config was rendered (the caller must restart zenohd to apply it),
    /// `Ok(false)` when a `ZENOH_CONFIG`-overridden config is in effect or the
    /// router is external. Both are operator-owned and left untouched. An
    /// external router never read peppy's rendered config, so there is nothing
    /// to rewrite or restart. Errors if the adapter owns no router.
    #[cfg(feature = "router")]
    pub fn refederate(
        &mut self,
        connect_endpoints: Vec<String>,
        tls: Option<TlsConfig>,
    ) -> Result<bool> {
        let facade = self.zenohd.as_ref().ok_or_else(|| {
            Error::BackendError("refederate called on an adapter that owns no router".to_string())
        })?;
        // Operator-owned routers are never rendered over. A pinned
        // `ZENOH_CONFIG` file stays untouched, and an external router never read
        // the rendered file at all. Report the no-op so the caller skips a
        // restart that peppy must not perform.
        if facade.is_pinned() || facade.is_external() {
            return Ok(false);
        }
        let ep = &facade.zenoh_endpoint;
        // Rewrite the exact config file captured when the router was built, *not*
        // via `router_config_path` — that re-reads `ZENOH_CONFIG`, which (if it
        // changed after startup) could redirect this write elsewhere or skip it
        // via the override early-return, leaving the running router's file stale.
        zenohd::render_router_config_to_path(
            facade
                .managed_config_path()
                .expect("a non-external facade has a managed config path"),
            ep.protocol(),
            ep.host(),
            ep.port(),
            connect_endpoints,
            tls,
            // The identity the router was built with, not a fresh one: a
            // (de)federation is the same router changing upstream, and minting a
            // new zid here would make it look like a different machine to
            // anything reading the upstream's session list.
            facade
                .managed_router_id()
                .expect("a non-external facade has a managed router id"),
        )?;
        Ok(true)
    }

    /// Creates a ZenohAdapter that joins the mesh seeded by an existing zenohd
    /// router, using the default discovery (gossip on, seed = `host:port`) and
    /// default subscriber buffers. The session runs in `peer` mode so data forms
    /// direct links instead of relaying through the router.
    pub fn connect_to(protocol: ZenohNetProtocol, host: &str, port: u16) -> Result<Self> {
        Self::connect_to_with_discovery(
            protocol,
            host,
            port,
            Vec::new(),
            true,
            SubscriberBufferSizes::default(),
            None,
        )
    }

    /// Like [`connect_to`](Self::connect_to) but over TLS: opens a `tls/`
    /// **client** session to `host:port`, verifying the router against `tls`'s
    /// `root_ca_certificate` (with `verify_name_on_connect`). Unlike `connect_to`,
    /// this is **client** mode (`gossip = false`): all traffic routes through the
    /// router and the session binds no loopback peer listener — which is what we
    /// want for a remote router (a peer listener would also need its own server
    /// cert, which a pure client has no reason to hold). Use
    /// [`connect_to_with_discovery`](Self::connect_to_with_discovery) for control.
    ///
    /// This is the low-level TLS-client primitive: it is what the `tls/` transport
    /// tests dial a router with, and is available for a direct client→router
    /// session. The peppy daemon does **not** use it to reach the shared platform
    /// router: that is router-to-router federation (the local zenohd dials the
    /// remote over a `tls/` `connect` endpoint; see [`Self::with_router`]).
    pub fn connect_to_tls(host: &str, port: u16, tls: TlsConfig) -> Result<Self> {
        Self::connect_to_with_discovery(
            ZenohNetProtocol::Tls,
            host,
            port,
            Vec::new(),
            false,
            SubscriberBufferSizes::default(),
            Some(tls),
        )
    }

    /// Like [`connect_to`](Self::connect_to) but with an explicit gossip seed
    /// list, gossip toggle, and subscriber buffer sizes. The node runtime passes
    /// its `DiscoveryConfig` here. An empty `seed_peers` falls back to the single
    /// `host:port` seed.
    #[allow(clippy::too_many_arguments)]
    pub fn connect_to_with_discovery(
        protocol: ZenohNetProtocol,
        host: &str,
        port: u16,
        seed_peers: Vec<String>,
        gossip: bool,
        buffer_sizes: SubscriberBufferSizes,
        tls: Option<TlsConfig>,
    ) -> Result<Self> {
        let client_config = Self::create_client_config(
            protocol,
            host,
            port,
            false,
            seed_peers,
            gossip,
            buffer_sizes,
            tls,
            // Namespace-free by default; callers apply workspace routing with
            // [`Self::with_namespace`] (e.g. peppylib's `MessengerHandle::connect`
            // builder, which defaults the namespace to `local`).
            None,
        );

        Ok(Self {
            #[cfg(feature = "router")]
            zenohd: None,
            client_config,
            session: None,
            reconnect_session: false,
        })
    }

    /// Marks this adapter's long-lived session as reconnecting: on
    /// [`start_session`](MessengerBackend::start_session) it uses a config that
    /// retries the connection (and re-declares its subscriptions/queryables) if
    /// the router is restarted under it. Used by the daemon so the router
    /// watchdog can respawn zenohd without leaving the daemon's own session
    /// dead. CLI and short-lived adapters leave this off (fail-fast default).
    ///
    /// # `start_session` blocks for a client-mode session
    ///
    /// The retry is **foreground** in client mode and **background** in peer
    /// mode, so what this changes about `start_session` depends on which one
    /// the adapter builds:
    ///
    /// * A **peer** session ([`connect_to`](Self::connect_to), and any
    ///   `gossip = true` constructor) returns from `start_session` immediately
    ///   even when the seed is unreachable, and keeps dialing behind the
    ///   session.
    /// * A **client** session ([`connect_to_tls`](Self::connect_to_tls), and
    ///   any `gossip = false` constructor) does **not** return from
    ///   `start_session` until the router accepts it. There is no timeout to
    ///   fall back on: this config sets `connect.timeout_ms: -1`, which zenoh
    ///   reads as `Duration::MAX`.
    ///
    /// The asymmetry is zenoh's, not ours, and it is not what the config keys
    /// suggest: `connect.exit_on_failure: false` is what buys the peer session
    /// its background dial (`connect_peers_multiply_links`), and the
    /// client-mode path (`connect_peers_single_link`) never reads that key at
    /// all.
    ///
    /// So a caller whose router is remote, and may be down when the process
    /// starts, must not `await` `start_session` on its startup path: doing so
    /// hangs the process for exactly as long as the router is unreachable.
    /// Drive the open from a background task and treat the session as absent
    /// until it lands. [`is_linked`](Self::is_linked) is the signal for that.
    /// `reconnecting_client_open_blocks_until_the_router_accepts` pins both
    /// halves of this behaviour.
    pub fn with_session_reconnect(mut self) -> Self {
        self.reconnect_session = true;
        self
    }

    /// Whether this adapter's session currently holds a transport link to a
    /// router.
    ///
    /// A reconnecting session is deliberately tolerant: `zenoh::open` succeeds
    /// against an unreachable endpoint and retries in the background, so a
    /// query issued while the link is down returns *no replies* rather than an
    /// error. That makes "the transport is down" and "nothing is live"
    /// indistinguishable from the query result alone, and any caller that
    /// reports liveliness to a user has to tell those two apart. This is the
    /// signal that does it: `false` means an empty result carries no
    /// information, `true` means it means nothing is live.
    ///
    /// Answered from the session's current routing state, so it costs no
    /// round trip. `false` before [`start_session`](MessengerBackend::start_session).
    pub async fn is_linked(&self) -> bool {
        let Some(session) = self.session.as_ref() else {
            return false;
        };
        session.info().routers_zid().await.next().is_some()
    }

    /// Starts a zenohd router with an ephemeral port, retrying on bind failures.
    /// The hosted session is a peer with default subscriber buffers; use
    /// [`start_router_ephemeral_in_mode`](Self::start_router_ephemeral_in_mode)
    /// to pick the session's gossip mode and buffer sizes.
    #[cfg(feature = "router")]
    pub async fn start_router_ephemeral(host: &str, port: Option<u16>) -> Result<ZenohdInstance> {
        Self::start_router_ephemeral_in_mode(
            host,
            port,
            true,
            SubscriberBufferSizes::default(),
            None,
        )
        .await
    }

    /// Like [`start_router_ephemeral`](Self::start_router_ephemeral) but the
    /// hosted session's `gossip` (peer vs router-relay) and subscriber buffer
    /// sizes are explicit. Used by tests to exercise both messaging modes.
    ///
    /// `namespace` stamps a workspace namespace onto the hosted session
    /// (the same `with_router(...).with_namespace(...)` pairing the daemon uses),
    /// so a test that runs a core node off this session and spawns nodes under
    /// that workspace stays routing-consistent with them. `None` leaves the hosted
    /// session namespace-free (the default for client-vs-client tests).
    ///
    /// When `port` is `None`, automatically selects an available port and retries
    /// up to 32 times if the port becomes unavailable. When `port` is `Some`,
    /// attempts exactly once with that port.
    ///
    /// Returns a [`ZenohdInstance`] that automatically stops the router when dropped.
    #[cfg(feature = "router")]
    pub async fn start_router_ephemeral_in_mode(
        host: &str,
        port: Option<u16>,
        gossip: bool,
        buffer_sizes: SubscriberBufferSizes,
        namespace: Option<Namespace>,
    ) -> Result<ZenohdInstance> {
        let max_attempts = if port.is_some() { 1 } else { 32 };

        for attempt in 0..max_attempts {
            let (port, _reservation) = match port {
                Some(p) => (p, None),
                None => {
                    let (p, listener) =
                        reserve_ephemeral_port().map_err(|e| Error::BackendError(e.to_string()))?;
                    (p, Some(listener))
                }
            };

            let mut adapter = Self::with_router(
                ZenohNetProtocol::Tcp,
                host,
                port,
                gossip,
                buffer_sizes,
                Vec::new(),
                None,
                // An ephemeral router lives and dies with its caller, so it has
                // no identity to keep across restarts. A fresh one per attempt
                // also keeps two concurrent ephemeral routers distinct, which a
                // shared constant would not.
                RouterId::generate(),
            )?
            .with_namespace(namespace.clone());
            // An ephemeral router's caller is typically a test process, which
            // can die by SIGKILL without running any Drop. Have the kernel reap
            // the zenohd child in that case (Linux only) so killed test runs
            // cannot strand routers holding ports.
            if let Some(facade) = adapter.zenohd.as_mut() {
                facade.set_kill_on_parent_death();
            }
            // A lightweight client probe (no listener, no peer discovery) is the
            // cheapest reliable "router accepts sessions yet?" check.
            let probe_config = render_probe_config(ZenohNetProtocol::Tcp, host, port, None);
            let log_path = adapter
                .zenohd
                .as_ref()
                .and_then(zenohd::ZenohdFacade::managed_log_path)
                .expect("ephemeral routers own their log file")
                .to_path_buf();
            let mut messenger = Messenger::new(MessengerAdapter::Zenoh(adapter));

            // Drop the port reservation before starting the router so zenohd can bind to it
            drop(_reservation);

            match messenger.start_router().await {
                Ok(()) => {
                    // Readiness signal: zenohd's TCP listener can accept before the
                    // protocol handshake is settled, so a real zenoh::open is the only
                    // reliable signal that subsequent sessions will succeed. The probe
                    // session is dropped immediately; the caller opens their own.
                    match zenoh::open(probe_config).await {
                        Ok(probe) => {
                            drop(probe);
                            return Ok(ZenohdInstance {
                                messenger: Some(messenger),
                                host: host.to_string(),
                                port,
                                log_path,
                            });
                        }
                        Err(_) if attempt + 1 < max_attempts => {
                            // Drop messenger to stop the router, then retry on a fresh port.
                            drop(messenger);
                            continue;
                        }
                        Err(e) => {
                            return Err(Error::BackendError(format!(
                                "Zenoh readiness probe failed: {}",
                                e
                            )));
                        }
                    }
                }
                Err(Error::BackendError(_)) if attempt + 1 < max_attempts => {
                    continue;
                }
                Err(err) => return Err(err),
            }
        }

        Err(Error::BackendError(format!(
            "Failed to start zenoh router after {max_attempts} attempts"
        )))
    }

    pub fn client_endpoint(&self) -> (&str, u16) {
        (self.client_config.host.as_str(), self.client_config.port)
    }

    /// Returns the complete locator used by this adapter's client session.
    /// Unlike [`client_endpoint`](Self::client_endpoint), this retains the
    /// transport protocol and can be persisted directly in daemon state.
    pub fn client_locator(&self) -> ZenohEndpoint {
        ZenohEndpoint::new(
            self.client_config.protocol,
            self.client_config.host.clone(),
            self.client_config.port,
        )
    }

    /// Builds a lock-free [`RouterHealthChecker`] bound to this adapter's router
    /// endpoint, for the router watchdog to probe liveness without holding the
    /// central messenger lock.
    #[cfg(feature = "router")]
    pub fn router_health_checker(&self) -> zenohd::RouterHealthChecker {
        let probe_config = render_probe_config(
            self.client_config.protocol,
            &self.client_config.host,
            self.client_config.port,
            // Probe a `tls/` router over TLS using the same trust the adapter
            // holds; `None` for a plaintext router renders an unchanged config.
            self.client_config.tls.clone(),
        );
        zenohd::RouterHealthChecker::new(probe_config)
    }

    /// Builds a lock-free [`zenohd::RouterLinksProbe`] over this adapter's
    /// *managed* router: the bounded wait for the router's configured `connect`
    /// links (operator-pinned federation, an applied cloud upstream) to
    /// establish, run by the daemon before its boot-time presence check so the
    /// check sees the wired mesh instead of racing zenohd's dials. `None` when
    /// there is nothing to wait for: no router, an external/adopted router
    /// (its config — and so its links — belong to the operator), or a managed
    /// config with no connect endpoints (the standalone default).
    #[cfg(feature = "router")]
    pub fn router_links_probe(&self) -> Option<zenohd::RouterLinksProbe> {
        let zenohd = self.zenohd.as_ref()?;
        if zenohd.is_external() {
            return None;
        }
        let probe_config = render_probe_config(
            self.client_config.protocol,
            &self.client_config.host,
            self.client_config.port,
            self.client_config.tls.clone(),
        );
        zenohd::RouterLinksProbe::new(probe_config, zenohd.configured_connect_endpoints())
    }

    #[cfg(feature = "router")]
    pub fn router_is_adopted(&self) -> bool {
        self.zenohd.as_ref().is_some_and(|z| z.is_adopted())
    }

    /// Builds a peer-session config seeded by `host:port` (or `seed_peers` when
    /// non-empty). `buffer_sizes` are applied later at the flume/mpsc layer
    /// (subscribe / listen / call).
    #[allow(clippy::too_many_arguments)]
    fn create_client_config(
        protocol: ZenohNetProtocol,
        host: &str,
        port: u16,
        reconnect: bool,
        seed_peers: Vec<String>,
        gossip: bool,
        buffer_sizes: SubscriberBufferSizes,
        tls: Option<TlsConfig>,
        namespace: Option<Namespace>,
    ) -> ZenohClientConfig {
        let connect_host = connectable_host(host);
        let seeds = if seed_peers.is_empty() {
            vec![format!("{protocol}/{connect_host}:{port}")]
        } else {
            seed_peers
        };

        let zenoh_config = if gossip {
            // A `peer` binds a wildcard listener and forms direct peer-to-peer
            // links via gossip; the endpoint must be non-loopback for zenoh
            // 1.10 to advertise it (see [`peer_listen_endpoint`]).
            render_config(&ZenohConfigSpec {
                mode: SessionMode::Peer,
                connect_endpoints: seeds.clone(),
                listen_endpoints: vec![peer_listen_endpoint(protocol)],
                reconnect,
                gossip: true,
                tls: tls.clone(),
                namespace: namespace.clone(),
                // Application sessions are process-scoped and identified by their
                // declarations, not by a zid, so nothing pins one here.
                id: None,
            })
        } else {
            // A plain `client` routes only through the selected router (no
            // listener or peer discovery).
            render_config(&ZenohConfigSpec {
                mode: SessionMode::Client,
                connect_endpoints: seeds.clone(),
                listen_endpoints: Vec::new(),
                reconnect,
                gossip: false,
                tls: tls.clone(),
                namespace: namespace.clone(),
                // Application sessions are process-scoped and identified by their
                // declarations, not by a zid, so nothing pins one here.
                id: None,
            })
        };

        ZenohClientConfig {
            zenoh_config,
            host: connect_host,
            port,
            protocol,
            seed_peers: seeds,
            gossip,
            buffer_sizes,
            tls,
            namespace,
        }
    }

    #[cfg(feature = "router")]
    fn derive_client_config_from_zenohd(
        zenohd: &zenohd::ZenohdFacade,
        gossip: bool,
        buffer_sizes: SubscriberBufferSizes,
        tls: Option<TlsConfig>,
    ) -> Result<ZenohClientConfig> {
        // Build the adapter session from the router endpoint carried by the
        // facade. `with_router` supplies a Peppy-managed router, while
        // `with_external_router` may supply a remote, operator-owned router.
        // `gossip` selects whether the session is a peer (direct links) or a
        // client (router relay). The long-lived session is rebuilt as
        // reconnecting in `start_session` when `reconnect_session` is set; the
        // readiness probe in `start_router_ephemeral` builds its own client
        // probe config.
        Ok(Self::create_client_config(
            zenohd.zenoh_endpoint.protocol(),
            zenohd.zenoh_endpoint.host(),
            zenohd.zenoh_endpoint.port(),
            false,
            Vec::new(),
            gossip,
            buffer_sizes,
            tls,
            // Both router constructors derive a namespace-free session; callers
            // apply workspace routing afterward via [`Self::with_namespace`].
            None,
        ))
    }

    /// Applies a workspace namespace to this adapter's session, re-rendering
    /// the stored session config so a
    /// non-reconnecting session -- which opens `client_config.zenoh_config`
    /// directly -- carries it, and `start_session`'s reconnecting rebuild
    /// re-applies it the same way it does `tls`. `None` leaves the session
    /// namespace-free.
    ///
    /// There is intentionally no in-process namespace *swap* once a session is
    /// open: zenoh captures the namespace once at session build, so a change
    /// requires a fresh session (the daemon rebuilds its whole generation).
    pub fn with_namespace(mut self, namespace: Option<Namespace>) -> Self {
        let protocol = self.client_config.protocol;
        let host = self.client_config.host.clone();
        let port = self.client_config.port;
        let seed_peers = self.client_config.seed_peers.clone();
        let gossip = self.client_config.gossip;
        let buffer_sizes = self.client_config.buffer_sizes;
        let tls = self.client_config.tls.clone();
        self.client_config = Self::create_client_config(
            protocol,
            &host,
            port,
            false,
            seed_peers,
            gossip,
            buffer_sizes,
            tls,
            namespace,
        );
        self
    }
}

impl MessengerBackend for ZenohAdapter {
    async fn start_session(&mut self) -> Result<()> {
        // The daemon's long-lived session uses a reconnecting config so it
        // re-establishes itself (and re-declares its subscriptions/queryables)
        // if the router is restarted under it — e.g. by the router watchdog.
        // Short-lived / CLI sessions keep the fail-fast default.
        let config = if self.reconnect_session {
            Self::create_client_config(
                self.client_config.protocol,
                &self.client_config.host,
                self.client_config.port,
                true,
                self.client_config.seed_peers.clone(),
                self.client_config.gossip,
                self.client_config.buffer_sizes,
                self.client_config.tls.clone(),
                self.client_config.namespace.clone(),
            )
            .zenoh_config
        } else {
            self.client_config.zenoh_config.clone()
        };

        let session = zenoh::open(config)
            .await
            .map_err(|e| Error::BackendError(format!("Failed to create Zenoh session: {}", e)))?;

        let mode = if self.client_config.gossip {
            "peer"
        } else {
            "client"
        };
        info!(
            "Zenoh {} session connected to {}://{}:{}",
            mode, &self.client_config.protocol, &self.client_config.host, &self.client_config.port
        );
        self.session = Some(Arc::new(session));
        Ok(())
    }

    async fn stop_session(&mut self) -> Result<()> {
        if let Some(session) = self.session.take() {
            // Close while zenohd is still alive so the undeclare-face
            // messages reach the router. Drop's later close becomes a
            // no-op (primitives already taken), which is what keeps the
            // session's other Arc clones — e.g. ZenohPublisher — from
            // spamming "Undefined face context" when they finally drop.
            if let Err(err) = session.close().await {
                tracing::warn!("Zenoh session close returned an error: {err}");
            }
        }
        Ok(())
    }

    async fn subscribe_topic(
        &self,
        recv: &TopicWireReceiver,
        qos: SubscriberQoS,
    ) -> Result<Subscription> {
        let drop_secondary = recv.drops_secondary_publishes();
        self.subscribe_keyexpr(ZenohWireFormat::topic_subscribe(recv), qos, drop_secondary)
            .await
    }

    async fn publish_topic(
        &mut self,
        sender: &TopicWireSender,
        payload: Payload,
        qos: PublisherQoS,
        is_primary: bool,
    ) -> Result<()> {
        self.publish_keyexpr(
            &ZenohWireFormat::topic_publish(sender),
            payload,
            qos,
            is_primary,
        )
        .await
    }

    async fn listen_service(&self, recv: &ServiceWireReceiver) -> Result<ServiceQueryable> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| Error::MessagingSessionError("Session not initialized".to_string()))?;

        let (tx, rx) = flume::bounded::<IncomingRequest>(
            self.client_config
                .buffer_sizes
                .size_for(SubscriberQoS::Standard),
        );

        // One queryable per listen call. The declared keyexpr has `*` at the
        // link_id slot so a single queryable absorbs every bound link_id —
        // `process_inbound_query` does the dispatch by parsing the selector.
        // Two queryables for one process would let a caller's wildcard
        // `*` selector double-deliver via `QueryTarget::All`.
        let declare_keyexpr = ZenohWireFormat::service_queryable_declare(recv);
        let recv_clone = recv.clone();
        // Probe replies are spawned onto the listener's runtime: the zenoh
        // callback runs on a zenoh worker thread that must not block, and
        // `listen_service` always executes inside the consumer's tokio
        // runtime, so the handle is available to capture here.
        let rt = tokio::runtime::Handle::current();
        let queryable = session
            .declare_queryable(&declare_keyexpr)
            .complete(true)
            .callback(move |query| {
                process_inbound_query(query, &recv_clone, &tx, &rt);
            })
            .await
            .map_err(|e| Error::MessagingSessionError(e.to_string()))?;

        Ok(ServiceQueryable::new(rx, vec![Box::new(queryable)]))
    }

    async fn call_service(
        &self,
        sender: &ServiceWireSender,
        payload: Payload,
        kind: ServiceQueryKind,
        timeout: Option<std::time::Duration>,
    ) -> Result<ReplyStream> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| Error::MessagingSessionError("Session not initialized".to_string()))?;
        let selector = ZenohWireFormat::service_get_selector(sender);
        // Mandatory query attachment: carries the request kind (UserRequest
        // vs Probe) plus the consumer's sibling-exclusion set. The producer
        // refuses queries with no attachment, which is what makes the
        // mid-rollout failure mode loud (consumer sees ServiceUnreachable
        // instead of misclassifying the request as a default).
        let attachment = ZenohWireFormat::service_get_selector_attachment(sender, kind);

        // The zenoh-side `.timeout(...)` is the query's wire lifetime, not
        // the caller's deadline; see `service_query_lifetime`.
        let timeout = service_query_lifetime(timeout);

        let (tx, rx) = tokio::sync::mpsc::channel::<ServiceReply>(
            self.client_config
                .buffer_sizes
                .size_for(SubscriberQoS::Standard),
        );

        // `try_send` (not `send`) because the callback runs synchronously on
        // a zenoh worker thread that we must not block. Two drop conditions
        // are tolerated here:
        //   1. receiver dropped — caller has the first valid reply and has
        //      released the `ReplyStream`; sibling producers' late replies
        //      go nowhere, which is intentional;
        //   2. channel full (capacity = the Standard subscriber buffer size)
        //      — would only happen if the consumer's `poll_service` loop
        //      stalls for thousands of replies; in practice the consumer
        //      drains the channel as fast as zenoh fills it, so this branch
        //      is effectively unreachable. If it ever fires, the lost reply
        //      is acceptable: `QueryTarget::All` is best-effort fan-in, not
        //      a guaranteed-delivery API.
        // See the module-level "Why callback handlers, not FIFO" doc.
        session
            .get(&selector)
            .payload(payload.into_zbytes())
            .attachment(attachment.to_vec())
            .target(zenoh::query::QueryTarget::All)
            .consolidation(zenoh::query::ConsolidationMode::None)
            .accept_replies(zenoh::query::ReplyKeyExpr::Any)
            .timeout(timeout)
            .callback(move |reply| {
                let sample = match reply.result() {
                    Ok(sample) => sample,
                    // Producers reply with samples only (a handler error rides
                    // in a sample's attachment), so an error reply is zenoh's
                    // own `Timeout`: the query's wire lifetime ended. The
                    // caller stopped waiting `LATE_REPLY_GRACE` earlier and
                    // has reported the outcome, and zenoh's routing layer
                    // warns about the expired query itself, so this is a
                    // diagnostic, not an event.
                    Err(err) => {
                        tracing::debug!(
                            reason = %String::from_utf8_lossy(&err.payload().to_bytes()),
                            "service query ended with a transport error reply"
                        );
                        return;
                    }
                };
                let key_expr = sample.key_expr().as_str();
                if tx.is_closed() {
                    tracing::trace!(
                        %key_expr,
                        "discarding service reply after caller stopped waiting"
                    );
                    return;
                }
                let zbytes = sample.payload().clone();
                let attachment_bytes = sample
                    .attachment()
                    .map(|z| z.to_bytes())
                    .unwrap_or_default();
                let reply_kind = match ServiceReplyAttachment::decode(attachment_bytes.as_ref()) {
                    Ok(a) => a.kind,
                    Err(err) => {
                        tracing::error!(%key_expr, %err, "dropping service reply with malformed attachment");
                        return;
                    }
                };
                match TopicMessage::from_zbytes(key_expr, zbytes) {
                    Ok(message) => {
                        let _ = tx.try_send(ServiceReply::new(message, reply_kind));
                    }
                    Err(err) => {
                        tracing::error!(%key_expr, %err, "failed to parse service reply keyexpr");
                    }
                }
            })
            .await
            .map_err(|e| Error::BackendError(e.to_string()))?;

        Ok(ReplyStream::new(rx, None))
    }

    async fn subscribe_action_feedback(
        &self,
        sender: &ActionWireSender,
        goal_id: &str,
        qos: SubscriberQoS,
    ) -> Result<Subscription> {
        // Action feedback shares the wildcard-link_id keyexpr shape with
        // topic subscribe but doesn't multi-publish per goal — feedback is
        // emitted under the single link_id chosen at goal time (see the
        // `action_feedback_publish` comment in `wire::zenoh_format`). So
        // there are no secondaries to drop; pass `false`.
        self.subscribe_keyexpr(
            ZenohWireFormat::action_feedback_subscribe(sender, goal_id),
            qos,
            false,
        )
        .await
    }

    async fn declare_action_liveliness(
        &self,
        recv: &ActionWireReceiver,
    ) -> Result<LivelinessToken> {
        self.declare_liveliness_token(ZenohWireFormat::action_liveliness_token(recv))
            .await
    }

    async fn watch_action_producer(&self, sender: &ActionWireSender) -> Result<LivelinessWatch> {
        self.watch_liveliness(ZenohWireFormat::action_liveliness_watch(sender), |_| {
            Some(())
        })
        .await
    }

    async fn probe_action_producer(
        &self,
        sender: &ActionWireSender,
        timeout: std::time::Duration,
    ) -> Result<ActionLivelinessProbe> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| Error::MessagingSessionError("Session not initialized".to_string()))?;
        let keyexpr = ZenohWireFormat::action_liveliness_watch(sender);
        // The callback closure owns `tx`; zenoh drops it when the query
        // finalizes (at the latest after `timeout`), so the probe's
        // `resolve` observes `Disconnected` exactly when the query
        // completed with no matching token. Only issuance is awaited here.
        let (tx, rx) = flume::bounded::<()>(1);
        session
            .liveliness()
            .get(&keyexpr)
            .timeout(timeout)
            .callback(move |reply| {
                if reply.result().is_ok() {
                    let _ = tx.try_send(());
                }
            })
            .await
            .map_err(|e| Error::MessagingSessionError(e.to_string()))?;
        Ok(ActionLivelinessProbe::new(rx))
    }

    async fn declare_core_node_presence(
        &self,
        core_node: &Segment,
        instance_id: &Segment,
    ) -> Result<LivelinessToken> {
        self.declare_liveliness_token(ZenohWireFormat::core_node_presence_token(
            core_node,
            instance_id,
        ))
        .await
    }

    async fn watch_core_node_presence(
        &self,
        core_node: Option<&Segment>,
    ) -> Result<LivelinessWatch<CoreNodePresence>> {
        self.watch_liveliness(
            ZenohWireFormat::core_node_presence_filter(PresenceScope::Session, core_node),
            |sample| {
                let keyexpr = sample.key_expr().as_str();
                match ZenohWireFormat::parse_core_node_presence(PresenceScope::Session, keyexpr) {
                    Ok(presence) => Some(presence),
                    Err(_) => {
                        tracing::error!(%keyexpr, "failed to parse core-node presence keyexpr");
                        None
                    }
                }
            },
        )
        .await
    }

    async fn list_core_node_presence(
        &self,
        scope: PresenceScope<'_>,
        core_node: Option<&Segment>,
        timeout: std::time::Duration,
    ) -> Result<CoreNodePresenceList> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| Error::MessagingSessionError("Session not initialized".to_string()))?;
        let keyexpr = ZenohWireFormat::core_node_presence_filter(scope, core_node);
        // Owned so the `'static` reply callback can strip exactly the prefix
        // the selector above wrote.
        let namespace = scope.to_namespace();
        // Use a callback rather than Zenoh's default FIFO handler (see the
        // module-level rationale). The callback-owned sender is dropped when
        // the query finalizes (at the latest after `timeout`), ending the
        // returned enumeration's `collect` after every reply was delivered.
        // Only issuance is awaited here, so callers can release any shared
        // lock before collecting.
        let (tx, rx) = flume::unbounded::<Result<CoreNodePresence>>();
        session
            .liveliness()
            .get(&keyexpr)
            .timeout(timeout)
            .callback(move |reply| {
                let scope = PresenceScope::from_namespace(namespace.as_ref());
                let presence = reply
                    .into_result()
                    .map_err(|err| Error::BackendError(err.to_string()))
                    .and_then(|sample| {
                        ZenohWireFormat::parse_core_node_presence(scope, sample.key_expr().as_str())
                            .map_err(Into::into)
                    });
                let _ = tx.send(presence);
            })
            .await
            .map_err(|e| Error::MessagingSessionError(e.to_string()))?;
        Ok(CoreNodePresenceList::new(rx))
    }

    async fn start_router(&mut self) -> Result<()> {
        #[cfg(feature = "router")]
        {
            let health_checker = self.router_health_checker();
            let zenohd = self
                .zenohd
                .as_mut()
                .ok_or(Error::ZenohDConfigurationNotFound)?;
            if zenohd.is_external() {
                let ep = &zenohd.zenoh_endpoint;
                if !zenohd
                    .router_endpoint_reachable(ADOPTION_PROBE_TIMEOUT)
                    .await
                {
                    return Err(Error::BackendError(format!(
                        "external Zenoh router endpoint `{ep}` is not accepting TCP connections; \
                         start the router at that endpoint or configure peppy to manage zenohd"
                    )));
                }
                if !health_checker
                    .is_router_responsive(ADOPTION_PROBE_TIMEOUT)
                    .await
                {
                    return Err(Error::BackendError(format!(
                        "external Zenoh router endpoint `{ep}` accepts TCP connections, but is not \
                         a responsive Zenoh router; verify the configured endpoint"
                    )));
                }
                zenohd.adopt_external_router();
                return Ok(());
            }
            zenohd.start_router().await?;
            Ok(())
        }
        // Client-only build: router management was not compiled in.
        #[cfg(not(feature = "router"))]
        {
            Err(Error::ZenohDConfigurationNotFound)
        }
    }

    async fn stop_router(&mut self) -> Result<()> {
        #[cfg(feature = "router")]
        {
            let zenohd = self
                .zenohd
                .as_mut()
                .ok_or(Error::ZenohDConfigurationNotFound)?;
            zenohd.stop_router_async().await
        }
        // Client-only build: router management was not compiled in.
        #[cfg(not(feature = "router"))]
        {
            Err(Error::ZenohDConfigurationNotFound)
        }
    }

    fn get_host(&self) -> SocketAddr {
        let host = &self.client_config.host;
        let port = self.client_config.port;
        // Zenoh locators bracket IPv6 literals (`tcp/[::1]:7448`), while
        // `IpAddr::from_str` expects the bare address. Preserve this legacy
        // socket accessor for IPv6 callers even though `client_locator()` is the
        // authoritative endpoint representation.
        let ip_host = host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(host);
        let ip = ip_host
            .parse()
            .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
        SocketAddr::new(ip, port)
    }
}

impl ZenohAdapter {
    /// Pre-bind a per-topic publisher for `sender`. The returned publisher
    /// holds an `Arc<Session>` clone so its `publish` is independent of the
    /// `Arc<Mutex<Messenger>>` global lock.
    pub fn declare_topic_publisher(
        &self,
        sender: &TopicWireSender,
        qos: PublisherQoS,
    ) -> Result<ZenohPublisher> {
        self.declare_publisher_keyexpr(ZenohWireFormat::topic_publish(sender), qos)
    }

    /// Pre-bind a per-goal action-feedback publisher.
    pub fn declare_action_feedback_publisher(
        &self,
        recv: &ActionWireReceiver,
        link_id: &str,
        goal_id: &str,
        qos: PublisherQoS,
    ) -> Result<ZenohPublisher> {
        self.declare_publisher_keyexpr(
            ZenohWireFormat::action_feedback_publish(recv, link_id, goal_id),
            qos,
        )
    }

    fn declare_publisher_keyexpr(
        &self,
        topic: String,
        qos: PublisherQoS,
    ) -> Result<ZenohPublisher> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| Error::MessagingSessionError("Session not initialized".to_string()))?;
        Ok(ZenohPublisher {
            session: Arc::clone(session),
            topic,
            qos: ZenohQoS::from(qos),
        })
    }

    /// Waits until a subscriber whose key expression matches `keyexpr` is known
    /// to this session, or `timeout` elapses; returns whether a match was seen.
    ///
    /// Peer-mode gossip discovery is not instantaneous, so a freshly-connected
    /// publisher may not yet know about an existing subscriber and would drop
    /// its first send. Awaiting Zenoh's matching status (event-driven via the
    /// matching listener) makes that first reliable send deterministic. A
    /// short-lived publisher is declared purely to observe matching; the publish
    /// path itself is unchanged.
    pub async fn wait_for_matching_subscriber(
        &self,
        keyexpr: &str,
        timeout: std::time::Duration,
    ) -> Result<bool> {
        self.subscriber_match_wait(keyexpr.to_string())?
            .resolve(timeout)
            .await
    }

    /// Snapshot a detached [`SubscriberMatchWait`] for `keyexpr`. Cheap and
    /// non-blocking (clones the session handle); the wait itself happens in
    /// [`SubscriberMatchWait::resolve`], so callers sharing the adapter behind
    /// a lock can release it before waiting out the timeout (mirrors
    /// [`ActionLivelinessProbe`]).
    fn subscriber_match_wait(&self, keyexpr: String) -> Result<SubscriberMatchWait> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| Error::MessagingSessionError("Session not initialized".to_string()))?;
        Ok(SubscriberMatchWait {
            session: Arc::clone(session),
            keyexpr,
        })
    }

    /// Shared body of the liveliness declaration methods: token keyexprs
    /// differ per domain (action producers, core-node presence), the
    /// declaration mechanics don't.
    async fn declare_liveliness_token(&self, keyexpr: String) -> Result<LivelinessToken> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| Error::MessagingSessionError("Session not initialized".to_string()))?;
        let token = session
            .liveliness()
            .declare_token(keyexpr)
            .await
            .map_err(|e| Error::MessagingSessionError(e.to_string()))?;
        Ok(LivelinessToken::new(Box::new(token)))
    }

    /// Shared body of the liveliness watch methods. `extract` pulls the
    /// event value out of a sample (action watches carry `()`, presence
    /// watches parse the token key), returning `None` to drop the sample.
    ///
    /// The channel is unbounded: liveliness transitions are rare (producer
    /// restarts, router flaps) and the callback runs on a zenoh worker
    /// thread that must never block. See the module-level "Why callback
    /// handlers, not FIFO" doc. `history(true)` replays a token that was
    /// declared before this watch existed as an initial PUT, so "already
    /// alive" and "came alive" are observed identically.
    async fn watch_liveliness<T: Send + 'static>(
        &self,
        keyexpr: String,
        extract: impl Fn(&Sample) -> Option<T> + Send + Sync + 'static,
    ) -> Result<LivelinessWatch<T>> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| Error::MessagingSessionError("Session not initialized".to_string()))?;
        let (tx, rx) = flume::unbounded::<LivelinessEvent<T>>();
        let subscriber = session
            .liveliness()
            .declare_subscriber(&keyexpr)
            .history(true)
            .callback(move |sample| {
                let Some(value) = extract(&sample) else {
                    return;
                };
                let event = match sample.kind() {
                    SampleKind::Put => LivelinessEvent::Alive(value),
                    SampleKind::Delete => LivelinessEvent::Gone(value),
                };
                let _ = tx.send(event);
            })
            .await
            .map_err(|e| Error::MessagingSessionError(e.to_string()))?;
        Ok(LivelinessWatch::new(rx, Box::new(subscriber)))
    }

    /// [`wait_for_matching_subscriber`](Self::wait_for_matching_subscriber) for a
    /// topic, building the publish key expression from `sender`.
    pub async fn wait_for_topic_subscriber(
        &self,
        sender: &TopicWireSender,
        timeout: std::time::Duration,
    ) -> Result<bool> {
        self.topic_subscriber_wait(sender)?.resolve(timeout).await
    }

    /// [`subscriber_match_wait`](Self::subscriber_match_wait) for a topic,
    /// building the publish key expression from `sender`.
    pub fn topic_subscriber_wait(&self, sender: &TopicWireSender) -> Result<SubscriberMatchWait> {
        self.subscriber_match_wait(ZenohWireFormat::topic_publish(sender))
    }

    async fn publish_keyexpr(
        &self,
        keyexpr: &str,
        payload: Payload,
        qos: PublisherQoS,
        is_primary: bool,
    ) -> Result<()> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| Error::MessagingSessionError("Session not initialized".to_string()))?;
        let zenoh_qos = ZenohQoS::from(qos);

        // session.put() directly rather than declare_publisher() + put() + drop.
        // This avoids the publisher declaration/undeclare lifecycle that causes
        // routing interference between successive service polls with different
        // targeting.
        session
            .put(keyexpr, payload.as_bytes().as_ref())
            .attachment(TopicAttachment { is_primary }.encode().to_vec())
            .congestion_control(zenoh_qos.congestion_control)
            .priority(zenoh_qos.priority)
            .express(zenoh_qos.express)
            .await
            .map_err(|e| Error::PublishError {
                topic: e.to_string(),
            })?;
        Ok(())
    }

    async fn subscribe_keyexpr(
        &self,
        keyexpr: String,
        qos: SubscriberQoS,
        drop_secondary: bool,
    ) -> Result<Subscription> {
        let (tx, rx) = flume::bounded(self.client_config.buffer_sizes.size_for(qos));

        let session = self
            .session
            .as_ref()
            .ok_or_else(|| Error::MessagingSessionError("Session not initialized".to_string()))?;

        // Blocking `flume::Sender::send` (not `try_send`) so Reliable QoS
        // topics get end-to-end backpressure: if the consumer's buffer is
        // full, zenoh's reception thread blocks here, propagating the stall
        // back to the publisher. `Err` only fires once the receiver is
        // dropped — silently discard, the subscription is going away. See
        // the module-level "Why callback handlers, not FIFO" doc.
        let subscriber = session
            .declare_subscriber(&keyexpr)
            .callback(move |sample| {
                let SampleFields {
                    key_expr,
                    payload,
                    attachment,
                    timestamp,
                    ..
                } = sample.into();
                if drop_secondary {
                    let raw = attachment
                        .as_ref()
                        .map(|z| z.to_bytes())
                        .unwrap_or_default();
                    if !TopicAttachment::decode(raw.as_ref()).is_primary {
                        return;
                    }
                }
                // Producer-stamped send time (NTP64 → ns since the Unix epoch),
                // present when session/router timestamping is enabled. Surfaced
                // so consumers can measure real delivery latency.
                let source_timestamp_nanos = timestamp.as_ref().map(|ts| ts.get_time().as_nanos());
                let key_expr = key_expr.as_str();
                match TopicMessage::from_zbytes(key_expr, payload) {
                    Ok(message) => {
                        let _ =
                            tx.send(message.with_source_timestamp_nanos(source_timestamp_nanos));
                    }
                    Err(err) => {
                        tracing::error!(
                            %key_expr,
                            %err,
                            "Failed to build ResponseMessage from sample"
                        );
                    }
                }
            })
            .await
            .map_err(|e| Error::MessagingSessionError(e.to_string()))?;

        Ok(Subscription::new(rx, Box::new(subscriber)))
    }
}

/// Per-query inbound handler. Parses the selector, verifies the caller's
/// link_id slot resolves to the producer's default `_` segment via
/// [`ParsedInboundQuery::claim`], builds an [`IncomingRequest`] with a
/// [`ResponseToken::Zenoh`] (carrying the concrete reply keyexpr) and pushes
/// it onto `tx`.
///
/// The parser also re-validates concrete target core / instance slots against
/// this receiver. That is defensive: Zenoh's matcher should already have
/// filtered them out, but a stale peer-routing view must not let a pinned
/// action goal run on a sibling instance. Queries whose link_id slot is neither
/// `*` nor `_` are dropped silently for the same reason.
///
/// This runs inside zenoh's reception callback, so the function is sync —
/// `flume::Sender::send` blocks the zenoh worker thread when the buffer is
/// full so peppylib applies backpressure rather than losing requests, and
/// returns `Err` (silently ignored) only when the consumer has dropped the
/// `ServiceQueryable`.
fn process_inbound_query(
    query: zenoh::query::Query,
    recv: &ServiceWireReceiver,
    tx: &flume::Sender<IncomingRequest>,
    rt: &tokio::runtime::Handle,
) {
    let attachment_bytes = query.attachment().map(|z| z.to_bytes()).unwrap_or_default();
    let parsed = match ZenohWireFormat::parse_inbound_query(
        recv,
        query.key_expr().as_str(),
        attachment_bytes.as_ref(),
    ) {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(
                query_keyexpr = %query.key_expr().as_str(),
                %err,
                "failed to parse inbound service query selector",
            );
            return;
        }
    };

    let chosen_link_id = match parsed.claim() {
        Some(l) => l.to_string(),
        None => {
            tracing::trace!(
                query_keyexpr = %query.key_expr().as_str(),
                parsed_link_id = %parsed.link_id,
                "dropping inbound query: link_id slot is neither '*' nor '_'",
            );
            return;
        }
    };

    let reply_keyexpr = ZenohWireFormat::service_reply_keyexpr(
        recv,
        &chosen_link_id,
        &parsed.caller_core,
        &parsed.caller_inst,
    );

    let payload = match query.payload() {
        Some(zb) => Payload::from_zbytes(zb.clone()),
        None => Payload::from_bytes(bytes::Bytes::new()),
    };

    let token = ResponseToken::Zenoh(ZenohResponseToken::new(query, reply_keyexpr));

    // Probes (liveness, discovery, benchmark sized-probes) are answered
    // right here in the dispatch path and never reach the endpoint channel:
    // the endpoint's recv loop only runs while the producer task is parked
    // in it, so answering there would starve discovery whenever user code
    // is executing a handler or goal. The reply MUST be Response-kind
    // (never Ack) — the consumer's discover-then-pin loop pins identity
    // off the first non-Ack reply. The reply is spawned (not awaited):
    // this callback runs on a zenoh worker thread that must not block.
    if parsed.kind == ServiceQueryKind::Probe {
        let response = crate::probe::probe_response_body(payload.as_bytes().as_ref());
        rt.spawn(async move {
            if let Err(err) = token.respond_response(Payload::from_bytes(response)).await {
                tracing::warn!(%err, "failed to publish probe response");
            }
        });
        return;
    }

    let request = IncomingRequest {
        payload,
        kind: parsed.kind,
        link_id: chosen_link_id,
        caller_core: parsed.caller_core,
        caller_inst: parsed.caller_inst,
        token,
    };

    let _ = tx.send(request);
}

/// Zenoh-side per-topic publisher returned by [`ZenohAdapter::declare_publisher`].
///
/// Mirrors [`ZenohAdapter::publish`]'s `session.put()` path (NOT a long-lived
/// `zenoh::pubsub::Publisher`); see the comment there about routing
/// interference between successive service polls. The win here is bypassing
/// the central `Messenger` mutex; zenoh's session itself is lock-free for
/// `put`.
pub struct ZenohPublisher {
    session: Arc<zenoh::Session>,
    topic: String,
    qos: ZenohQoS,
}

/// In-flight wait for a matching subscriber, issued by
/// [`ZenohAdapter::topic_subscriber_wait`]. Owns its session handle, so
/// [`resolve`](Self::resolve) runs detached from the adapter and callers can
/// release any shared adapter lock before waiting out the timeout — the same
/// issue-then-resolve split as
/// [`ActionLivelinessProbe`](crate::types::ActionLivelinessProbe).
pub struct SubscriberMatchWait {
    session: Arc<zenoh::Session>,
    keyexpr: String,
}

impl SubscriberMatchWait {
    /// Waits until a subscriber whose key expression matches is known to the
    /// session, or `timeout` elapses; returns whether a match was seen.
    pub async fn resolve(self, timeout: std::time::Duration) -> Result<bool> {
        let publisher = self
            .session
            .declare_publisher(self.keyexpr)
            .await
            .map_err(|e| Error::MessagingSessionError(e.to_string()))?;

        if publisher
            .matching_status()
            .await
            .map_err(|e| Error::BackendError(e.to_string()))?
            .matching()
        {
            return Ok(true);
        }
        // Subscribe to changes, then re-check once: this closes the race where a
        // matching subscriber appears between the first query and the listener
        // being installed.
        let listener = publisher
            .matching_listener()
            .await
            .map_err(|e| Error::BackendError(e.to_string()))?;
        if publisher
            .matching_status()
            .await
            .map_err(|e| Error::BackendError(e.to_string()))?
            .matching()
        {
            return Ok(true);
        }

        let matched = tokio::time::timeout(timeout, async {
            loop {
                match listener.recv_async().await {
                    Ok(status) if status.matching() => return true,
                    Ok(_) => continue,
                    Err(_) => return false,
                }
            }
        })
        .await
        .unwrap_or(false);
        Ok(matched)
    }
}

impl ZenohPublisher {
    pub async fn publish(&self, payload: bytes::Bytes) -> Result<()> {
        // Pre-bound publishers are single-link (one keyexpr per declare),
        // so from a wildcard subscriber's view this publish is the only
        // one for its emit and must be marked primary. Topic publishers
        // that need multi-link fan-out should go through `emit`, not
        // `declare_publisher` — see the rustdoc on
        // `TopicMessenger::declare_publisher`.
        self.session
            .put(&self.topic, payload.as_ref())
            .attachment(TopicAttachment { is_primary: true }.encode().to_vec())
            .congestion_control(self.qos.congestion_control)
            .priority(self.qos.priority)
            .express(self.qos.express)
            .await
            .map_err(|e| Error::PublishError {
                topic: e.to_string(),
            })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `id` a rendered router config pins, read back off the file the way
    /// zenohd would.
    #[cfg(feature = "router")]
    fn rendered_id(config: &str) -> String {
        let parsed: serde_json::Value =
            serde_json::from_str(config).expect("a rendered router config is JSON");
        parsed["id"]
            .as_str()
            .expect("a rendered router config pins an id")
            .to_string()
    }

    #[test]
    fn create_client_config_rewrites_wildcard_host_and_defaults_the_seed() {
        // `0.0.0.0` must be rewritten to a connectable loopback host, and an
        // empty seed list falls back to the single `host:port` endpoint.
        let reconnecting = ZenohAdapter::create_client_config(
            ZenohNetProtocol::Tcp,
            "0.0.0.0",
            7448,
            true,
            Vec::new(),
            true,
            SubscriberBufferSizes::default(),
            None,
            None,
        );
        assert_eq!(reconnecting.host, "127.0.0.1");
        assert_eq!(
            reconnecting.seed_peers,
            vec!["tcp/127.0.0.1:7448".to_string()]
        );
        assert!(reconnecting.gossip);
    }

    #[test]
    fn create_client_config_honors_an_explicit_seed_list_and_buffer_sizes() {
        let cfg = ZenohAdapter::create_client_config(
            ZenohNetProtocol::Tcp,
            "127.0.0.1",
            7448,
            false,
            vec!["tcp/10.0.0.2:7448".to_string()],
            false,
            SubscriberBufferSizes {
                standard: 64,
                high_throughput: 4096,
            },
            None,
            None,
        );
        assert_eq!(cfg.seed_peers, vec!["tcp/10.0.0.2:7448".to_string()]);
        assert!(!cfg.gossip);
        assert_eq!(cfg.buffer_sizes.standard, 64);
        assert_eq!(cfg.buffer_sizes.high_throughput, 4096);
    }

    /// `refederate` re-renders the router's config in place with the upstream
    /// connect endpoint + connect-side trust — the live (de)federation the daemon
    /// drives on login/logout. `with_router` only renders + reads config (no
    /// zenohd process), so this is a pure file check.
    #[cfg(feature = "router")]
    #[test]
    fn refederate_rewrites_the_router_config_with_the_upstream_and_trust() {
        // A port unlikely to collide with other config-rendering tests (the
        // rendered config path is keyed by port).
        let port = 59247;
        let router_id = RouterId::parse("7f3a9c1e").expect("a valid router id literal");
        let mut adapter = ZenohAdapter::with_router(
            ZenohNetProtocol::Tcp,
            "127.0.0.1",
            port,
            false,
            SubscriberBufferSizes::default(),
            Vec::new(),
            None,
            router_id.clone(),
        )
        .expect("build standalone router adapter");

        let cfg_path = adapter
            .zenohd
            .as_ref()
            .expect("router adapter owns a facade")
            .managed_config_path()
            .expect("managed router owns a config path")
            .to_path_buf();
        let before = std::fs::read_to_string(&cfg_path).expect("read rendered config");
        assert!(
            !before.contains("tls/cap.zenoh.localhost:7443"),
            "a standalone router has no upstream connect endpoint"
        );
        assert_eq!(
            rendered_id(&before),
            router_id.to_string(),
            "the rendered config pins the identity it was built with"
        );

        let rewrote = adapter
            .refederate(
                vec!["tls/cap.zenoh.localhost:7443".to_string()],
                Some(TlsConfig::client(std::path::PathBuf::from("/certs/ca.pem"))),
            )
            .expect("refederate rewrites the config in place");
        assert!(rewrote, "a rendered config reports it was rewritten");

        let after = std::fs::read_to_string(&cfg_path).expect("read refederated config");
        assert!(
            after.contains("tls/cap.zenoh.localhost:7443"),
            "upstream connect endpoint is now present"
        );
        assert!(
            after.contains("/certs/ca.pem"),
            "connect-side CA trust is now present"
        );
        assert_eq!(
            rendered_id(&after),
            router_id.to_string(),
            "a federation change must keep the router's identity: minting a new zid here \
             would look, to anything reading the upstream's session list, like a different \
             machine arriving and the old one vanishing"
        );

        // refederate on an adapter that owns no router (a client) is an error.
        let mut clientish = ZenohAdapter::connect_to(ZenohNetProtocol::Tcp, "127.0.0.1", port)
            .expect("build client adapter");
        assert!(clientish.refederate(Vec::new(), None).is_err());

        let _ = std::fs::remove_file(&cfg_path);
    }

    #[cfg(feature = "router")]
    #[test]
    fn refederate_is_a_no_op_for_an_external_router() {
        let port = 59248;
        let mut adapter = ZenohAdapter::with_external_router(
            &format!("tcp/127.0.0.1:{port}"),
            false,
            SubscriberBufferSizes::default(),
        )
        .expect("build external router adapter");
        assert!(!adapter.router_is_adopted());

        let rewrote = adapter
            .refederate(
                vec!["tls/cap.zenoh.localhost:7443".to_string()],
                Some(TlsConfig::client(std::path::PathBuf::from("/certs/ca.pem"))),
            )
            .expect("refederate succeeds as a no-op for an external router");
        assert!(!rewrote);
    }

    #[cfg(feature = "router")]
    #[test]
    fn external_ipv6_locator_preserves_the_legacy_socket_address() {
        let adapter = ZenohAdapter::with_external_router(
            "tcp/[::1]:17448",
            false,
            SubscriberBufferSizes::default(),
        )
        .expect("build IPv6 external router adapter");

        assert_eq!(
            adapter.get_host(),
            "[::1]:17448".parse().expect("parse expected socket")
        );
        assert_eq!(adapter.client_locator().to_string(), "tcp/[::1]:17448");
    }
}
