//! The `peppy auth` command group: `login`, `logout`, and `whoami`. Each
//! variant maps to a handler in this module's directory; the OAuth device flow,
//! token storage, and credential resolution they share live in the separate
//! [`crate::auth`] engine.

pub mod login;
pub mod logout;
pub mod whoami;

use std::sync::Arc;
use std::time::Duration;

use clap::Subcommand;
use config::consts::PeppyDirs;

use super::Command;
use crate::daemon_control::{self, PokeOutcome};
use crate::error::Error;
use crate::{context::AppContext, error::Result};

/// Whether a federation poke follows a login (federate) or a logout
/// (de-federate). Affects the user-facing wording and, crucially, whether a
/// federation failure is fatal: a login that cannot establish federation fails
/// (the credentials are kept), while a logout is always best-effort.
pub(crate) enum FederationPokeAction {
    Login,
    Logout,
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
/// be established — the daemon isn't running, the apply timed out, no upstream
/// resolved, or the federation link does not validate — it returns an actionable
/// [`Error::Auth`]. The caller has already persisted the credentials, so the user
/// stays authenticated; only the command exits non-zero. For
/// [`FederationPokeAction::Logout`] it is best-effort and never returns `Err`
/// (de-federation that didn't reach the daemon is harmless — the daemon
/// re-resolves on its next poll).
pub(crate) fn poke_federation_and_report(
    dirs: &PeppyDirs,
    connect_timeout_secs: u64,
    action: FederationPokeAction,
) -> Result<()> {
    let socket = daemon_control::federation_control_socket_path(dirs);
    let read_timeout = Duration::from_secs(connect_timeout_secs) + daemon_control::POKE_READ_SLACK;
    let outcome = daemon_control::poke_refederate(&socket, read_timeout);
    match action {
        FederationPokeAction::Login => report_login(outcome),
        FederationPokeAction::Logout => {
            report_logout(outcome);
            Ok(())
        }
    }
}

/// Strict reporting for a login poke: success and the operator-pinned case print
/// and return `Ok`; every "federation not in effect" outcome returns an
/// actionable [`Error::Auth`] (credentials are already saved by the caller, so
/// the identity is kept — only the command fails).
fn report_login(outcome: PokeOutcome) -> Result<()> {
    match outcome {
        PokeOutcome::Applied(Some(_)) => {
            println!("Router federation established.");
            Ok(())
        }
        PokeOutcome::Pinned => {
            // The operator owns this router's config via ZENOH_CONFIG, so the CLI
            // is not responsible for federating it. Treat as non-fatal.
            println!(
                "Note: this daemon's router config is operator-pinned (ZENOH_CONFIG); \
                 federation is not auto-managed."
            );
            Ok(())
        }
        PokeOutcome::Unreachable(reason) => Err(Error::Auth(format!(
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
        PokeOutcome::Applied(None) => Err(Error::Auth(
            "logged in, but no cloud router resolved to federate to. Confirm the backend is \
             reachable and your account is provisioned, then run `peppy auth login` again."
                .to_string(),
        )),
    }
}

/// Best-effort reporting for a logout poke: print a one-line status and never
/// fail. De-federation that didn't reach the daemon is harmless.
fn report_logout(outcome: PokeOutcome) {
    match outcome {
        PokeOutcome::Applied(None) => println!("Router federation cleared."),
        PokeOutcome::Applied(Some(_)) => println!("Router federation refreshed."),
        PokeOutcome::Pinned => println!(
            "Note: this daemon's router config is operator-pinned (ZENOH_CONFIG); \
             federation is not auto-managed."
        ),
        PokeOutcome::Unreachable(msg) | PokeOutcome::DaemonError(msg) => {
            println!("Note: the daemon could not apply federation now ({msg}); it will retry.")
        }
        PokeOutcome::DaemonNotRunning => {
            println!("No running daemon; nothing to de-federate.")
        }
        PokeOutcome::TimedOut => {
            println!("Federation poke timed out; the daemon will retry shortly.")
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
    },
    /// Log out: revoke the access token on the backend and clear local credentials
    Logout {
        #[arg(long = "api-url")]
        api_url: Option<String>,
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
            } => login::LoginCommand {
                api_url,
                no_browser,
                peppy_dirs: None,
            }
            .execute(app_ctx),
            AuthCommands::Logout { api_url } => logout::LogoutCommand {
                api_url,
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
    use super::{report_login, report_logout};
    use crate::daemon_control::PokeOutcome;

    #[test]
    fn login_is_ok_for_applied_some_and_pinned() {
        assert!(
            report_login(PokeOutcome::Applied(Some("tls/cap:7443".to_string()))).is_ok(),
            "a verified upstream ⇒ login succeeds"
        );
        assert!(
            report_login(PokeOutcome::Pinned).is_ok(),
            "an operator-pinned router is non-fatal for login"
        );
    }

    #[test]
    fn login_fails_strictly_for_every_not_in_effect_outcome() {
        let failing = [
            PokeOutcome::Applied(None),
            PokeOutcome::Unreachable("UnknownCA".to_string()),
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
        let err = report_login(PokeOutcome::Unreachable(
            "received fatal alert: UnknownCA".to_string(),
        ))
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
        // `report_logout` returns `()` for every variant — a logout can never be
        // failed by the federation poke.
        for outcome in [
            PokeOutcome::Applied(None),
            PokeOutcome::Applied(Some("tls/cap:7443".to_string())),
            PokeOutcome::Pinned,
            PokeOutcome::Unreachable("x".to_string()),
            PokeOutcome::DaemonError("y".to_string()),
            PokeOutcome::DaemonNotRunning,
            PokeOutcome::TimedOut,
        ] {
            report_logout(outcome);
        }
    }
}
