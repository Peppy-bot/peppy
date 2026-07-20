//! The `peppy platform` command group. Interactive login and normal logout
//! negotiate the daemon's versioned identity-control protocol; only explicit
//! offline logout may mutate certificate or credential state without it.

pub mod federations;
pub mod login;
pub mod logout;
pub mod whoami;

use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Subcommand;
use core_node_api::encoding::StackListRequest;
use core_node_api::{NodeStage, SerializedNodeGraph};
use daemon::control::{
    self as daemon_control, ApplyResult, CleanupState, ControlClientError, ControlErrorCode,
    FederationStatus, LinkState, LogoutResult, PlatformLink, RouterApplyState,
};
use daemon::state::DaemonState;
#[cfg(test)]
use daemon::state::RouterOwnership;
use daemon_config::consts::PeppyDirs;
use peppylib::core_node::transport::poll;

use super::Command;
use crate::commands::CALLER_INSTANCE_ID;
use crate::error::Error;
use crate::{context::AppContext, error::Result};

/// Display name of the platform hub in federation reports.
pub(crate) const PLATFORM_HUB_NAME: &str = "platform-backend";

/// A pinned managed-router configuration remains an operator choice even while
/// the daemon owns credentials and certificate lifecycle.
pub(crate) const PINNED_NOTE: &str = "Note: this daemon's router uses an operator-pinned ZENOH_CONFIG; \
     the daemon updated platform identity, but federation routing remains operator-managed.";

/// External-router mode still uses the daemon for identity lifecycle. Only the
/// upstream router topology remains outside Peppy's control.
pub(crate) const EXTERNAL_ROUTER_NOTE: &str = "Platform identity was updated by the daemon. \
     This daemon uses `zenoh.external`, so upstream router configuration remains operator-managed.";

pub(crate) const EXTERNAL_ROUTER_LOGOUT_NOTE: &str = "Peppy-owned platform identity was cleared by the daemon. This daemon uses `zenoh.external`; \
     the operator must separately remove any identity installed in the external router.";

pub(crate) const EXTERNAL_ROUTER_OFFLINE_LOGOUT_NOTE: &str = "Peppy-owned local identity was cleared offline. This configuration uses `zenoh.external`; \
     the operator must separately remove any identity installed in the external router.";

pub(crate) const PINNED_ROUTER_LOGOUT_NOTE: &str = "Peppy-owned platform identity was cleared by the daemon. This daemon uses an \
     operator-pinned ZENOH_CONFIG; the operator must separately remove any identity installed \
     there.";

pub(crate) const PINNED_ROUTER_OFFLINE_LOGOUT_NOTE: &str = "Peppy-owned local identity was cleared offline. The last daemon generation used an \
     operator-pinned ZENOH_CONFIG; the operator must separately remove any identity installed \
     there.";

/// The handshake is intentionally short and always happens before login starts
/// OAuth/PAT validation or writes credentials.
pub(crate) const CONTROL_HELLO_TIMEOUT: Duration = Duration::from_secs(2);

