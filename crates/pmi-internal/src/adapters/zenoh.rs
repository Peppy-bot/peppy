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
use crate::types::{
    IncomingRequest, NO_TIMEOUT_SENTINEL, Payload, PublisherQoS, ReplyStream, ResponseToken,
    ServiceQueryable, ServiceReply, SubscriberQoS, TopicMessage, ZenohResponseToken,
};
use crate::wire::zenoh_format::{ServiceReplyAttachment, TopicAttachment, ZenohWireFormat};
use crate::wire::{
    ActionWireReceiver, ActionWireSender, ServiceQueryKind, ServiceWireReceiver, ServiceWireSender,
    TopicWireReceiver, TopicWireSender,
};
use crate::zenohd::{self, ZenohNetProtocol};
#[cfg(feature = "router")]
use crate::{Messenger, MessengerAdapter};
use crate::{MessengerBackend, Subscription};
use askama::Template;

use std::net::SocketAddr;
#[cfg(feature = "router")]
use std::net::TcpListener;
use std::sync::Arc;
use tracing::info;

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
use zenoh::sample::SampleFields;

#[derive(Template)]
#[template(
    source = r#"{
    "mode": "client",
    "connect": {
        "endpoints": ["{{ protocol }}/{{ host }}:{{ port }}"]
    },
    "timestamping": {
        "enabled": { "client": true }
    }
}"#,
    ext = "txt"
)]
pub struct ZenohClientConfigTemplate {
    pub host: String,
    pub port: u16,
    pub protocol: zenohd::ZenohNetProtocol,
}

/// Client config for the daemon's long-lived session. Unlike the fail-fast
/// default, this retries the connection forever (`timeout_ms: -1`,
/// `exit_on_failure: false`) so the session re-establishes — and re-declares
/// its subscriptions/queryables — if the router is restarted under it (e.g. by
/// the router watchdog respawning zenohd).
#[derive(Template)]
#[template(
    source = r#"{
    "mode": "client",
    "connect": {
        "endpoints": ["{{ protocol }}/{{ host }}:{{ port }}"],
        "timeout_ms": -1,
        "exit_on_failure": false,
        "retry": {
            "period_init_ms": 1000,
            "period_max_ms": 4000,
            "period_increase_factor": 2.0
        }
    },
    "timestamping": {
        "enabled": { "client": true }
    }
}"#,
    ext = "txt"
)]
pub struct ZenohReconnectingClientConfigTemplate {
    pub host: String,
    pub port: u16,
    pub protocol: ZenohNetProtocol,
}

/// Client config for the router watchdog's liveness probe. Scouting is
/// disabled so the probe only ever tries the configured router endpoint (never
/// a multicast-discovered peer), making "is *our* router responsive?" a
/// deterministic question.
#[derive(Template)]
#[template(
    source = r#"{
    "mode": "client",
    "connect": {
        "endpoints": ["{{ protocol }}/{{ host }}:{{ port }}"]
    },
    "scouting": {
        "multicast": {
            "enabled": false
        }
    },
    "timestamping": {
        "enabled": { "client": true }
    }
}"#,
    ext = "txt"
)]
pub struct ZenohProbeClientConfigTemplate {
    pub host: String,
    pub port: u16,
    pub protocol: ZenohNetProtocol,
}

pub struct ZenohClientConfig {
    zenoh_config: zenoh::config::Config,
    host: String,
    port: u16,
    protocol: ZenohNetProtocol,
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
    /// Creates a ZenohAdapter that owns and manages its own zenohd router.
    /// Use this when you need to start a new router instance.
    #[cfg(feature = "router")]
    pub fn with_router(protocol: ZenohNetProtocol, host: &str, port: u16) -> Result<Self> {
        let zenohd_config_path = zenohd::router_config_path(protocol, host, port)?;
        let facade = zenohd::ZenohdFacade::new(zenohd_config_path)?;
        let client_config = Self::derive_client_config_from_zenohd(&facade);

        Ok(Self {
            zenohd: Some(facade),
            client_config,
            session: None,
            reconnect_session: false,
        })
    }

