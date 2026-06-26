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
use crate::{context::AppContext, error::Result};

/// Whether a federation poke follows a login (federate) or a logout
/// (de-federate). Only affects the user-facing wording.
pub(crate) enum FederationPokeAction {
    Login,
    Logout,
}

/// After credentials change, poke the running daemon over its control socket so
/// federation is (re)applied *immediately* rather than on the daemon's next poll,
/// and print a one-line status.
///
/// Best effort: this never fails the command. The socket path is derived from the
/// same `dirs` the command used (so a test seam isolates it), and the read
/// deadline is the configured federation timeout plus client slack so the daemon
/// always has time to apply and reply.
pub(crate) fn poke_federation_and_report(
    dirs: &PeppyDirs,
    connect_timeout_secs: u64,
    action: FederationPokeAction,
) {
    let socket = daemon_control::federation_control_socket_path(dirs);
    let read_timeout = Duration::from_secs(connect_timeout_secs) + daemon_control::POKE_READ_SLACK;
    let outcome = daemon_control::poke_refederate(&socket, read_timeout);
    match (action, outcome) {
        (_, PokeOutcome::Applied(Some(_))) => println!("Router federation refreshed."),
        (FederationPokeAction::Logout, PokeOutcome::Applied(None)) => {
            println!("Router federation cleared.")
        }
        (FederationPokeAction::Login, PokeOutcome::Applied(None)) => {
            println!("No cloud router to federate to yet; the daemon will retry shortly.")
        }
        (_, PokeOutcome::Pinned) => println!(
            "Note: this daemon's router config is operator-pinned (ZENOH_CONFIG); \
             federation is not auto-managed."
        ),
        (_, PokeOutcome::DaemonError(msg)) => {
            println!("Note: the daemon could not apply federation now ({msg}); it will retry.")
        }
        (FederationPokeAction::Login, PokeOutcome::DaemonNotRunning) => {
            println!("No running daemon; federation will be applied the next time it starts.")
        }
        (FederationPokeAction::Logout, PokeOutcome::DaemonNotRunning) => {
            println!("No running daemon; nothing to de-federate.")
        }
        (_, PokeOutcome::TimedOut) => {
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