/// Whether a namespace-changing operation follows a login or logout. This is
/// used only for the existing restart confirmation wording.
pub(crate) enum FederationPokeAction {
    Login,
    Logout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IdentityControlSettings {
    pub(crate) timeout_secs: u64,
    pub(crate) shutdown_grace_secs: u64,
}

/// The running generation is authoritative about both control timeout and
/// router ownership. Disk configuration is only a daemon-down fallback.
pub(crate) fn identity_control_settings(
    dirs: &PeppyDirs,
    config: &daemon_config::peppy_config::PeppyConfig,
) -> IdentityControlSettings {
    match DaemonState::read_from(&DaemonState::state_file_in(dirs.root())) {
        Ok(state) if state.is_running() => IdentityControlSettings {
            timeout_secs: state
                .federation_connect_timeout_secs
                .unwrap_or(daemon_config::peppy_config::DEFAULT_FEDERATION_CONNECT_TIMEOUT_SECS),
            shutdown_grace_secs: state.shutdown_grace_secs,
        },
        _ => IdentityControlSettings {
            timeout_secs: config
                .zenoh
                .federation()
                .map(|federation| federation.connect_timeout_secs)
                .unwrap_or(daemon_config::peppy_config::DEFAULT_FEDERATION_CONNECT_TIMEOUT_SECS),
            shutdown_grace_secs: config.lifecycle.shutdown_grace_secs,
        },
    }
}

/// Total client deadline for an identity-changing request. External mode uses
/// the daemon's standard federation timeout even though it does not rewrite an
/// operator router.
pub(crate) fn identity_control_timeout(timeout_secs: u64) -> Duration {
    Duration::from_secs(timeout_secs).saturating_mul(2) + daemon_control::POKE_READ_SLACK
}

/// Logout may consume DELETE + OAuth discovery + refresh + DELETE retry +
/// logout POST before router teardown and durable local cleanup.
pub(crate) fn identity_logout_timeout(timeout_secs: u64) -> Duration {
    Duration::from_secs(timeout_secs).saturating_mul(5) + daemon_control::POKE_READ_SLACK
}

/// Completion of a namespace-changing login starts only after the old daemon
/// generation has flushed its acknowledgement and gracefully joined all
/// handlers. Include that configured teardown ceiling before budgeting the
/// replacement generation's resolve/apply/probe work.
pub(crate) fn identity_restart_timeout(settings: IdentityControlSettings) -> Duration {
    daemon::restart_handoff_budget(settings.shutdown_grace_secs)
        + identity_control_timeout(settings.timeout_secs)
}

/// Performs the mandatory protocol-v1 handshake.
pub(crate) fn require_daemon_hello(dirs: &PeppyDirs, offline_recovery: bool) -> Result<()> {
    let socket = daemon_control::federation_control_socket_path(dirs);
    daemon_control::hello(&socket, CONTROL_HELLO_TIMEOUT)
        .map_err(|error| control_error("complete the protocol handshake", error, offline_recovery))
}

/// Begins a login as a daemon-owned, durable fail-closed transition. The
/// controller arms binding-incomplete before changing the managed router, so a
/// later CLI error or crash cannot leave the previous identity eligible for
/// reuse after the new credential is published.
pub(crate) fn prepare_login_transition(
    dirs: &PeppyDirs,
    timeout: Duration,
    expected_session_revision: uuid::Uuid,
) -> Result<()> {
    let socket = daemon_control::federation_control_socket_path(dirs);
    let result =
        daemon_control::prepare_oauth_login(&socket, timeout, expected_session_revision)
            .map_err(|error| control_error("prepare a fail-closed platform login", error, false))?;
    match result {
        ApplyResult::Applied(PlatformLink {
            endpoint: None,
            link_state: LinkState::NotConfigured,
        }) => Ok(()),
        ApplyResult::OperatorManaged => {
            println!(
                "The daemon prepared Peppy-owned identity state; router identity remains \
                 operator-managed."
            );
            Ok(())
        }
        ApplyResult::Applied(link) => Err(Error::Auth(format!(
            "the daemon could not establish a fail-closed login transition: it still reports \
             endpoint {:?} in state {:?}",
            link.endpoint, link.link_state
        ))),
        ApplyResult::Restarting { target_namespace } => Err(Error::Auth(format!(
            "the daemon unexpectedly requested a restart into `{target_namespace}` before the \
             new platform credential was published; retry login after it settles"
        ))),
    }
}

/// Turns typed control failures into actionable command errors while retaining
/// daemon-provided public diagnostics (including daemon-PAT configuration and
/// stale-session errors).
pub(crate) fn control_error(
    operation: &str,
    error: ControlClientError,
    offline_recovery: bool,
) -> Error {
    let offline = if offline_recovery {
        " Stop the daemon completely, then use `peppy platform logout --offline` for local recovery."
    } else {
        " Start or restart `peppy service serve`, then retry."
    };
    match error {
        ControlClientError::DaemonNotRunning => Error::Auth(format!(
            "cannot {operation}: no compatible running Peppy daemon was found.{offline}"
        )),
        ControlClientError::TimedOut => Error::Auth(format!(
            "cannot {operation}: the daemon control request timed out.{offline}"
        )),
        ControlClientError::Transport(message) => Error::Auth(format!(
            "cannot {operation}: daemon control is unavailable ({message}).{offline}"
        )),
        ControlClientError::ProtocolVersion { expected, actual } => Error::Auth(format!(
            "cannot {operation}: daemon protocol v{actual} is incompatible with CLI protocol \
             v{expected}. Restart the daemon with this Peppy version, then retry."
        )),
        ControlClientError::Daemon { code, message } => {
            let recovery = if offline_recovery
                && matches!(
                    code,
                    ControlErrorCode::Unavailable | ControlErrorCode::DeadlineExceeded
                ) {
                offline
            } else {
                ""
            };
            Error::Auth(format!(
                "the daemon could not {operation} ({code:?}): {message}{recovery}"
            ))
        }
        ControlClientError::UnexpectedResponse { expected, actual } => Error::Auth(format!(
            "cannot {operation}: the daemon returned `{actual}` where `{expected}` was required"
        )),
    }
}

/// Completes a strict login across the one unavoidable namespace-generation
/// handoff. A `restarting` acknowledgement proves only that the staged rotation
/// is durable; success still requires the replacement daemon to publish the
/// target namespace and a committed certificate/router disposition.
pub(crate) fn complete_login(
    dirs: &PeppyDirs,
    socket: &std::path::Path,
    timeout: Duration,
    result: ApplyResult,
    external: bool,
    expected_authentication: daemon_control::AuthenticationState,
    expected_session_revision: Option<uuid::Uuid>,
) -> Result<()> {
    let result = match result {
        ApplyResult::Restarting { target_namespace } => {
            let expected_generation = capture_login_generation(dirs, expected_session_revision)?;
            println!(
                "The daemon is restarting under namespace `{target_namespace}`; waiting for \
                 identity verification."
            );
            wait_for_restarted_login(
                dirs,
                socket,
                timeout,
                &target_namespace,
                expected_authentication,
                expected_session_revision,
                &expected_generation,
            )?
        }
        settled => settled,
    };
    report_login(result, external)
}

fn wait_for_restarted_login(
    dirs: &PeppyDirs,
    socket: &std::path::Path,
    timeout: Duration,
    target_namespace: &str,
    expected_authentication: daemon_control::AuthenticationState,
    expected_session_revision: Option<uuid::Uuid>,
    expected_generation: &str,
) -> Result<ApplyResult> {
    const POLL_INTERVAL: Duration = Duration::from_millis(100);
    const STATUS_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

    let deadline = Instant::now() + timeout;
    let state_path = DaemonState::state_file_in(dirs.root());
    let mut last_status = None;
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let generation_ready = DaemonState::read_from(&state_path)
            .is_ok_and(|state| state.is_running() && state.namespace.as_str() == target_namespace);
        if generation_ready {
            ensure_login_binding_current(dirs, expected_session_revision, expected_generation)?;
            let probe_timeout = STATUS_PROBE_TIMEOUT.min(deadline.saturating_duration_since(now));
            match daemon_control::status(socket, probe_timeout) {
                Ok(status) => {
                    if let Some(result) = completed_login_from_status(
                        &status,
                        expected_authentication,
                        expected_generation,
                    ) {
                        // Fence once more after the status response so a
                        // concurrent fresh login cannot satisfy this caller
                        // with another session's ready daemon state.
                        ensure_login_binding_current(
                            dirs,
                            expected_session_revision,
                            expected_generation,
                        )?;
                        return Ok(result);
                    }
                    last_status = Some(status);
                }
                Err(ControlClientError::ProtocolVersion { expected, actual }) => {
                    return Err(control_error(
                        "verify the restarted platform login",
                        ControlClientError::ProtocolVersion { expected, actual },
                        false,
                    ));
                }
                Err(_) => {}
            }
        }
        std::thread::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())));
    }

    let detail = last_status.map_or_else(
        || "the replacement daemon never published ready identity status".to_string(),
        |status| {
            format!(
                "last status was authentication={:?}, certificate={:?}, router={:?}, link={:?}",
                status.authentication,
                status.certificate,
                status.router_apply_state,
                status.link.link_state
            )
        },
    );
    Err(Error::Auth(format!(
        "logged in, but the daemon did not finish verified startup under namespace \
         `{target_namespace}` before the deadline: {detail}. The session was retained; inspect \
         `peppy platform federations` and retry login."
    )))
}

