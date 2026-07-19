use super::core_node::CoreNodeRunner;
use super::federation_control::FederationControl;
use super::messaging_router::{MessagingRouter, teardown_budget_for};
use super::router_federation::RouterFederation;
use super::serve::{CompositeCommand, Serve};
use crate::error::{Error, Result};
use crate::state::DaemonState;
use daemon_config::consts::PeppyDirs;
use daemon_config::peppy_config::PeppyConfig;
use pmi::Messenger;
use pmi::MessengerAdapter;
use pmi::MockAdapter;
use pmi::RouterLinks;
use pmi::SubscriberBufferSizes;
use pmi::{ZenohAdapter, ZenohNetProtocol};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, watch};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

const DEFAULT_NODE_STARTUP_TIMEOUT: Duration = Duration::from_secs(600); // 10 minutes
const DEFAULT_NODE_START_HEALTH_TIMEOUT: Duration = Duration::from_secs(15);

pub struct ServeCommandBuilder {
    composite_command: CompositeCommand,
    messenger: Option<Arc<Mutex<Messenger>>>,
    messaging_ready: Option<watch::Receiver<bool>>,
    core_node_requested: bool,
    core_node_name: Option<String>,
    clock_source: crate::ClockSource,
    shutdown_token: Option<CancellationToken>,
    /// Sender the core node runner uses to tell the messaging router that
    /// teardown is done. Created alongside the messaging router so the router
    /// holds the receiver; handed to the core node runner in [`Self::build`].
    core_node_done_tx: Option<watch::Sender<bool>>,
    root_dir: PathBuf,
    /// The binary's compile-time git hash, recorded in the daemon state file.
    /// Passed in by the embedding binary (this crate reads no build-time env).
    git_hash: String,
    /// The peppy data root for this generation, threaded from
    /// [`ServeOptions`](crate::ServeOptions): the daemon state file, the
    /// federation control socket, and the core node's storage all derive
    /// their paths from it.
    peppy_dirs: PeppyDirs,
    peppy_config: PeppyConfig,
    /// Backend URL for platform federation, set by
    /// [`with_messaging_router`](Self::with_messaging_router) for the `zenoh`
    /// engine. `Some` means [`build`](Self::build) spawns the
    /// [`RouterFederation`] task that federates the local router to the
    /// platform router (and keeps it federated across login/logout). The local
    /// router is always started *standalone*; the task applies the federation
    /// off the startup path so a slow/unreachable backend can never stall
    /// daemon startup beyond the federation connect timeout (the core node's
    /// boot presence check waits, that bounded long at most, for the initial
    /// federation to settle, so name collisions across the federated mesh
    /// refuse boot; see `build`).
    federation_api_url: Option<String>,
    /// Bound on the federation backend round-trip (the startup gate and each
    /// resolve). Read from `peppy_config.zenoh.managed.federation` in
    /// [`with_messaging_router`](Self::with_messaging_router) before the config is
    /// moved into the core node, and shared by [`RouterFederation`] and
    /// [`FederationControl`].
    federation_connect_timeout: Duration,
    /// Whether the managed router was built from an operator-pinned
    /// `ZENOH_CONFIG` file, captured when the adapter is created and handed to
    /// the federation task so its first poll reports ownership correctly.
    router_pinned: bool,
    /// The namespace resolved once for this daemon generation (`"local"` when
    /// logged out, else the workspace id). Resolved in
    /// [`with_messaging_router`](Self::with_messaging_router) from the cached
    /// credentials and applied to the daemon's own session there; also threaded
    /// into [`DaemonState`], the core node (and thus every spawned node), and the
    /// [`RouterFederation`] task (which compares against it to decide restart vs
    /// live re-federate). A single source for the whole generation.
    namespace: config::namespace::Namespace,
    /// The shared coordinator token for this generation: cloned into every serve
    /// task (so a restart/stop unparks them for graceful teardown) and handed to
    /// [`Serve`] (which cancels it on its way out). Created per generation.
    teardown_token: CancellationToken,
}

