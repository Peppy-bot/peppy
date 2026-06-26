use super::core_node::CoreNodeRunner;
use super::messaging_router::{MessagingRouter, teardown_budget_for};
use super::router_federation::RouterFederation;
use super::serve::{CompositeCommand, Serve};
use crate::daemon_state::DaemonState;
use crate::error::{Error, Result};
use config::peppy_config::PeppyConfig;
use pmi::Messenger;
use pmi::MessengerAdapter;
use pmi::MockAdapter;
use pmi::SubscriberBufferSizes;
use pmi::{ZenohAdapter, ZenohNetProtocol};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, watch};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// The git hash embedded at compile time by build.rs
const GIT_HASH: &str = env!("PEPPY_GIT_HASH");

const DEFAULT_NODE_STARTUP_TIMEOUT: Duration = Duration::from_secs(600); // 10 minutes
const DEFAULT_NODE_START_HEALTH_TIMEOUT: Duration = Duration::from_secs(15);

pub struct ServeCommandBuilder {
    composite_command: CompositeCommand,
    messenger: Option<Arc<Mutex<Messenger>>>,
    messaging_ready: Option<watch::Receiver<bool>>,
    core_node_requested: bool,
    core_node_name: Option<String>,
    clock_source: super::ClockSource,
    shutdown_token: Option<CancellationToken>,
    /// Sender the core node runner uses to tell the messaging router that
    /// teardown is done. Created alongside the messaging router so the router
    /// holds the receiver; handed to the core node runner in [`Self::build`].
    core_node_done_tx: Option<watch::Sender<bool>>,
    root_dir: PathBuf,
    peppy_config: PeppyConfig,
    /// Backend URL for per-user-router federation, set by
    /// [`with_messaging_router`](Self::with_messaging_router) for the `zenoh`
    /// engine. `Some` ⇒ [`build`](Self::build) spawns the [`RouterFederation`]
    /// task that keeps the local router federated to the cloud router.
    federation_api_url: Option<String>,
    /// The upstream connect endpoint the builder federated the local router to at
    /// startup (`None` if not logged in). Handed to the federation task as its
    /// baseline so it only acts on a later change.
    federation_initial_endpoint: Option<String>,
}

impl ServeCommandBuilder {
    pub fn new(root_dir: impl Into<PathBuf>) -> Result<Self> {
        Ok(Self {
            composite_command: CompositeCommand::default(),
            messenger: None,
            messaging_ready: None,
            core_node_requested: false,
            core_node_name: None,
            clock_source: super::ClockSource::default(),
            shutdown_token: None,
            core_node_done_tx: None,
            root_dir: root_dir.into(),
            peppy_config: PeppyConfig::default(),
            federation_api_url: None,
            federation_initial_endpoint: None,
        })
    }

    /// Supplies the daemon-global config (messaging mode + peer buffer sizes)
    /// read once at startup. Must be called before [`with_messaging_router`]
    /// (Self::with_messaging_router) so the daemon's own session is built in the
    /// configured mode.
    pub fn with_peppy_config(mut self, peppy_config: PeppyConfig) -> Self {
        self.peppy_config = peppy_config;
        self
    }

    pub fn with_shutdown_token(mut self, token: CancellationToken) -> Self {
        self.shutdown_token = Some(token);
        self
    }

