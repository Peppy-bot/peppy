pub mod install;
pub mod serve;
mod ssh_agent;

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
        /// Optional name for the core node. Overrides `core_node_name` in
        /// ~/.peppy/conf/peppy_config.json5; when both are absent a
        /// machine-specific default is derived.
        #[arg(long)]
        core_node_name: Option<String>,
    },
    /// Install the peppy daemon as a background service (user-level by default; run with sudo for system-wide).
    Install {},
    /// Stop the peppy background service.
    Stop {},
    /// Uninstall the peppy background service.
    Uninstall {},
}

pub struct ServiceCommand {
    pub command: ServiceCommands,
}

impl Command for ServiceCommand {
    fn execute(self, app_ctx: &Arc<AppContext>) -> Result<(), CommandError> {
        match self.command {
            ServiceCommands::Serve {
                messaging_engine,
                core_node_name,
            } => {
                ssh_agent::bind_ssh_agent_from_config();
                serve::ServeCommand {
                    messaging_engine,
                    core_node_name,
                    shutdown_token: None,
                    peppy_dirs: daemon_config::consts::PeppyDirs::default(),
                }
                .execute(app_ctx)
            }
            ServiceCommands::Install {} => install::InstallCommand {}.execute(app_ctx),
            ServiceCommands::Stop {} => install::StopCommand {}.execute(app_ctx),
            ServiceCommands::Uninstall {} => install::UninstallCommand {}.execute(app_ctx),
        }
    }
}
