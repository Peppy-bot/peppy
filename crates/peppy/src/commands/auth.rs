//! The `peppy auth` command group: `login`, `logout`, and `whoami`. Each
//! variant maps to a handler in this module's directory; the OAuth device flow,
//! token storage, and credential resolution they share live in the separate
//! `auth` engine crate.

pub mod login;
pub mod logout;
pub mod whoami;

use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Subcommand;
use core_node_api::encoding::StackListRequest;
use core_node_api::{NodeStage, SerializedNodeGraph};
use daemon_config::consts::PeppyDirs;
use peppylib::core_node::transport::poll;

use super::Command;
use crate::commands::CALLER_INSTANCE_ID;
use crate::error::Error;
use crate::{context::AppContext, error::Result};
use daemon::control::{self as daemon_control, PokeOutcome};
use daemon::state::DaemonState;

/// Shown when the managed router uses an operator-pinned config, so the daemon
/// cannot rewrite it. Used by both login and logout reporting.
pub(crate) const PINNED_NOTE: &str = "Note: this daemon's router uses an operator-pinned ZENOH_CONFIG; \
     federation is not auto-managed.";

/// Shown after login/logout in external mode. No federation control task exists
/// in that mode, so the CLI deliberately leaves the operator's router alone.
pub(crate) const EXTERNAL_ROUTER_NOTE: &str = "Note: this daemon dials an operator-run router \
     (`zenoh.external`); federation belongs to the operator and was left untouched. Restart the \
     daemon to apply the new sign-in state to its sessions.";

/// Re-poke cadence and overall deadline while waiting for the daemon to restart
/// under the new namespace. The deadline covers zenohd's readiness ceiling (30s)
/// plus the federation connect timeout and slack.
const RESTART_POLL_INTERVAL: Duration = Duration::from_millis(250);
const RESTART_POLL_DEADLINE: Duration = Duration::from_secs(60);

/// Upper bound on the pre-prompt probe that asks the running daemon whether its
/// node stack holds any user nodes. Kept short so a sluggish or half-up daemon
/// (pid alive but its messaging router not yet reachable) delays the
/// login/logout prompt only briefly before we fall back to showing the warning.
const STACK_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// The namespace the on-disk credentials under `dirs` resolve to (`local` when
/// logged out, else the org id). The same resolution the daemon does at startup,
/// so the CLI can confirm the daemon came back under exactly what it just wrote.
/// Reads the credentials from the same `dirs` the command resolved, so a test
/// seam isolates it.
fn current_creds_namespace(dirs: &PeppyDirs) -> String {
    config::org::resolve_session_namespace(
        auth::router::cached_organization_id(&auth::storage::credentials_path(dirs)).as_deref(),
    )
    .as_str()
    .to_string()
}

/// Whether a federation poke follows a login (federate) or a logout
/// (de-federate). Affects the user-facing wording and, crucially, whether a
/// federation failure is fatal: a login that cannot establish federation fails
/// (the credentials are kept), while a logout is always best-effort.
pub(crate) enum FederationPokeAction {
    Login,
    Logout,
}

/// The managed-federation connect timeout (seconds) a login/logout should honor:
/// `Some` means "managed mode: warn about the restart and poke the control
/// socket with this timeout", `None` means "external mode: leave federation to
/// the operator, never warn or poke".
///
/// A RUNNING daemon is authoritative: its state file records whether that
/// generation armed managed-router federation (and with which timeout), so a
/// config edited on disk after it started can neither make login/logout poke a
/// control socket that does not exist (external daemon, managed config on disk)
/// nor skip the poke a managed daemon needs to (de)federate immediately
/// (managed daemon, external config on disk). Only when no daemon is running,
/// so there is nothing to poke or restart either way, does the on-disk `config`
/// decide, matching what the next daemon start will do. The state file is read
/// from the same `dirs` the command resolved, so a test seam isolates it.
pub(crate) fn federation_poke_timeout_secs(
    dirs: &PeppyDirs,
    config: &daemon_config::peppy_config::PeppyConfig,
) -> Option<u64> {
    match DaemonState::read_from(&DaemonState::state_file_in(dirs.root())) {
        Ok(state) if state.is_running() => state.federation_connect_timeout_secs,
        _ => config.zenoh.federation().map(|f| f.connect_timeout_secs),
    }
}