    /// The messaging router (Zenoh/MQTT etc...) is reponsible for message passing between the nodes and between the nodes and the peppy program
    pub fn with_messaging_router(mut self, engine: String) -> Result<Self> {
        let engine = engine.to_lowercase();
        let listening_port = extract_messaging_port();
        let adapter = match engine.as_str() {
            "zenoh" => {
                // Reconnecting session: if the router watchdog respawns zenohd,
                // the daemon's own session re-establishes (and re-declares the
                // core node's services) instead of going silent. The session's
                // mode (peer vs router-relay) and buffer sizes come from the
                // daemon-global config read at startup.
                let buffer_sizes = SubscriberBufferSizes::from(self.peppy_config.peer);

                // Best-effort: if the user is logged in, federate the local router
                // to their per-user cloud router so messages cross between the two
                // routers as one network (only the inter-router hop is TLS; local
                // nodes stay plaintext loopback). Resolved synchronously here so
                // the FIRST zenohd already carries the upstream connect endpoint;
                // live (re)federation on a later login/logout is the
                // `RouterFederation` task's job (registered in `build`). If not
                // logged in / offline, this is `None` and the router is standalone.
                let api_url = crate::auth::profile::resolve_api_url(
                    None,
                    &self.peppy_config.resource_servers,
                )
                .ok();
                let federation = api_url
                    .as_deref()
                    .and_then(crate::auth::router::resolve_federation_target);
                let (connect_endpoints, federation_tls) = match &federation {
                    Some((endpoint, tls)) => {
                        info!(
                            target: "peppy::serve",
                            upstream = %endpoint,
                            "federating local router to the per-user cloud router"
                        );
                        (vec![endpoint.clone()], Some(tls.clone()))
                    }
                    None => (Vec::new(), None),
                };
                self.federation_initial_endpoint = federation.map(|(endpoint, _)| endpoint);
                self.federation_api_url = api_url;

                let adapter = ZenohAdapter::with_router(
                    ZenohNetProtocol::Tcp,
                    "0.0.0.0",
                    listening_port,
                    self.peppy_config.mode.gossip(),
                    buffer_sizes,
                    // Local nodes reach this router over plaintext loopback TCP;
                    // the only TLS is the (optional) federation link above.
                    connect_endpoints,
                    federation_tls,
                )?
                .with_session_reconnect();
                MessengerAdapter::Zenoh(adapter)
            }
            "mock" => MessengerAdapter::Mock(MockAdapter::default()),
            other => {
                warn!(target: "peppy::serve", "Unsupported messaging engine '{}', using mock", other);
                MessengerAdapter::Mock(MockAdapter::default())
            }
        };
        let messenger = Arc::new(Mutex::new(Messenger::new(adapter)));
        let (messaging_ready_tx, messaging_ready_rx) = watch::channel(false);
        // Shutdown-side counterpart of `messaging_ready`: the core node signals
        // this once teardown finishes, releasing the router to close the session.
        let (core_node_done_tx, core_node_done_rx) = watch::channel(false);
        // Keep the session open until the core node's worst-case teardown
        // finishes (cooperative node shutdown rides over it). Derived from the
        // same force_kill_deadline the teardown uses; see `teardown_budget_for`.
        let teardown_budget = teardown_budget_for(self.peppy_config.lifecycle.shutdown_grace_secs);
        self.messenger = Some(Arc::clone(&messenger));
        self.messaging_ready = Some(messaging_ready_rx);
        self.core_node_done_tx = Some(core_node_done_tx);
        self.composite_command =
            self.composite_command
                .add_async_command(Box::new(MessagingRouter::new(
                    messenger,
                    messaging_ready_tx,
                    Some(core_node_done_rx),
                    teardown_budget,
                )));
        Ok(self)
    }

    pub fn with_core_node(
        mut self,
        core_node_name: Option<String>,
        clock_source: super::ClockSource,
    ) -> Result<Self> {
        self.core_node_requested = true;
        self.core_node_name = core_node_name;
        self.clock_source = clock_source;
        Ok(self)
    }

    pub fn build(mut self) -> Result<Serve> {
        if self.core_node_requested {
            if let Some(messenger) = &self.messenger {
                // Capture the shutdown grace before `peppy_config` is moved into
                // the runner, so the daemon state file can advertise it to clients.
                let shutdown_grace_secs = self.peppy_config.lifecycle.shutdown_grace_secs;
                // The send half of the router's shutdown handshake, created in
                // `with_messaging_router`. Present whenever a messaging router
                // exists, which is required for a core node (checked above).
                let core_node_done_tx = self
                    .core_node_done_tx
                    .take()
                    .expect("core_node_done channel created in with_messaging_router");
                let core_node = CoreNodeRunner::new(
                    Arc::clone(messenger),
                    self.core_node_name.clone(),
                    DEFAULT_NODE_STARTUP_TIMEOUT,
                    DEFAULT_NODE_START_HEALTH_TIMEOUT,
                    self.root_dir.clone(),
                    self.messaging_ready.clone(),
                    self.clock_source,
                    self.peppy_config,
                    core_node_done_tx,
                );

                // Write the daemon state file with the core node name
                let core_node_name = core_node.node_name().to_string();
                let daemon_state = DaemonState::new(
                    &core_node_name,
                    messenger.blocking_lock().messaging_port(),
                    GIT_HASH,
                    shutdown_grace_secs,
                );
                let state_path = daemon_state.write().map_err(|e| {
                    Error::ExecutionFailed(format!("Failed to write daemon state: {}", e))
                })?;
                info!(
                    "Daemon state written to {} with core_node_name={}",
                    state_path.display(),
                    core_node_name
                );

                self.composite_command = self
                    .composite_command
                    .add_async_command(Box::new(core_node));
            } else {
                warn!("Commands listener requires a messaging router");
                return Err(Error::MissingMessagingRouter);
            }
        }

        // Per-user-router federation manager (zenoh engine only — other engines
        // never set `federation_api_url`). Keeps the cloud router alive and
        // (de)federates the local router live on login/logout. Started even when
        // not currently logged in, so a later login is picked up without a restart.
        if let Some(api_url) = self.federation_api_url.take()
            && let Some(messenger) = self.messenger.clone()
        {
            let initial = self.federation_initial_endpoint.take();
            self.composite_command = self
                .composite_command
                .add_async_command(Box::new(RouterFederation::new(messenger, api_url, initial)));
        }

        let serve = Serve::new(self.composite_command);
        let serve = match self.shutdown_token {
            Some(token) => serve.with_shutdown_token(token),
            None => serve,
        };
        Ok(serve)
    }
}

/// Extracts the messaging port from the environment variable, falling back to the default port.
fn extract_messaging_port() -> u16 {
    std::env::var(config::consts::PEPPY_MESSAGING_PORT_VAR_NAME)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(config::consts::DEFAULT_MESSAGING_PORT)
}
