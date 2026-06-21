//! The `peppy auth` command group: `login`, `logout`, and `whoami`. Each
//! variant maps to a handler in this module's directory; the OAuth device flow,
//! token storage, and credential resolution they share live in the separate
//! [`crate::auth`] engine.

pub mod login;
pub mod logout;
pub mod whoami;

use std::sync::Arc;

use clap::Subcommand;

use super::Command;
use crate::{context::AppContext, error::Result};

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