impl ServeCommandBuilder {
    pub fn new(
        root_dir: impl Into<PathBuf>,
        git_hash: impl Into<String>,
        peppy_dirs: PeppyDirs,
    ) -> Result<Self> {
        Ok(Self {
            composite_command: CompositeCommand::default(),
            messenger: None,
            messaging_ready: None,
            core_node_requested: false,
            core_node_name: None,
            clock_source: crate::ClockSource::default(),
            shutdown_token: None,
            core_node_done_tx: None,
            root_dir: root_dir.into(),
            git_hash: git_hash.into(),
            peppy_dirs,
            peppy_config: PeppyConfig::default(),
            federation_api_url: None,
            federation_connect_timeout: Duration::from_secs(
                daemon_config::peppy_config::DEFAULT_FEDERATION_CONNECT_TIMEOUT_SECS,
            ),
            router_pinned: false,
            // Default for the mock/other engines that never resolve a namespace;
            // the zenoh path overwrites this in `with_messaging_router`.
            namespace: config::namespace::Namespace::local(),
            teardown_token: CancellationToken::new(),
        })
    }

    /// Supplies the daemon-global config (messaging mode + subscriber buffer sizes)
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

    pub(crate) fn messenger_handle(&self) -> Option<Arc<Mutex<Messenger>>> {
        self.messenger.clone()
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
                // topology (peer vs router-relay) and subscriber buffer sizes come
                // from the daemon-global config read at startup.
                let subscriber_buffers =
                    SubscriberBufferSizes::from(self.peppy_config.zenoh.subscriber_buffers());

                // A managed local router starts STANDALONE here. Federating it
                // to the platform router needs a backend round-trip, done off
                // this synchronous startup path by `RouterFederation`.
                // External routers are entirely operator-run, so federation is
                // not armed and no control socket or presence gate is created.
                // `resolve_api_url` is a local config/env lookup (no I/O), so it
                // is safe here; an invalid URL fails startup loudly rather than
                // silently degrading the daemon to standalone mode.
                if let Some(federation_config) = self.peppy_config.zenoh.federation() {
                    let api_url =
                        auth::profile::resolve_api_url(None, &self.peppy_config.resource_servers)
                            .map_err(|e| {
                            Error::ExecutionFailed(format!(
                                "invalid managed-federation backend URL: {e}"
                            ))
                        })?;
                    self.federation_api_url = Some(api_url);
                    // Capture the federation timeout here, before `peppy_config`
                    // is moved into the core node in `build`; both the federation
                    // task and its control socket share it.
                    self.federation_connect_timeout =
                        Duration::from_secs(federation_config.connect_timeout_secs);
                }

                // Resolve this generation's namespace once, from the credentials
                // cached under this run's data root: `"local"` when logged out,
                // else the workspace id. It is the single source threaded into
                // the daemon's own session (here), `DaemonState`, every spawned
                // node, and the federation task. The router itself is never
                // namespaced (it only forwards), so the namespace rides only on
                // application sessions.
                let namespace = auth::router::cached_namespace(&auth::storage::credentials_path(
                    &self.peppy_dirs,
                ))
                .unwrap_or_else(config::namespace::Namespace::local);
                self.namespace = namespace.clone();

                // The effective gossip bit for this whole generation: the
                // configured local topology, forced off under a workspace
                // namespace so an authenticated daemon relays everything
                // through its router (and, transitively, the platform hub)
                // instead of forming direct links that bypass it. Drives the
                // daemon session's mode (peer vs client), every spawned node's
                // defaults, AND the managed router's own gossip scouting, so
                // the three flip together only across generation restarts.
                let gossip = self.peppy_config.zenoh.session_gossip(&namespace);
                let adapter = match self.peppy_config.zenoh.external_endpoint() {
                    Some(endpoint) => {
                        ZenohAdapter::with_external_router(endpoint, gossip, subscriber_buffers)?
                    }
                    None => ZenohAdapter::with_router(
                        ZenohNetProtocol::Tcp,
                        "0.0.0.0",
                        listening_port,
                        gossip,
                        subscriber_buffers,
                        // Plaintext, standalone spawn: local nodes reach this
                        // router over loopback TCP. The federation task applies
                        // the platform TLS upstream after startup.
                        RouterLinks::default(),
                    )?,
                }
                .with_session_reconnect()
                .with_namespace(Some(namespace));
                MessengerAdapter::Zenoh(adapter)
            }
            "mock" => MessengerAdapter::Mock(MockAdapter::default()),
            other => {
                warn!(target: "daemon::serve", "Unsupported messaging engine '{}', using mock", other);
                MessengerAdapter::Mock(MockAdapter::default())
            }
        };
        self.router_pinned = match &adapter {
            MessengerAdapter::Zenoh(adapter) => adapter.router_config_is_pinned(),
            MessengerAdapter::Mock(_) => false,
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
                    self.teardown_token.clone(),
                )));
        Ok(self)
    }

    pub fn with_core_node(
        mut self,
        core_node_name: Option<String>,
        clock_source: crate::ClockSource,
    ) -> Result<Self> {
        self.core_node_requested = true;
        self.core_node_name = core_node_name;
        self.clock_source = clock_source;
        Ok(self)
    }

    pub fn build(mut self) -> Result<Serve> {
        // The core-node name this generation materialized (explicit or derived),
        // captured out of the core-node block for the federation task below: the
        // federation POST registers this daemon under it in the backend's
        // core-node registry. `None` only when no core node was requested, in
        // which case there is nothing to register and federation stays unarmed.
        let mut federation_core_node_name: Option<String> = None;
        // Presence-check ordering gate: when a federation task will be armed
        // below, the core node delays its boot-time presence check and token
        // declaration until the *initial* federation poll has settled, so it
        // sees the federated mesh rather than the always-standalone just-started
        // local router (a same-name daemon reachable only through the per-user
        // cloud router must refuse boot). RouterFederation fires the sender in
        // lockstep with its startup readiness gate, so the wait is bounded by
        // `federation_connect_timeout` and fail-open (dropped sender ⇒ the core
        // node proceeds standalone).
        let (federation_settled_tx, federation_settled_rx) =
            if self.core_node_requested && self.federation_api_url.is_some() {
                let (tx, rx) = watch::channel(false);
                (Some(tx), Some(rx))
            } else {
                (None, None)
            };
        if self.core_node_requested {
            if let Some(messenger) = &self.messenger {
                // Precedence: `--core-node-name` beats the config's
                // `core_node_name`; both absent ⇒ `None`, and the core node
                // derives its machine-specific default. Resolved (and an explicit
                // name validated) here, before `peppy_config` is moved into the
                // runner.
                let resolved_core_node_name = resolve_core_node_name(
                    self.core_node_name.clone(),
                    self.peppy_config.core_node_name.clone(),
                )?;
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
                // Federated daemons (a managed router with configured
                // `connect` links — an operator-pinned mesh) hold the boot
                // presence claim open for the settle window, so the claim
                // observes an incumbent whose token is still propagating
                // across the freshly-established links. Standalone routers
                // (and the mock/external engines) are authoritative
                // immediately and skip the wait. The probe only reads the
                // active router config; nothing has started yet.
                let name_claim_settle = if messenger.blocking_lock().router_links_probe().is_some()
                {
                    core_node::NAME_CLAIM_LINKED_SETTLE
                } else {
                    Duration::ZERO
                };
                let core_node = CoreNodeRunner::new(
                    Arc::clone(messenger),
                    resolved_core_node_name,
                    DEFAULT_NODE_STARTUP_TIMEOUT,
                    DEFAULT_NODE_START_HEALTH_TIMEOUT,
                    self.root_dir.clone(),
                    self.peppy_dirs.clone(),
                    self.messaging_ready.clone(),
                    federation_settled_rx,
                    self.clock_source,
                    self.peppy_config,
                    self.namespace.clone(),
                    name_claim_settle,
                    self.teardown_token.clone(),
                    core_node_done_tx,
                );

                // Write the daemon state file with the core node name. The
                // organization namespace is recorded here, before the control
                // socket binds (below), so a CLI control session that reads it
                // never sees a half-set generation.
                let core_node_name = core_node.node_name().to_string();
                let daemon_state = {
                    let messenger = messenger.blocking_lock();
                    daemon_state_for_messenger(
                        &messenger,
                        &core_node_name,
                        &self.git_hash,
                        shutdown_grace_secs,
                        self.namespace.as_str(),
                        // `Some` exactly when this generation arms managed-router
                        // federation below (a control socket will exist), so the
                        // auth commands can follow the running daemon's mode.
                        self.federation_api_url
                            .as_ref()
                            .map(|_| self.federation_connect_timeout.as_secs()),
                    )
                };
                let state_path = DaemonState::state_file_in(self.peppy_dirs.root());
                DaemonState::write_to(&state_path, &daemon_state).map_err(|e| {
                    Error::ExecutionFailed(format!("Failed to write daemon state: {}", e))
                })?;
                info!(
                    "Daemon state written to {} with core_node_name={}",
                    state_path.display(),
                    core_node_name
                );
                federation_core_node_name = Some(core_node_name);

                self.composite_command = self
                    .composite_command
                    .add_async_command(Box::new(core_node));
            } else {
                warn!("Commands listener requires a messaging router");
                return Err(Error::MissingMessagingRouter);
            }
        }

        // Per-user-router federation manager (zenoh engine only; other engines
        // never set `federation_api_url`). Applies the initial federation once the
        // router is up (gating `serve` reporting ready, bounded by the timeout),
        // keeps the cloud router alive, and (de)federates the local router live on
        // login/logout: immediately when poked over the control socket, else on
        // the next poll. It waits on `messaging_ready` before touching the router,
        // so it can't race MessagingRouter's initial `start_router`.
        // In-process restart channel. Armed only for managed zenoh when the
        // federation API URL resolves; external zenoh and the mock engine have no
        // federation control channel and never restart through this path.
        let mut restart_rx: Option<watch::Receiver<bool>> = None;
        if self.federation_api_url.is_some() && federation_core_node_name.is_none() {
            // Only reachable when a zenoh router was built without a core node
            // (never the serve path): the federation POST must carry a core-node
            // name, so with none materialized there is nothing to register.
            warn!("Router federation requires a core node; staying standalone");
        }
        if let Some(api_url) = self.federation_api_url.take()
            && let Some(messenger) = self.messenger.clone()
            && let Some(messaging_ready) = self.messaging_ready.clone()
            && let Some(core_node_name) = federation_core_node_name
        {
            let connect_timeout = self.federation_connect_timeout;
            // Poke channel: `auth login`/`logout` reach the federation loop through
            // the control socket so a login is federated immediately, not on the
            // next poll. Bounded + tiny: pokes are rare and serviced one at a time.
            let (trigger_tx, trigger_rx) = tokio::sync::mpsc::channel(8);
            // Restart signal: the control handler raises it after flushing the
            // `Restarting` ack; the serve coordinator observes it.
            let (restart_tx, restart_signal_rx) = watch::channel(false);
            restart_rx = Some(restart_signal_rx);
            self.composite_command =
                self.composite_command
                    .add_async_command(Box::new(RouterFederation::new(
                        messenger,
                        api_url,
                        // This generation's core-node name, carried in every
                        // federation POST so the backend registry knows which
                        // daemon pulled the config.
                        core_node_name,
                        // The data root the loop's per-poll credential reads
                        // and materialized dev TLS derive from.
                        self.peppy_dirs.clone(),
                        messaging_ready,
                        trigger_rx,
                        connect_timeout,
                        // This generation's namespace: the federation loop compares
                        // the namespace it re-resolves from fresh creds against this
                        // to decide live re-federate (unchanged) vs restart (changed).
                        self.namespace.as_str().to_string(),
                        // The startup poll raises this if it resolves a namespace
                        // that differs from this generation's (the steady-state
                        // poke path leaves the restart to the control handler).
                        restart_tx.clone(),
                        // Opens the core node's presence-check gate once the initial
                        // federation poll settles (see above).
                        federation_settled_tx,
                        // Router-config ownership captured when the adapter was
                        // built, so the first status reports pinned correctly.
                        self.router_pinned,
                        self.teardown_token.clone(),
                    )));

            // Control socket the CLI pokes. Derived from this run's `PeppyDirs`
            // (the same root the CLI resolves by default), so the two agree
            // without a discovery handshake.
            let socket_path = crate::control::federation_control_socket_path(&self.peppy_dirs);
            self.composite_command =
                self.composite_command
                    .add_async_command(Box::new(FederationControl::new(
                        socket_path,
                        trigger_tx,
                        connect_timeout,
                        restart_tx,
                        self.teardown_token.clone(),
                    )));
        }

        let mut serve = Serve::new(self.composite_command).with_teardown_token(self.teardown_token);
        if let Some(rx) = restart_rx {
            serve = serve.with_restart_rx(rx);
        }
        let serve = match self.shutdown_token {
            Some(token) => serve.with_shutdown_token(token),
            None => serve,
        };
        Ok(serve)
    }
}