fn completed_login_from_status(
    status: &FederationStatus,
    expected_authentication: daemon_control::AuthenticationState,
    expected_generation: &str,
) -> Option<ApplyResult> {
    if !status.controller_settled
        || status.authentication != expected_authentication
        || status.offline_recovery_required
        || status.generation.as_deref() != Some(expected_generation)
        || !matches!(
            status.certificate,
            daemon_control::CertificateState::Valid | daemon_control::CertificateState::Expiring
        )
    {
        return None;
    }
    match status.router_apply_state {
        RouterApplyState::Applied if status.link.link_state == LinkState::Verified => {
            Some(ApplyResult::Applied(status.link.clone()))
        }
        RouterApplyState::OperatorManaged => Some(ApplyResult::OperatorManaged),
        _ => None,
    }
}

fn capture_login_generation(
    dirs: &PeppyDirs,
    expected_session_revision: Option<uuid::Uuid>,
) -> Result<String> {
    let credentials = auth::storage::load(&auth::storage::credentials_path(dirs))?;
    let identity = credentials.core_node_identity.as_ref().ok_or_else(|| {
        Error::Auth(
            "the daemon requested a restart without publishing an enrolled identity receipt".into(),
        )
    })?;
    let generation = identity.active_generation.clone();
    ensure_login_binding(&credentials, expected_session_revision, &generation)?;
    Ok(generation)
}

