use super::certificate_renewal::CertificateRenewal;
use super::core_node::CoreNodeRunner;
use super::federation_control::FederationControl;
use super::messaging_router::{MessagingRouter, teardown_budget_for};
use super::router_federation::RouterFederation;
use super::serve::{CompositeCommand, Serve};
use crate::error::{Error, Result};
use crate::state::DaemonState;
use auth::{Enrollment, FederationIdentity};
use config::namespace::Namespace;
use daemon_config::consts::PeppyDirs;
use daemon_config::peppy_config::PeppyConfig;
use pmi::Messenger;
use pmi::MessengerAdapter;
use pmi::MockAdapter;
use pmi::RouterId;
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
    /// This generation's identity, resolved once in
    /// [`with_messaging_router`](Self::with_messaging_router) from the
    /// enrollment on disk: the routing namespace (`local` when not enrolled,
    /// else the project id) that the daemon's own session, [`DaemonState`] and
    /// the core node (and thus every spawned node) open under, and the router
    /// id the enrollment pins. The federation task compares pokes against it,
    /// and an enrolled generation (one with a pinned id) arms the task that
    /// renews the peer certificate, with a managed router and with an
    /// operator-run one: the certificate of the peer expires in both.
    identity: FederationIdentity,
    /// The managed zenoh router of this generation, set by
    /// [`with_messaging_router`](Self::with_messaging_router). `None` for an
    /// external router (the operator owns its identity and its federation) and
    /// for the non-zenoh engines, which arm no federation task and bind no
    /// control socket.
    managed_router: Option<ManagedRouter>,
    /// The shared coordinator token for this generation: cloned into every serve
    /// task (so a restart/stop unparks them for graceful teardown) and handed to
    /// [`Serve`] (which cancels it on its way out). Created per generation.
    teardown_token: CancellationToken,
}

/// What [`ServeCommandBuilder::build`] needs to know about a managed zenoh
/// router.
struct ManagedRouter {
    /// The router's transport identity: the enrollment's zenoh id when
    /// enrolled, a fresh per-boot id otherwise. Recorded in [`DaemonState`] so
    /// the CLI can tell the generation it poked from the one that replaced it.
    router_id: RouterId,
    /// Whether the router runs an operator-pinned `ZENOH_CONFIG`, so its
    /// federation is the operator's and a poke verifies nothing.
    pinned: bool,
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
            shutdown_token: None,
            core_node_done_tx: None,
            root_dir: root_dir.into(),
            git_hash: git_hash.into(),
            peppy_dirs,
            peppy_config: PeppyConfig::default(),
            // The identity of the mock/other engines, which read no enrollment;
            // the zenoh path overwrites this in `with_messaging_router`.
            identity: FederationIdentity::of(None),
            managed_router: None,
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

                // This generation's identity comes from the enrollment on disk:
                // not enrolled means a standalone router under `local`; enrolled
                // means the platform-minted router id, the project namespace,
                // and a mutual-TLS link to the project's cloud router, all fixed
                // for the life of the generation. A malformed enrollment fails
                // startup loudly rather than silently booting standalone.
                let enrollment = auth::enrollment::load(&self.peppy_dirs).map_err(|error| {
                    Error::ExecutionFailed(format!(
                        "could not read this machine's platform enrollment: {error}"
                    ))
                })?;
                self.identity = FederationIdentity::of(enrollment.as_ref());

