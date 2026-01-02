mod builder;
mod master_node;
mod messaging_router;
mod pid_lock;

pub mod install;
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
        /// Optional name for the master node
        #[arg(long)]
        master_name: Option<String>,
    },
    // Install the peppy daemon system-wide
    Install {},
}

pub struct ServiceCommand {
    pub command: ServiceCommands,
}

impl Command for ServiceCommand {
    fn execute(self, app_ctx: &Arc<AppContext>) -> Result<(), CommandError> {
        match self.command {
            ServiceCommands::Serve {
                messaging_engine,
                master_name,
            } => serve::ServeCommand {
                messaging_engine,
                master_name,
                shutdown_token: None,
            }
            .execute(app_ctx),
            ServiceCommands::Install {} => install::InstallCommand {}.execute(app_ctx),
        }
    }
}