/// Confirms (before authentication begins) a managed-router login/logout that
/// may restart the daemon and wipe the running node stack, unless `--yes` was
/// passed. Callers skip this entirely for `zenoh.external`, where authentication
/// never pokes or restarts the daemon. Returns `Ok(true)` to proceed. Only
/// prompts when a daemon is actually running (else there is nothing to restart),
/// stdin is a TTY (so a script is never blocked on a prompt), and the daemon is
/// running at least one user node (else the restart wipes nothing worth warning
/// about).
pub(crate) fn confirm_restart(
    ctx: &Arc<AppContext>,
    yes: bool,
    action: &FederationPokeAction,
) -> Result<bool> {
    use std::io::{IsTerminal, Write};

    if yes {
        return Ok(true);
    }
    // Nothing to restart if no daemon is running; and never block a non-interactive
    // invocation (a script / CI) on a prompt. A readable state file can outlive a
    // crashed daemon, so probe the recorded pid for *real* liveness rather than
    // treating state-file readability as "a daemon is up".
    let daemon_running = DaemonState::read().is_ok_and(|s| s.is_running());
    if !daemon_running || !std::io::stdin().is_terminal() {
        return Ok(true);
    }
    // The restart only wipes a node stack worth warning about when the daemon is
    // actually running user nodes. A stack that holds nothing but the synthetic
    // core-node root loses nothing on restart, so the warning would be noise.
    if !daemon_has_user_nodes(ctx) {
        return Ok(true);
    }
    let verb = match action {
        FederationPokeAction::Login => "Logging in",
        FederationPokeAction::Logout => "Logging out",
    };
    eprintln!(
        "{verb} changes this machine's organization namespace, which restarts the messaging \
         daemon and wipes the running node stack."
    );
    eprint!("Continue? [y/N] ");
    std::io::stderr().flush().ok();
    super::confirm::read_yes_no(None)
}