/// Builds the state-file payload from the messenger endpoint selected by the
/// builder. Keeping endpoint extraction and [`DaemonState::new`] together makes
/// the full locator (including an operator-configured host and port) the single
/// source used by [`ServeCommandBuilder::build`]. Mock backends retain the
/// historical loopback-host fallback.
fn daemon_state_for_messenger(
    messenger: &Messenger,
    core_node_name: &str,
    git_hash: &str,
    shutdown_grace_secs: u64,
    namespace: &str,
    federation_connect_timeout_secs: Option<u64>,
) -> DaemonState {
    let (messaging_host, messaging_port) = messenger
        .messaging_locator()
        .map(|endpoint| (endpoint.host().to_string(), endpoint.port()))
        .unwrap_or_else(|| {
            (
                config::consts::DEFAULT_MESSAGING_HOST.to_string(),
                messenger.messaging_port(),
            )
        });
    DaemonState::new(
        core_node_name,
        messaging_host,
        messaging_port,
        git_hash,
        shutdown_grace_secs,
        namespace,
        federation_connect_timeout_secs,
    )
}

/// Extracts the messaging port from the environment variable, falling back to the default port.
pub(crate) fn extract_messaging_port() -> u16 {
    std::env::var(daemon_config::consts::PEPPY_MESSAGING_PORT_VAR_NAME)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(config::consts::DEFAULT_MESSAGING_PORT)
}