fn ensure_login_binding_current(
    dirs: &PeppyDirs,
    expected_session_revision: Option<uuid::Uuid>,
    expected_generation: &str,
) -> Result<()> {
    let credentials = auth::storage::load(&auth::storage::credentials_path(dirs))?;
    ensure_login_binding(&credentials, expected_session_revision, expected_generation)
}

fn ensure_login_binding(
    credentials: &auth::Credentials,
    expected_session_revision: Option<uuid::Uuid>,
    expected_generation: &str,
) -> Result<()> {
    let session_revision = credentials
        .session
        .as_ref()
        .map(|session| session.session_revision);
    let identity_matches = credentials
        .core_node_identity
        .as_ref()
        .is_some_and(|identity| {
            identity.session_revision == expected_session_revision
                && identity.active_generation == expected_generation
        });
    if session_revision == expected_session_revision && identity_matches {
        Ok(())
    } else {
        Err(Error::Auth(
            "this login was replaced while the daemon generation was restarting; retry platform login"
                .into(),
        ))
    }
}

/// Reports a successful daemon-owned login. Managed mode remains strict about
/// the verified platform link; external mode reports its operator boundary.
pub(crate) fn report_login(result: ApplyResult, external: bool) -> Result<()> {
    match result {
        ApplyResult::Applied(PlatformLink {
            link_state: LinkState::Verified,
            ..
        }) => {
            println!("Platform federation established.");
            Ok(())
        }
        ApplyResult::Applied(PlatformLink {
            link_state: LinkState::Error(reason),
            ..
        }) => Err(Error::Auth(format!(
            "logged in, but the daemon could not verify platform federation: {reason}"
        ))),
        ApplyResult::Applied(PlatformLink {
            link_state: LinkState::NotConfigured,
            ..
        }) => Err(Error::Auth(
            "logged in, but the daemon did not resolve a platform upstream".into(),
        )),
        ApplyResult::Applied(PlatformLink {
            link_state: LinkState::Unverified,
            ..
        }) => Err(Error::Auth(
            "logged in, but the daemon did not verify the platform link".into(),
        )),
        ApplyResult::OperatorManaged => {
            println!(
                "{}",
                if external {
                    EXTERNAL_ROUTER_NOTE
                } else {
                    PINNED_NOTE
                }
            );
            Ok(())
        }
        ApplyResult::Restarting { target_namespace } => Err(Error::Auth(format!(
            "the daemon requested a restart into `{target_namespace}`, but login completion \
                 did not wait for the replacement generation"
        ))),
    }
}