/// Whether the running daemon's node stack holds any user node, by querying its
/// live stack over the messaging session (the same query `peppy stack list`
/// uses). Drives the login/logout restart prompt: an empty stack means the
/// restart wipes nothing the user staged, so the warning is skipped.
///
/// Best effort: connecting to the daemon and reading its stack can fail or stall
/// (it is mid-restart, its messaging router is not up yet, the query times out).
/// Any such outcome returns `true` so the caller still shows the warning rather
/// than silently dropping it. The whole probe is bounded by
/// [`STACK_PROBE_TIMEOUT`] because opening the session can itself stall when the
/// router is unreachable, which the per-query timeout alone would not cover.
fn daemon_has_user_nodes(ctx: &Arc<AppContext>) -> bool {
    let probe = async {
        let conn = ctx.connect_to_daemon().await?;
        // Deliberately targets the *local* daemon (not `conn.target_core_node`):
        // this probe backs the "login/logout restarts the local daemon" warning,
        // so a global `--core-node` override must not redirect it.
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

/// Whether a serialized stack graph contains a user node, i.e. any node other
/// than the synthetic [`NodeStage::Root`] entity the daemon always carries for
/// itself. A node entity counts as present regardless of its instances' states,
/// since a node whose only instances have finished is still in the stack and
/// would be wiped by a restart. Pure over the graph so the decision is
/// unit-testable without a live daemon.
fn stack_has_user_nodes(graph: &SerializedNodeGraph) -> bool {
    graph
        .nodes
        .iter()
        .any(|node| node.stage != Some(NodeStage::Root))
}

/// After credentials change, poke the running daemon over its control socket so
/// federation is (re)applied *immediately* rather than on the daemon's next poll,
/// and report the result.
///
/// The socket path is derived from the same `dirs` the command used (so a test
/// seam isolates it), and the read deadline is the configured federation timeout
/// plus client slack so the daemon always has time to apply and reply.
///
/// For [`FederationPokeAction::Login`] this is **strict**: if federation cannot
/// be established (the daemon isn't running, the apply timed out, no upstream
/// resolved, or the federation link does not validate), it returns an actionable
/// [`Error::Auth`]. The caller has already persisted the credentials, so the user
/// stays authenticated; only the command exits non-zero. For
/// [`FederationPokeAction::Logout`] it is best-effort and never returns `Err`
/// (de-federation that didn't reach the daemon is harmless; the daemon
/// re-resolves on its next poll).
pub(crate) fn poke_federation_and_report(
    dirs: &PeppyDirs,
    connect_timeout_secs: u64,
    action: FederationPokeAction,
) -> Result<()> {
    let socket = daemon_control::federation_control_socket_path(dirs);
    let read_timeout = Duration::from_secs(connect_timeout_secs) + daemon_control::POKE_READ_SLACK;
    // The poke blocks while the daemon re-resolves the user's cloud router and
    // verifies the TLS link, which can take a few seconds; show the same
    // steady-tick spinner as the browser-approval wait so the step isn't a silent
    // pause. Only for a login: a logout's de-federation is best-effort and quick.
    // Cleared before the outcome is reported so the result prints on a clean line.
    let spinner = match action {
        FederationPokeAction::Login => {
            crate::terminal::spinner("Waiting for federation link to establish")
        }
        FederationPokeAction::Logout => None,
    };
    let outcome = daemon_control::poke_refederate(&socket, read_timeout);
    if let Some(pb) = spinner {
        pb.finish_and_clear();
    }
    // A namespace change makes the daemon restart its whole generation. The first
    // ack is `Restarting`; poll the (path-stable) control socket until the daemon
    // is back under the namespace we just wrote, then report the settled outcome.
    if matches!(outcome, PokeOutcome::Restarting) {
        return await_restart(dirs, &socket, read_timeout, &action);
    }
    match action {
        FederationPokeAction::Login => report_login(outcome),
        FederationPokeAction::Logout => {
            report_logout(outcome);
            Ok(())
        }
    }
}

/// Polls until the daemon is back under the namespace the credentials now resolve
/// to, then reports the settled federation outcome. Detects a concurrent
/// credentials change (a second login/logout mid-restart) and a never-recovers
/// timeout. Bounded by [`RESTART_POLL_DEADLINE`].
fn await_restart(
    dirs: &PeppyDirs,
    socket: &std::path::Path,
    read_timeout: Duration,
    action: &FederationPokeAction,
) -> Result<()> {
    // The namespace the daemon must come back under (what we just wrote).
    let expected = current_creds_namespace(dirs);
    // This helper is shared by both flows, so the recovery guidance must name the
    // caller's own subcommand rather than always saying `login`.
    let subcommand = match action {
        FederationPokeAction::Login => "login",
        FederationPokeAction::Logout => "logout",
    };
    let spinner =
        crate::terminal::spinner("Waiting for the daemon to restart under the new namespace");
    let deadline = Instant::now() + RESTART_POLL_DEADLINE;
    let result = loop {
        if Instant::now() >= deadline {
            break Err(Error::Auth(format!(
                "the daemon did not come back under namespace `{expected}` within the timeout; \
                 check the `peppy service serve` logs and re-run `peppy auth {subcommand}`"
            )));
        }
        std::thread::sleep(RESTART_POLL_INTERVAL);

        // A concurrent login/logout rewrote the credentials mid-restart, so the
        // daemon will not come back under what we wrote.
        if current_creds_namespace(dirs) != expected {
            break Err(Error::Auth(format!(
                "credentials changed during restart; re-run `peppy auth {subcommand}`"
            )));
        }

        // The (path-stable) daemon state records the live generation's namespace,
        // written before the control socket binds. While the daemon is down or the
        // old generation is still up, this is unreadable or carries the old value.
        // Read from the same `dirs` the command resolved, like the socket path.
        let back_under_expected = matches!(
            DaemonState::read_from(&DaemonState::state_file_in(dirs.root())),
            Ok(state) if state.organization_namespace == expected
        );
        if !back_under_expected {
            continue;
        }

        // Back under the expected namespace. Confirm the settled federation state
        // with a fresh poke (which now resolves "unchanged" and federates live).
        match daemon_control::poke_refederate(socket, read_timeout) {
            // Still settling: the new generation wrote its state (so we got here)
            // but its control socket may not have bound yet, so a poke can
            // transiently find no socket or time out. Keep polling until it
            // actually answers (or the deadline above fires) rather than reporting
            // one of these in-flight outcomes as the settled state.
            PokeOutcome::Restarting | PokeOutcome::DaemonNotRunning | PokeOutcome::TimedOut => {
                continue;
            }
            other => {
                break match action {
                    FederationPokeAction::Login => report_login(other),
                    FederationPokeAction::Logout => {
                        report_logout(other);
                        Ok(())
                    }
                };
            }
        }
    };
    if let Some(pb) = spinner {
        pb.finish_and_clear();
    }
    result
}

/// Strict reporting for a login poke: success and a managed router pinned via
/// `ZENOH_CONFIG` print and return `Ok`; every "federation not in effect" outcome
/// returns an actionable [`Error::Auth`] (credentials are already saved by the
/// caller, so the identity is kept; only the command fails).
fn report_login(outcome: PokeOutcome) -> Result<()> {
    match outcome {
        PokeOutcome::Applied(applied) if applied.backend.is_some() => {
            println!("Router federation established.");
            Ok(())
        }
        PokeOutcome::Pinned => {
            // The managed router's `ZENOH_CONFIG` pin prevents the daemon from
            // rewriting it. Treat that operator choice as non-fatal.
            println!("{PINNED_NOTE}");
            Ok(())
        }
        PokeOutcome::Unreachable { reason, .. } => Err(Error::Auth(format!(
            "logged in, but federation with the platform could not be established: {reason}. \
             The per-user cloud router is unreachable or its certificate is not trusted; in \
             dev, ensure the router cert is signed by the committed dev CA (re-run \
             gen_dev_certs), then run `peppy auth login` again."
        ))),
        PokeOutcome::DaemonError(msg) => Err(Error::Auth(format!(
            "logged in, but the daemon could not apply federation: {msg}. Check the \
             `peppy service serve` logs and run `peppy auth login` again."
        ))),
        PokeOutcome::TimedOut => Err(Error::Auth(
            "logged in, but the daemon did not apply federation within the timeout. Check the \
             `peppy service serve` logs and run `peppy auth login` again."
                .to_string(),
        )),
        PokeOutcome::DaemonNotRunning => Err(Error::Auth(
            "logged in, but no running peppy daemon was found to establish federation. Start it \
             with `peppy service serve`, then run `peppy auth login` again."
                .to_string(),
        )),
        PokeOutcome::Applied(_) => Err(Error::Auth(
            "logged in, but no cloud router resolved to federate to. Confirm the backend is \
             reachable and your account is provisioned, then run `peppy auth login` again."
                .to_string(),
        )),
        // `Restarting` is intercepted by `poke_federation_and_report` (it drives
        // the restart poll), so it should not reach here; treat defensively.
        PokeOutcome::Restarting => Err(Error::Auth(
            "the daemon is restarting to apply the new namespace; re-run `peppy auth login` \
             once it is back."
                .to_string(),
        )),
    }
}

/// Best-effort reporting for a logout poke: print a one-line status and never
/// fail. De-federation that didn't reach the daemon is harmless.
fn report_logout(outcome: PokeOutcome) {
    match outcome {
        PokeOutcome::Applied(applied) if applied.backend.is_none() => {
            println!("Router federation cleared.")
        }
        PokeOutcome::Applied(_) => println!("Router federation refreshed."),
        PokeOutcome::Pinned => println!("{PINNED_NOTE}"),
        PokeOutcome::Unreachable { reason: msg, .. } | PokeOutcome::DaemonError(msg) => {
            println!("Note: the daemon could not apply federation now ({msg}); it will retry.")
        }
        PokeOutcome::DaemonNotRunning => {
            println!("No running daemon; nothing to de-federate.")
        }
        PokeOutcome::TimedOut => {
            println!("Federation poke timed out; the daemon will retry shortly.")
        }
        // Intercepted by `poke_federation_and_report`; defensive only.
        PokeOutcome::Restarting => {
            println!("The daemon is restarting to apply the cleared namespace.")
        }
    }
}

#[derive(Subcommand)]
pub enum AuthCommands {
    /// Log in to Peppy via the browser (OAuth device flow)
    Login {
        /// Override the backend base URL (else the build default / PEPPY_API_URL).
        #[arg(long = "api-url")]
        api_url: Option<String>,
        /// Print the verification URL/code instead of opening a browser.
        #[arg(long = "no-browser")]
        no_browser: bool,
        /// Skip the "this restarts the daemon and wipes the node stack" prompt.
        #[arg(long = "yes", short = 'y')]
        yes: bool,
    },
    /// Log out: revoke the access token on the backend and clear local credentials
    Logout {
        #[arg(long = "api-url")]
        api_url: Option<String>,
        /// Skip the "this restarts the daemon and wipes the node stack" prompt.
        #[arg(long = "yes", short = 'y')]
        yes: bool,
    },
    /// Show the current Peppy identity, backend, and token status
    #[command(visible_alias = "status")]
    Whoami {
        #[arg(long = "api-url")]
        api_url: Option<String>,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
}

pub struct AuthCommand {
    pub command: AuthCommands,
}

impl Command for AuthCommand {
    fn execute(self, app_ctx: &Arc<AppContext>) -> Result<()> {
        match self.command {
            AuthCommands::Login {
                api_url,
                no_browser,
                yes,
            } => login::LoginCommand {
                api_url,
                no_browser,
                yes,
                peppy_dirs: None,
            }
            .execute(app_ctx),
            AuthCommands::Logout { api_url, yes } => logout::LogoutCommand {
                api_url,
                yes,
                peppy_dirs: None,
            }
            .execute(app_ctx),
            AuthCommands::Whoami { api_url, json } => whoami::WhoamiCommand {
                api_url,
                json,
                peppy_dirs: None,
            }
            .execute(app_ctx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{federation_poke_timeout_secs, report_login, report_logout, stack_has_user_nodes};
    use core_node_api::{
        InstanceState, NodeStage, SerializedInstance, SerializedNode, SerializedNodeGraph,
    };
    use daemon::control::PokeOutcome;
    use daemon::state::DaemonState;
    use daemon_config::consts::PeppyDirs;
    use daemon_config::peppy_config::{
        ExternalZenohConfig, ManagedZenohConfig, PeppyConfig, ZenohConfig,
    };
    use std::collections::BTreeMap;

    fn managed_config() -> PeppyConfig {
        PeppyConfig {
            zenoh: ZenohConfig::Managed(ManagedZenohConfig::default()),
            ..PeppyConfig::default()
        }
    }

    fn external_config() -> PeppyConfig {
        PeppyConfig {
            zenoh: ZenohConfig::External(ExternalZenohConfig {
                endpoint: "tcp/router.example:7447".to_string(),
            }),
            ..PeppyConfig::default()
        }
    }

    /// Writes a daemon state file under `dirs` whose recorded pid is this test
    /// process (so `is_running` holds) and whose federation field is `timeout`.
    fn write_running_state(dirs: &PeppyDirs, timeout: Option<u64>) {
        let state = DaemonState::new("cn-test", "127.0.0.1", 7447, "test", 5, "local", timeout);
        DaemonState::write_to(&DaemonState::state_file_in(dirs.root()), &state)
            .expect("write daemon state");
    }

    #[test]
    fn with_no_daemon_running_the_disk_config_decides() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dirs = PeppyDirs::new(dir.path());
        let managed = managed_config();
        assert_eq!(
            federation_poke_timeout_secs(&dirs, &managed),
            managed.zenoh.federation().map(|f| f.connect_timeout_secs),
            "no state file: a managed disk config supplies the poke timeout"
        );
        assert_eq!(
            federation_poke_timeout_secs(&dirs, &external_config()),
            None,
            "no state file: an external disk config means no poke"
        );
    }

    #[test]
    fn a_running_daemon_beats_the_disk_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dirs = PeppyDirs::new(dir.path());

        // Managed daemon, external config on disk: the poke must still happen,
        // with the daemon's own timeout.
        write_running_state(&dirs, Some(7));
        assert_eq!(
            federation_poke_timeout_secs(&dirs, &external_config()),
            Some(7),
            "a running managed daemon must be poked even if the disk config went external"
        );

        // External daemon, managed config on disk: there is no control socket,
        // so login/logout must not warn about a restart or poke anything.
        write_running_state(&dirs, None);
        assert_eq!(
            federation_poke_timeout_secs(&dirs, &managed_config()),
            None,
            "a running external daemon has no control socket to poke"
        );
    }

    #[test]
    fn a_stale_state_file_from_a_dead_daemon_falls_back_to_the_disk_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dirs = PeppyDirs::new(dir.path());
        let mut state = DaemonState::new("cn-test", "127.0.0.1", 7447, "test", 5, "local", None);
        // A pid outside the valid range names no live process, so the state is
        // stale and the disk config decides again.
        state.daemon_pid = Some(u32::MAX);
        DaemonState::write_to(&DaemonState::state_file_in(dirs.root()), &state)
            .expect("write daemon state");

        let managed = managed_config();
        assert_eq!(
            federation_poke_timeout_secs(&dirs, &managed),
            managed.zenoh.federation().map(|f| f.connect_timeout_secs),
            "a dead daemon's state must not override the disk config"
        );
    }

    /// Builds an instance-less node fixed at `stage`. The bindings/instances are
    /// irrelevant to the user-node predicate, which keys only on `stage`.
    fn node_with_stage(name: &str, stage: NodeStage) -> SerializedNode {
        SerializedNode {
            name: name.to_string(),
            tag: "v1".to_string(),
            core_node: "test-core".to_string(),
            config_path: format!("/tmp/{name}.json5"),
            artifact_path: None,
            stage: Some(stage),
            instances: Vec::new(),
        }
    }

    fn graph_of(nodes: Vec<SerializedNode>) -> SerializedNodeGraph {
        SerializedNodeGraph {
            nodes,
            edges: Vec::new(),
        }
    }

    fn applied(backend: Option<&str>) -> PokeOutcome {
        PokeOutcome::Applied(daemon::control::AppliedFederation {
            backend: backend.map(str::to_string),
            peers: Vec::new(),
        })
    }

    #[test]
    fn a_stack_with_only_the_core_root_has_no_user_nodes() {
        let graph = graph_of(vec![node_with_stage("core", NodeStage::Root)]);
        assert!(
            !stack_has_user_nodes(&graph),
            "a daemon carrying only its synthetic root is an empty stack"
        );
    }

    #[test]
    fn a_stack_with_a_user_node_alongside_the_root_has_user_nodes() {
        let graph = graph_of(vec![
            node_with_stage("core", NodeStage::Root),
            node_with_stage("sensor", NodeStage::Added),
        ]);
        assert!(stack_has_user_nodes(&graph));
    }

    #[test]
    fn an_empty_graph_has_no_user_nodes() {
        assert!(!stack_has_user_nodes(&graph_of(Vec::new())));
    }

    #[test]
    fn a_user_node_with_only_terminal_instances_still_counts() {
        // "Empty" is about node entities present in the stack, not running
        // instances: a node whose only instance has finished is still in the
        // stack and would be wiped by a restart, so it must keep the warning.
        let mut recorder = node_with_stage("recorder", NodeStage::Ready);
        recorder.instances = vec![SerializedInstance {
            instance_id: "rec-1".to_string(),
            state: InstanceState::Finished,
            healthy: true,
            slot_bindings: BTreeMap::new(),
            pairing_slots: BTreeMap::new(),
        }];
        let graph = graph_of(vec![node_with_stage("core", NodeStage::Root), recorder]);
        assert!(stack_has_user_nodes(&graph));
    }

    #[test]
    fn login_is_ok_for_applied_some_and_pinned() {
        assert!(
            report_login(applied(Some("tls/cap:7443"))).is_ok(),
            "a verified upstream ⇒ login succeeds"
        );
        assert!(
            report_login(PokeOutcome::Pinned).is_ok(),
            "a managed router pinned via ZENOH_CONFIG is non-fatal for login"
        );
    }

    #[test]
    fn login_fails_strictly_for_every_not_in_effect_outcome() {
        let failing = [
            applied(None),
            unreachable("UnknownCA"),
            PokeOutcome::DaemonError("boom".to_string()),
            PokeOutcome::TimedOut,
            PokeOutcome::DaemonNotRunning,
        ];
        for outcome in failing {
            assert!(
                report_login(outcome).is_err(),
                "login must fail when federation is not in effect"
            );
        }
    }

    #[test]
    fn unreachable_login_error_is_actionable_and_carries_the_reason() {
        let err = report_login(unreachable("received fatal alert: UnknownCA"))
            .expect_err("an unreachable upstream fails login");
        let msg = err.to_string();
        assert!(
            msg.contains("UnknownCA"),
            "the probe reason is surfaced: {msg}"
        );
        assert!(
            msg.contains("gen_dev_certs"),
            "the message is actionable for dev: {msg}"
        );
    }

    #[test]
    fn logout_is_always_best_effort() {
        // `report_logout` returns `()` for every variant; a logout can never be
        // failed by the federation poke.
        for outcome in [
            applied(None),
            applied(Some("tls/cap:7443")),
            PokeOutcome::Pinned,
            unreachable("x"),
            PokeOutcome::DaemonError("y".to_string()),
            PokeOutcome::DaemonNotRunning,
            PokeOutcome::TimedOut,
        ] {
            report_logout(outcome);
        }
    }

    fn unreachable(reason: &str) -> PokeOutcome {
        PokeOutcome::Unreachable {
            reason: reason.to_string(),
            applied: daemon::control::AppliedFederation {
                backend: Some("tls/cap:7443".to_string()),
                peers: Vec::new(),
            },
        }
    }
}