    /// Creates a ZenohAdapter that connects to an existing zenohd router.
    /// Use this when you want to connect to a router that's already running.
    pub fn connect_to(protocol: ZenohNetProtocol, host: &str, port: u16) -> Result<Self> {
        let client_config = Self::create_client_config(protocol, host, port, false);

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
    pub fn with_session_reconnect(mut self) -> Self {
        self.reconnect_session = true;
        self
    }

    /// Starts a zenohd router with an ephemeral port, retrying on bind failures.
    ///
    /// When `port` is `None`, automatically selects an available port and retries
    /// up to 32 times if the port becomes unavailable. When `port` is `Some`,
    /// attempts exactly once with that port.
    ///
    /// Returns a [`ZenohdInstance`] that automatically stops the router when dropped.
    #[cfg(feature = "router")]
    pub async fn start_router_ephemeral(host: &str, port: Option<u16>) -> Result<ZenohdInstance> {
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

            let adapter = Self::with_router(ZenohNetProtocol::Tcp, host, port)?;
            let probe_config = adapter.client_config.zenoh_config.clone();
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

    /// Builds a lock-free [`RouterHealthChecker`] bound to this adapter's router
    /// endpoint, for the router watchdog to probe liveness without holding the
    /// central messenger lock.
    #[cfg(feature = "router")]
    pub fn router_health_checker(&self) -> zenohd::RouterHealthChecker {
        let probe_str = ZenohProbeClientConfigTemplate {
            host: self.client_config.host.clone(),
            port: self.client_config.port,
            protocol: self.client_config.protocol,
        }
        .render()
        .expect("Failed to render probe client config template");
        let probe_config = zenoh::config::Config::from_json5(&probe_str)
            .expect("Failed to create probe client config");
        zenohd::RouterHealthChecker::new(probe_config)
    }

    fn create_client_config(
        protocol: ZenohNetProtocol,
        host: &str,
        port: u16,
        reconnect: bool,
    ) -> ZenohClientConfig {
        let connect_host = if host == "0.0.0.0" {
            "127.0.0.1".to_string()
        } else {
            host.to_string()
        };

        // The long-lived daemon session uses the reconnecting template so it
        // survives a router restart; everything else (CLI connects, readiness
        // probes) uses the fail-fast template so a down router errors quickly
        // instead of blocking on retries.
        let client_config_str = if reconnect {
            ZenohReconnectingClientConfigTemplate {
                host: connect_host.clone(),
                port,
                protocol,
            }
            .render()
            .expect("Failed to render reconnecting client config template")
        } else {
            ZenohClientConfigTemplate {
                host: connect_host.clone(),
                port,
                protocol,
            }
            .render()
            .expect("Failed to render client config template")
        };

        let client_config = zenoh::config::Config::from_json5(&client_config_str)
            .expect("Failed to create client config");

        ZenohClientConfig {
            zenoh_config: client_config,
            host: connect_host,
            port,
            protocol,
        }
    }

    #[cfg(feature = "router")]
    fn derive_client_config_from_zenohd(zenohd: &zenohd::ZenohdFacade) -> ZenohClientConfig {
        // Fail-fast: this config also backs the ephemeral readiness probe in
        // `start_router_ephemeral`, which relies on `zenoh::open` erroring
        // quickly. The daemon's reconnecting session is built separately in
        // `start_session` when `reconnect_session` is set.
        Self::create_client_config(
            zenohd.zenoh_endpoint.protocol,
            &zenohd.zenoh_endpoint.host,
            zenohd.zenoh_endpoint.port,
            false,
        )
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
            )
            .zenoh_config
        } else {
            self.client_config.zenoh_config.clone()
        };

        let session = zenoh::open(config)
            .await
            .map_err(|e| Error::BackendError(format!("Failed to create Zenoh session: {}", e)))?;

        info!(
            "Zenoh session started on: {}://{}:{}",
            &self.client_config.protocol, &self.client_config.host, &self.client_config.port
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
        // Wildcard subscribers (from_link_id: None) match every per-link_id
        // publish a multi-link `emit` produces and must drop secondaries —
        // see the topic-attachment block in `wire::zenoh_format`. Pinned
        // subscribers ignore the attachment because their keyexpr already
        // selects a single publish per emit.
        //
        // The exclusion bypass: when the consumer has registered a
        // sibling-pinned set, peppylib filters by `link_id()` above the
        // adapter (the primary may be excluded and the secondary may be
        // the one to keep). Dropping secondaries here would silence the
        // only acceptable publish in that case. The peppylib filter then
        // dedupes alone — relying on "at most one bound link_id is not in
        // the excluded set" — which holds because peppylib's
        // `MessengerHandle::reserve_from_any_topic` rejects a second
        // from_any subscription on the same `(name, tag)` at subscribe
        // time, making it the runtime enforcer of the manifest validator's
        // invariant.
        let drop_secondary = recv.from_link_id.is_none() && !recv.defers_secondary_drop;
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

        let (tx, rx) = flume::bounded::<IncomingRequest>(SubscriberQoS::Standard.channel_size());

        // One queryable per listen call. The declared keyexpr has `*` at the
        // link_id slot so a single queryable absorbs every bound link_id —
        // `process_inbound_query` does the dispatch by parsing the selector.
        // Two queryables for one process would let a `from_any` consumer's
        // `*` selector double-deliver via `QueryTarget::All`.
        let declare_keyexpr = ZenohWireFormat::service_queryable_declare(recv);
        let recv_clone = recv.clone();
        let queryable = session
            .declare_queryable(&declare_keyexpr)
            .complete(true)
            .callback(move |query| {
                process_inbound_query(query, &recv_clone, &tx);
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

        let timeout = timeout.unwrap_or(NO_TIMEOUT_SENTINEL);

        let (tx, rx) =
            tokio::sync::mpsc::channel::<ServiceReply>(SubscriberQoS::Standard.channel_size());

        // `try_send` (not `send`) because the callback runs synchronously on
        // a zenoh worker thread that we must not block. Two drop conditions
        // are tolerated here:
        //   1. receiver dropped — caller has the first valid reply and has
        //      released the `ReplyStream`; sibling producers' late replies
        //      go nowhere, which is intentional;
        //   2. channel full (capacity = `SubscriberQoS::Standard.channel_size`)
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
                    Err(err) => {
                        tracing::warn!(?err, "service reply contained an error");
                        return;
                    }
                };
                let key_expr = sample.key_expr().as_str();
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

    async fn start_router(&mut self) -> Result<()> {
        #[cfg(feature = "router")]
        {
            let zenohd = self
                .zenohd
                .as_mut()
                .ok_or(Error::ZenohDConfigurationNotFound)?;
            zenohd.start_router()?;
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
            zenohd.stop_router()?;
            Ok(())
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
        let ip = host
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
        let (tx, rx) = flume::bounded(qos.channel_size());

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
/// Probe / ACK semantics are handled by peppylib's request loop, not here —
/// every claimed query (including probes) is delivered to peppylib via
/// `tx`, and peppylib decides whether to reply inline or hand the request
/// to the user handler. Queries whose link_id slot is neither `*` nor `_`
/// are dropped silently (defensive — Zenoh's matcher should already have
/// filtered them out).
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

    #[test]
    fn reconnecting_and_probe_configs_parse() {
        // Guards the JSON5 schemas: a malformed connect/scouting block would
        // otherwise panic at daemon startup inside `create_client_config` /
        // `router_health_checker` (both `.expect()` on parse).
        let reconnecting =
            ZenohAdapter::create_client_config(ZenohNetProtocol::Tcp, "0.0.0.0", 7448, true);
        // `0.0.0.0` must be rewritten to a connectable loopback host.
        assert_eq!(reconnecting.host, "127.0.0.1");

        let fail_fast =
            ZenohAdapter::create_client_config(ZenohNetProtocol::Tcp, "127.0.0.1", 7448, false);
        assert_eq!(fail_fast.port, 7448);

        let probe_str = ZenohProbeClientConfigTemplate {
            host: "127.0.0.1".to_string(),
            port: 7448,
            protocol: ZenohNetProtocol::Tcp,
        }
        .render()
        .expect("probe template renders");
        zenoh::config::Config::from_json5(&probe_str).expect("probe config parses");
    }
}