/// Resolves the core-node name for one daemon generation: the
/// `--core-node-name` flag wins, else `core_node_name` from
/// `peppy_config.json5`, else `None` (the core node derives its
/// machine-specific default). An explicit name is validated with the same
/// `Name` rules (and length cap) the daemon applies, so a bad flag value fails
/// here with an actionable error instead of panicking inside `CoreNode::new`.
fn resolve_core_node_name(flag: Option<String>, config: Option<String>) -> Result<Option<String>> {
    let (name, source) = match (flag, config) {
        (Some(name), _) => (name, "--core-node-name"),
        (None, Some(name)) => (name, "core_node_name in peppy_config.json5"),
        (None, None) => return Ok(None),
    };
    if config::runtime::Name::new(name.as_str()).is_err()
        || name.len() > daemon_config::peppy_config::MAX_CORE_NODE_NAME_LEN
    {
        return Err(Error::ExecutionFailed(format!(
            "invalid core node name {name:?} (from {source}): must be non-empty, at most {} \
             characters, and use only characters from \"{}\"",
            daemon_config::peppy_config::MAX_CORE_NODE_NAME_LEN,
            config::consts::ALLOWED_CONFIG_CHARS
        )));
    }
    Ok(Some(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    // PMI's managed config filename is keyed by the messaging port, so tests
    // that successfully render and then inspect it must not overlap.
    static MANAGED_ROUTER_CONFIG_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn some(s: &str) -> Option<String> {
        Some(s.to_string())
    }

    #[test]
    fn flag_beats_config() {
        let resolved = resolve_core_node_name(some("from-flag"), some("from-config"))
            .expect("both names valid");
        assert_eq!(resolved.as_deref(), Some("from-flag"));
    }

    #[test]
    fn config_beats_derivation() {
        let resolved = resolve_core_node_name(None, some("from-config")).expect("valid name");
        assert_eq!(resolved.as_deref(), Some("from-config"));
    }

    #[test]
    fn absent_everywhere_passes_none_through_to_derivation() {
        let resolved = resolve_core_node_name(None, None).expect("nothing to validate");
        assert_eq!(resolved, None);
    }

    /// An invalid `--core-node-name` must come back as an actionable
    /// `ExecutionFailed`, not reach `CoreNode::new`'s `Name::new(...).unwrap()`
    /// panic path.
    #[test]
    fn invalid_explicit_name_errors_instead_of_panicking() {
        for bad in ["", "has space", "robot/7"] {
            let err = resolve_core_node_name(some(bad), None)
                .expect_err("an invalid explicit name must be rejected");
            let msg = err.to_string();
            assert!(
                matches!(err, Error::ExecutionFailed(_)),
                "expected ExecutionFailed for {bad:?}, got: {msg}"
            );
            assert!(
                msg.contains("--core-node-name"),
                "the error names the flag the bad value came from: {msg}"
            );
        }
    }

    /// The flag enforces the same length cap the config file does, so the two
    /// sources cannot diverge on what a valid name is.
    #[test]
    fn explicit_name_length_cap_matches_the_config_cap() {
        let max = "n".repeat(daemon_config::peppy_config::MAX_CORE_NODE_NAME_LEN);
        assert_eq!(
            resolve_core_node_name(some(&max), None)
                .expect("boundary length accepted")
                .as_deref(),
            Some(max.as_str())
        );

        let over = "n".repeat(daemon_config::peppy_config::MAX_CORE_NODE_NAME_LEN + 1);
        let err = resolve_core_node_name(None, some(&over))
            .expect_err("an over-long name must be rejected");
        assert!(
            err.to_string()
                .contains("core_node_name in peppy_config.json5"),
            "the error names the config source: {err}"
        );
    }

    /// Pins the complete production handoff for an operator-run router:
    /// `PeppyConfig` selects the external PMI constructor, that constructor
    /// retains the non-default dial locator, and the same helper `build()` calls
    /// copies its host + port into `DaemonState`.
    #[test]
    fn external_router_endpoint_flows_from_config_through_builder_into_daemon_state() {
        const ENDPOINT: &str = "tcp/zenoh-router.regression.test:17555";
        let peppy_config = PeppyConfig {
            zenoh: daemon_config::peppy_config::ZenohConfig::External(
                daemon_config::peppy_config::ExternalZenohConfig {
                    endpoint: ENDPOINT.to_string(),
                },
            ),
            ..PeppyConfig::default()
        };

        let builder =
            ServeCommandBuilder::new("/unused", "regression-git-hash", PeppyDirs::new("/unused"))
                .expect("create builder")
                .with_peppy_config(peppy_config)
                .with_messaging_router("zenoh".to_string())
                .expect("build external messaging adapter without starting it");
        assert!(
            builder.federation_api_url.is_none(),
            "external mode must not arm router federation"
        );
        let messenger = builder
            .messenger_handle()
            .expect("builder retains its messenger");
        let mut messenger = messenger.blocking_lock();

        let adapter = match &messenger.adapter {
            MessengerAdapter::Zenoh(adapter) => adapter,
            MessengerAdapter::Mock(_) => panic!("zenoh config must select the Zenoh adapter"),
        };
        assert_eq!(adapter.client_locator().to_string(), ENDPOINT);
        assert_eq!(
            adapter.client_endpoint(),
            ("zenoh-router.regression.test", 17555)
        );
        assert!(
            !messenger
                .refederate(RouterLinks {
                    upstream: Some("tcp/unused.example:7448".to_string()),
                    tls: None,
                })
                .expect("external refederation is a no-op"),
            "an external adapter must not own a router config to rewrite"
        );

        let state = daemon_state_for_messenger(
            &messenger,
            "regression-core",
            "regression-git-hash",
            42,
            config::namespace::LOCAL_NAMESPACE,
            builder
                .federation_api_url
                .as_ref()
                .map(|_| builder.federation_connect_timeout.as_secs()),
        );
        assert_eq!(state.messaging_host, "zenoh-router.regression.test");
        assert_eq!(state.messaging_port, 17555);
        assert_eq!(
            state.federation_connect_timeout_secs, None,
            "external mode must record no federation control channel in the daemon state"
        );
    }

    #[test]
    fn managed_router_arms_federation() {
        let _guard = MANAGED_ROUTER_CONFIG_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let peppy_config = PeppyConfig {
            zenoh: daemon_config::peppy_config::ZenohConfig::Managed(
                daemon_config::peppy_config::ManagedZenohConfig::default(),
            ),
            ..PeppyConfig::default()
        };

        let builder =
            ServeCommandBuilder::new("/unused", "regression-git-hash", PeppyDirs::new("/unused"))
                .expect("create builder")
                .with_peppy_config(peppy_config)
                .with_messaging_router("zenoh".to_string())
                .expect("build managed messaging adapter without starting it");

        assert!(
            builder.federation_api_url.is_some(),
            "managed mode must arm router federation"
        );
    }
}