/// Reports the daemon's structured logout transaction without collapsing a
/// partial remote-revocation failure into silence. Local cleanup and managed
/// router de-federation remain strict because otherwise the CLI cannot claim a
/// safe logout.
pub(crate) fn report_logout(result: LogoutResult, external: bool) -> Result<()> {
    if result.certificate_revocation == CleanupState::Failed {
        println!(
            "Warning: server-side certificate revocation failed; the issued leaf remains bounded \
             by its expiry."
        );
    }
    if result.oauth_revocation == CleanupState::Failed {
        println!("Warning: OAuth token revocation failed.");
    }
    if result.local_cleanup == CleanupState::Failed {
        return Err(Error::Auth(
            "the daemon could not finish local credential/certificate cleanup. Check the daemon \
             logs; after stopping it completely, use `peppy platform logout --offline`."
                .into(),
        ));
    }

    match result.router_apply {
        RouterApplyState::Standalone => {
            if external {
                println!("{EXTERNAL_ROUTER_LOGOUT_NOTE}");
            } else if result.operator_action_required {
                println!("{PINNED_ROUTER_LOGOUT_NOTE}");
            } else {
                println!("Peppy-owned platform identity and managed federation were cleared.");
            }
        }
        RouterApplyState::OperatorManaged => println!(
            "{}",
            if external {
                EXTERNAL_ROUTER_LOGOUT_NOTE
            } else {
                PINNED_ROUTER_LOGOUT_NOTE
            }
        ),
        RouterApplyState::Error => {
            return Err(Error::Auth(
                "the daemon cleared local platform identity, but could not prove the managed \
                 router was de-federated. Check the daemon logs before treating logout as clean."
                    .into(),
            ));
        }
        RouterApplyState::Applied => {
            return Err(Error::Auth(
                "the daemon cleared local platform identity, but still reports an applied \
                 platform router identity; logout is not clean. Check the daemon logs."
                    .into(),
            ));
        }
    }

    if let Some(target_namespace) = result.target_namespace {
        println!(
            "The daemon cleared platform identity and is restarting under namespace \
             `{target_namespace}`."
        );
    }
    Ok(())
}

/// Confirms any login/logout that may change the session namespace, restart the
/// daemon, and wipe the running node stack. External-router ownership changes
/// who applies federation, not the namespace of Peppy's own sessions.
pub(crate) fn confirm_restart(
    ctx: &Arc<AppContext>,
    yes: bool,
    action: &FederationPokeAction,
) -> Result<bool> {
    use std::io::{IsTerminal, Write};

    if yes {
        return Ok(true);
    }
    let daemon_running = DaemonState::read().is_ok_and(|state| state.is_running());
    if !daemon_running || !std::io::stdin().is_terminal() || !daemon_has_user_nodes(ctx) {
        return Ok(true);
    }
    let verb = match action {
        FederationPokeAction::Login => "Logging in",
        FederationPokeAction::Logout => "Logging out",
    };
    eprintln!(
        "{verb} changes this machine's namespace, which restarts the messaging daemon and wipes \
         the running node stack."
    );
    eprint!("Continue? [y/N] ");
    std::io::stderr().flush().ok();
    super::confirm::read_yes_no(None)
}

fn daemon_has_user_nodes(ctx: &Arc<AppContext>) -> bool {
    const STACK_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
    let probe = async {
        let conn = ctx.connect_to_daemon().await?;
        let response = poll(
            &StackListRequest::new(),
            conn.messenger,
            &conn.core_node_name,
            CALLER_INSTANCE_ID,
            &conn.core_node_name,
            STACK_PROBE_TIMEOUT,
        )
        .await?;
        let graph = crate::commands::parse_stack_graph(&response.graph_json)?;
        Ok::<bool, Error>(stack_has_user_nodes(&graph))
    };

    crate::commands::block_on(async move {
        Ok(
            match tokio::time::timeout(STACK_PROBE_TIMEOUT, probe).await {
                Ok(Ok(has_user_nodes)) => has_user_nodes,
                Ok(Err(_)) | Err(_) => true,
            },
        )
    })
    .unwrap_or(true)
}

fn stack_has_user_nodes(graph: &SerializedNodeGraph) -> bool {
    graph
        .nodes
        .iter()
        .any(|node| node.stage != Some(NodeStage::Root))
}

