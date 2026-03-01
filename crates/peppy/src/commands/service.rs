mod builder;
mod daemon_node;
mod messaging_router;

pub mod install;
pub mod reset;
pub mod serve;

use super::Command;
use crate::{context::AppContext, error::Error as CommandError};
use clap::Subcommand;
use std::sync::Arc;

#[derive(Subcommand)]
pub enum ServiceCommands {
    /// Run the peppy service that listen to node communication, node configuration file changes and also act as a Zenoh router.
    /// This is the background service that runs with the systemd peppy service.
    Serve {
        /// Messaging engine to use (zenoh by default)
        #[arg(long, default_value = "zenoh")]
        messaging_engine: String,
        /// Optional name for the daemon node
        #[arg(long)]
        daemon_name: Option<String>,
    },
    /// Install the peppy daemon as a background service (user-level by default; run with sudo for system-wide).
    Install {},
    /// Stop the peppy background service.
    Stop {},
    /// Uninstall the peppy background service.
    Uninstall {},
    /// Reset the current daemon node stack (clears all nodes except the daemon).
    Reset {},
}

pub struct ServiceCommand {
    pub command: ServiceCommands,
}

impl Command for ServiceCommand {
    fn execute(self, app_ctx: &Arc<AppContext>) -> Result<(), CommandError> {
        match self.command {
            ServiceCommands::Serve {
                messaging_engine,
                daemon_name,
            } => serve::ServeCommand {
                messaging_engine,
                daemon_name,
                shutdown_token: None,
            }
            .execute(app_ctx),
            ServiceCommands::Install {} => install::InstallCommand {}.execute(app_ctx),
            ServiceCommands::Stop {} => install::StopCommand {}.execute(app_ctx),
            ServiceCommands::Uninstall {} => install::UninstallCommand {}.execute(app_ctx),
            ServiceCommands::Reset {} => reset::ResetCommand {}.execute(app_ctx),
        }
    }
}