                let gossip = self.peppy_config.zenoh.gossip();
                let external_endpoint = self
                    .peppy_config
                    .zenoh
                    .external_endpoint()
                    .map(str::to_string);
                let adapter = match external_endpoint {
                    // An operator-run router keeps its own identity and its own
                    // federation; only the session namespace follows the
                    // enrollment here.
                    Some(endpoint) => {
                        ZenohAdapter::with_external_router(&endpoint, gossip, subscriber_buffers)?
                    }
                    None => self.managed_router_adapter(
                        enrollment.as_ref(),
                        listening_port,
                        gossip,
                        subscriber_buffers,
                    )?,
                }
                .with_session_reconnect()
                .with_namespace(Some(self.identity.namespace.clone()));
                MessengerAdapter::Zenoh(adapter)
            }
            "mock" => MessengerAdapter::Mock(MockAdapter::default()),
            other => {
                warn!(target: "daemon::serve", "Unsupported messaging engine '{}', using mock", other);
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
                    self.teardown_token.clone(),
                )));
        Ok(self)
    }

    /// Builds the managed zenoh router: standalone under a per-boot id when
    /// not enrolled; under the platform-minted id, dialing the project's cloud
    /// router over mutual TLS, when enrolled. Records the router for
    /// [`Self::build`] to arm the federation task and the control socket.
    fn managed_router_adapter(
        &mut self,
        enrollment: Option<&Enrollment>,
        listening_port: u16,
        gossip: bool,
        subscriber_buffers: SubscriberBufferSizes,
    ) -> Result<ZenohAdapter> {
        let (router_id, connect_endpoints, tls) = match enrollment {
            Some(enrollment) => {
                if enrollment.document.is_expired(auth::storage::now_unix()) {
                    warn!(
                        "the platform peer certificate has expired; the cloud router refuses \
                         this daemon's link until the daemon renews the certificate, which \
                         needs a session (`peppy platform login`), or until `peppy platform \
                         enroll --replace` mints a new one"
                    );
                }
                let (locator, tls) = enrollment.federation_target();
                (
                    enrollment.document.zenoh_id.clone(),
                    vec![locator],
                    Some(tls),
                )
            }
            None => (RouterId::generate(), Vec::new(), None),
        };
        let adapter = ZenohAdapter::with_router(
            ZenohNetProtocol::Tcp,
            "0.0.0.0",
            listening_port,
            gossip,
            subscriber_buffers,
            connect_endpoints,
            tls,
            router_id.clone(),
        )?;
        self.managed_router = Some(ManagedRouter {
            router_id,
            pinned: adapter.router_config_is_pinned(),
        });
        Ok(adapter)
    }

    pub fn with_core_node(mut self, core_node_name: Option<String>) -> Result<Self> {
        self.core_node_requested = true;
        self.core_node_name = core_node_name;
        Ok(self)
    }

    pub fn build(mut self) -> Result<Serve> {
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
                    self.peppy_config,
                    self.identity.namespace.clone(),
                    name_claim_settle,
                    self.teardown_token.clone(),
                    core_node_done_tx,
                );

                // Write the daemon state file with the core node name. The
                // namespace and router identity are recorded here, before the
                // control socket binds (below), so a CLI control session that
                // reads it never sees a half-set generation.
                let core_node_name = core_node.node_name().to_string();
                let daemon_state = {
                    let messenger = messenger.blocking_lock();
                    daemon_state_for_messenger(
                        &messenger,
                        &core_node_name,
                        &self.git_hash,
                        shutdown_grace_secs,
                        self.identity.namespace.clone(),
                        self.managed_router
                            .as_ref()
                            .map(|router| router.router_id.clone()),
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

                self.composite_command = self
                    .composite_command
                    .add_async_command(Box::new(core_node));
            } else {
                warn!("Commands listener requires a messaging router");
                return Err(Error::MissingMessagingRouter);
            }
        }

        // The federation task and its control socket (managed zenoh only). The
        // router already boots federated or standalone from the enrollment; the
        // task answers `peppy platform enroll`/`unenroll` pokes by restarting the
        // generation when the enrollment changed its identity and by verifying
        // the link when it did not. External zenoh and the mock engine have no
        // control channel and never restart through this path.
        let mut restart_rx: Option<watch::Receiver<bool>> = None;
        if let Some(router) = self.managed_router.take() {
            // Poke channel: the control socket reaches the federation task
            // through it. Bounded + tiny: pokes are rare and serviced one at a time.
            let (trigger_tx, trigger_rx) = tokio::sync::mpsc::channel(8);
            // Restart signal: the control handler raises it after flushing the
            // `Restarting` ack; the serve coordinator observes it.
            let (restart_tx, restart_signal_rx) = watch::channel(false);
            restart_rx = Some(restart_signal_rx);
            self.composite_command =
                self.composite_command
                    .add_async_command(Box::new(RouterFederation::new(
                        // The data root the task re-reads the enrollment from.
                        self.peppy_dirs.clone(),
                        trigger_rx,
                        self.identity.clone(),
                        router.pinned,
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
                        restart_tx,
                        self.teardown_token.clone(),
                    )));
        }

        if self.identity.router_id.is_some() {
            self.composite_command =
                self.composite_command
                    .add_async_command(Box::new(CertificateRenewal::new(
                        self.peppy_dirs.clone(),
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
    namespace: Namespace,
    router_id: Option<RouterId>,
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
        router_id,
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
    if let Err(reason) = config::runtime::CoreNodeName::new(name.as_str()) {
        return Err(Error::ExecutionFailed(format!(
            "invalid core node name {name:?} (from {source}): {reason}"
        )));
    }
    Ok(Some(name))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let max = "n".repeat(config::runtime::MAX_CORE_NODE_NAME_LEN);
        assert_eq!(
            resolve_core_node_name(some(&max), None)
                .expect("boundary length accepted")
                .as_deref(),
            Some(max.as_str())
        );

        let over = "n".repeat(config::runtime::MAX_CORE_NODE_NAME_LEN + 1);
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
            builder.managed_router.is_none(),
            "external mode must not arm router federation"
        );
        let messenger = builder
            .messenger_handle()
            .expect("builder retains its messenger");
        let messenger = messenger.blocking_lock();

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
            !messenger.router_config_is_pinned(),
            "an external router is the operator's, never a pinned managed config"
        );

        let state = daemon_state_for_messenger(
            &messenger,
            "regression-core",
            "regression-git-hash",
            42,
            Namespace::local(),
            builder
                .managed_router
                .as_ref()
                .map(|router| router.router_id.clone()),
        );
        assert_eq!(state.messaging_host, "zenoh-router.regression.test");
        assert_eq!(state.messaging_port, 17555);
        assert_eq!(
            state.router_id, None,
            "an external router's identity is the operator's, not recorded"
        );
        assert!(
            !state.has_federation_control(),
            "external mode must record no federation control channel in the daemon state"
        );
    }

    fn managed_builder(data_root: &std::path::Path) -> ServeCommandBuilder {
        let peppy_config = PeppyConfig {
            zenoh: daemon_config::peppy_config::ZenohConfig::Managed(
                daemon_config::peppy_config::ManagedZenohConfig::default(),
            ),
            ..PeppyConfig::default()
        };
        ServeCommandBuilder::new("/unused", "regression-git-hash", PeppyDirs::new(data_root))
            .expect("create builder")
            .with_peppy_config(peppy_config)
            .with_messaging_router("zenoh".to_string())
            .expect("build managed messaging adapter without starting it")
    }

    /// Not enrolled: a standalone router under `local`, with a per-boot
    /// identity, and the federation task armed against exactly that.
    #[test]
    fn an_unenrolled_managed_root_boots_a_standalone_router_under_local() {
        let data_root = tempfile::tempdir().expect("temp data root");
        let builder = managed_builder(data_root.path());

        let router = builder
            .managed_router
            .as_ref()
            .expect("managed mode must arm router federation");
        assert!(!router.pinned);
        assert_eq!(
            builder.identity,
            FederationIdentity::of(None),
            "a machine that is not enrolled runs under `local` and renews no certificate"
        );
        let messenger = builder.messenger_handle().expect("messenger");
        assert!(
            messenger.blocking_lock().router_links_probe().is_none(),
            "a standalone router dials no upstream"
        );
    }

    use auth::test_support::{PROJECT, ZID};

    /// Enrolls the machine under `data_root` in [`PROJECT`] as [`ZID`], with
    /// a certificate that does not expire.
    fn write_enrollment(data_root: &std::path::Path) {
        auth::test_support::write_enrollment(
            &PeppyDirs::new(data_root),
            auth::enrollment::EnrollmentDocument {
                certificate_expires_at: i64::MAX,
                ..auth::test_support::enrollment_document()
            },
        );
    }

    /// Enrolled: the router boots under the enrollment's id and namespace,
    /// dialing the project's cloud router, the federation task is armed
    /// against that identity, and the certificate renewal is armed.
    #[test]
    fn an_enrolled_root_boots_the_router_federated_under_the_enrollment_identity() {
        let data_root = tempfile::tempdir().expect("temp data root");
        write_enrollment(data_root.path());

        let builder = managed_builder(data_root.path());

        assert_eq!(
            builder.identity,
            FederationIdentity {
                namespace: Namespace::parse(PROJECT).unwrap(),
                router_id: Some(RouterId::parse(ZID).unwrap()),
            }
        );
        let router = builder.managed_router.as_ref().expect("armed");
        assert_eq!(router.router_id, RouterId::parse(ZID).unwrap());
        let messenger = builder.messenger_handle().expect("messenger");
        let probe = messenger
            .blocking_lock()
            .router_links_probe()
            .expect("an enrolled router dials the cloud router");
        assert_eq!(probe.endpoints(), ["tls/rtr-p.example:7447"]);
    }

    /// An operator-run router: the federation is the operator's, and the peer
    /// certificate of the enrolled machine is renewed as with a managed router.
    #[test]
    fn an_enrolled_root_with_an_external_router_renews_its_certificate() {
        let data_root = tempfile::tempdir().expect("temp data root");
        write_enrollment(data_root.path());
        let peppy_config = PeppyConfig {
            zenoh: daemon_config::peppy_config::ZenohConfig::External(
                daemon_config::peppy_config::ExternalZenohConfig {
                    endpoint: "tcp/zenoh-router.regression.test:17555".to_string(),
                },
            ),
            ..PeppyConfig::default()
        };

        let builder = ServeCommandBuilder::new(
            "/unused",
            "regression-git-hash",
            PeppyDirs::new(data_root.path()),
        )
        .expect("create builder")
        .with_peppy_config(peppy_config)
        .with_messaging_router("zenoh".to_string())
        .expect("build external messaging adapter without starting it");

        assert!(builder.managed_router.is_none());
        assert_eq!(
            builder.identity,
            FederationIdentity {
                namespace: Namespace::parse(PROJECT).unwrap(),
                router_id: Some(RouterId::parse(ZID).unwrap()),
            },
            "an enrolled machine renews its certificate under an operator-run router too"
        );
    }

    /// A present but broken enrollment fails startup rather than booting a
    /// standalone router that would silently leave the project.
    #[test]
    fn a_broken_enrollment_fails_the_build() {
        let data_root = tempfile::tempdir().expect("temp data root");
        let dirs = PeppyDirs::new(data_root.path());
        std::fs::create_dir_all(dirs.peer_dir()).unwrap();
        std::fs::write(dirs.peer_dir().join("enrollment.json5"), "{ version: 2 }").unwrap();
        let peppy_config = PeppyConfig {
            zenoh: daemon_config::peppy_config::ZenohConfig::Managed(
                daemon_config::peppy_config::ManagedZenohConfig::default(),
            ),
            ..PeppyConfig::default()
        };

        let Err(err) = ServeCommandBuilder::new("/unused", "hash", dirs)
            .expect("create builder")
            .with_peppy_config(peppy_config)
            .with_messaging_router("zenoh".to_string())
        else {
            panic!("a malformed enrollment must fail startup");
        };
        assert!(
            err.to_string().contains("platform enrollment"),
            "the error names the enrollment: {err}"
        );
    }
}