#[derive(Subcommand)]
pub enum PlatformCommands {
    /// Log in through the running daemon (browser OAuth, or daemon PEPPY_API_KEY)
    Login {
        #[arg(long = "api-url")]
        api_url: Option<String>,
        #[arg(long = "no-browser")]
        no_browser: bool,
        #[arg(long = "yes", short = 'y')]
        yes: bool,
    },
    /// Ask the daemon to log out, or recover locally after it is fully stopped
    Logout {
        #[arg(long = "api-url")]
        api_url: Option<String>,
        #[arg(long = "yes", short = 'y')]
        yes: bool,
        /// Perform local recovery only after proving the daemon is stopped.
        #[arg(long)]
        offline: bool,
    },
    /// Show the current platform identity, backend, and token status
    #[command(visible_alias = "status")]
    Whoami {
        #[arg(long = "api-url")]
        api_url: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Report platform federation state and reachable core nodes
    Federations {
        #[arg(long)]
        json: bool,
    },
}

pub struct PlatformCommand {
    pub command: PlatformCommands,
}

impl Command for PlatformCommand {
    fn execute(self, app_ctx: &Arc<AppContext>) -> Result<()> {
        let pat = auth::resolver::pat_from_env();
        match self.command {
            PlatformCommands::Login {
                api_url,
                no_browser,
                yes,
            } => login::LoginCommand {
                api_url,
                no_browser,
                yes,
                peppy_dirs: None,
                pat,
            }
            .execute(app_ctx),
            PlatformCommands::Logout {
                api_url,
                yes,
                offline,
            } => logout::LogoutCommand {
                api_url,
                yes,
                offline,
                peppy_dirs: None,
                pat,
            }
            .execute(app_ctx),
            PlatformCommands::Whoami { api_url, json } => whoami::WhoamiCommand {
                api_url,
                json,
                peppy_dirs: None,
                pat,
            }
            .execute(app_ctx),
            PlatformCommands::Federations { json } => federations::FederationsCommand {
                json,
                peppy_dirs: None,
                pat,
            }
            .execute(app_ctx),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use core_node_api::{
        InstanceState, NodeStage, SerializedInstance, SerializedNode, SerializedNodeGraph,
    };

    use super::*;

    fn graph_of(nodes: Vec<SerializedNode>) -> SerializedNodeGraph {
        SerializedNodeGraph {
            nodes,
            edges: Vec::new(),
        }
    }

    fn node_with_stage(name: &str, stage: NodeStage) -> SerializedNode {
        SerializedNode {
            name: name.into(),
            tag: "v1".into(),
            core_node: "test-core".into(),
            config_path: format!("/tmp/{name}.json5"),
            artifact_path: None,
            stage: Some(stage),
            instances: Vec::new(),
        }
    }

    #[test]
    fn stack_user_node_detection_preserves_restart_confirmation_semantics() {
        assert!(!stack_has_user_nodes(&graph_of(vec![node_with_stage(
            "core",
            NodeStage::Root,
        )])));
        let mut user = node_with_stage("sensor", NodeStage::Added);
        user.instances = vec![SerializedInstance {
            instance_id: "sensor-1".into(),
            state: InstanceState::Finished,
            healthy: false,
            slot_bindings: BTreeMap::new(),
            pairing_slots: BTreeMap::new(),
        }];
        assert!(stack_has_user_nodes(&graph_of(vec![
            node_with_stage("core", NodeStage::Root),
            user,
        ])));
    }

    #[test]
    fn managed_login_requires_a_verified_structured_result() {
        assert!(
            report_login(
                ApplyResult::Applied(PlatformLink {
                    endpoint: Some("tls/hub:7447".into()),
                    link_state: LinkState::Verified,
                }),
                false,
            )
            .is_ok()
        );
        assert!(
            report_login(
                ApplyResult::Applied(PlatformLink {
                    endpoint: None,
                    link_state: LinkState::NotConfigured,
                }),
                false,
            )
            .is_err()
        );
    }

    #[test]
    fn external_login_accepts_daemon_identity_ownership_without_managing_router() {
        assert!(report_login(ApplyResult::OperatorManaged, true).is_ok());
    }

    #[test]
    fn restarted_login_requires_committed_identity_and_router_readiness() {
        let mut status = FederationStatus {
            controller_settled: true,
            authentication: daemon_control::AuthenticationState::Oauth,
            certificate: daemon_control::CertificateState::Valid,
            generation: Some("generation-1".into()),
            router_apply_state: RouterApplyState::Applied,
            link: PlatformLink {
                endpoint: Some("tls/hub:7447".into()),
                link_state: LinkState::Verified,
            },
            ..FederationStatus::default()
        };
        assert!(matches!(
            completed_login_from_status(
                &status,
                daemon_control::AuthenticationState::Oauth,
                "generation-1",
            ),
            Some(ApplyResult::Applied(_))
        ));

        status.certificate = daemon_control::CertificateState::Renewing;
        assert!(
            completed_login_from_status(
                &status,
                daemon_control::AuthenticationState::Oauth,
                "generation-1",
            )
            .is_none(),
            "a durable restart handoff is not yet a committed login"
        );

        status.certificate = daemon_control::CertificateState::Valid;
        status.router_apply_state = RouterApplyState::OperatorManaged;
        status.link = PlatformLink::default();
        assert_eq!(
            completed_login_from_status(
                &status,
                daemon_control::AuthenticationState::Oauth,
                "generation-1",
            ),
            Some(ApplyResult::OperatorManaged)
        );
        assert!(
            completed_login_from_status(
                &status,
                daemon_control::AuthenticationState::Pat,
                "generation-1",
            )
            .is_none(),
            "readiness from a different authentication class must not satisfy login"
        );
    }

    #[test]
    fn daemon_unavailable_logout_guidance_names_offline_recovery() {
        let error = control_error("log out", ControlClientError::DaemonNotRunning, true);
        assert!(error.to_string().contains("logout --offline"));
        let error = control_error(
            "log out",
            ControlClientError::Daemon {
                code: ControlErrorCode::Unavailable,
                message: "identity controller unavailable".into(),
            },
            true,
        );
        assert!(error.to_string().contains("logout --offline"));
    }

    #[test]
    fn live_router_ownership_is_not_inferred_from_control_timeout_presence() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = PeppyDirs::new(temp.path());
        let state = DaemonState::new(
            "core-test",
            "127.0.0.1",
            config::consts::DEFAULT_MESSAGING_PORT,
            "test",
            5,
            config::namespace::Namespace::local(),
            RouterOwnership::OperatorManaged,
            Some(17),
        );
        DaemonState::write_to(&DaemonState::state_file_in(temp.path()), &state).unwrap();

        let settings =
            identity_control_settings(&dirs, &daemon_config::peppy_config::PeppyConfig::default());
        assert_eq!(settings.timeout_secs, 17);
    }

    #[test]
    fn restart_wait_covers_old_generation_teardown_and_new_router_startup() {
        let settings = IdentityControlSettings {
            timeout_secs: 17,
            shutdown_grace_secs: 11,
        };
        let old_generation_teardown =
            core_node::force_kill_deadline(Duration::from_secs(settings.shutdown_grace_secs))
                + core_node::TEARDOWN_REAP_BUDGET
                + Duration::from_secs(1);

        assert_eq!(
            daemon::restart_handoff_budget(settings.shutdown_grace_secs),
            old_generation_teardown + Duration::from_secs(2 + 5 + 30),
            "the handoff includes graceful join, child reap, port release, and managed-router startup"
        );
        assert_eq!(
            identity_restart_timeout(settings),
            daemon::restart_handoff_budget(settings.shutdown_grace_secs)
                + identity_control_timeout(settings.timeout_secs)
        );
    }

    #[test]
    fn logout_reporting_rejects_unsafe_local_or_managed_router_outcomes() {
        let outcome = |router_apply, local_cleanup| LogoutResult {
            certificate_revocation: CleanupState::Failed,
            oauth_revocation: CleanupState::Failed,
            router_apply,
            local_cleanup,
            operator_action_required: false,
            target_namespace: None,
        };

        assert!(
            report_logout(
                outcome(RouterApplyState::Standalone, CleanupState::Succeeded),
                false,
            )
            .is_ok()
        );
        assert!(
            report_logout(
                outcome(RouterApplyState::Error, CleanupState::Succeeded),
                false,
            )
            .is_err()
        );
        assert!(
            report_logout(
                outcome(RouterApplyState::Standalone, CleanupState::Failed),
                false,
            )
            .is_err()
        );
        assert!(
            report_logout(
                outcome(RouterApplyState::OperatorManaged, CleanupState::Succeeded,),
                true,
            )
            .is_ok()
        );
    }
}
