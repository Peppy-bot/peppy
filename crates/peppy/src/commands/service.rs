use clap::Subcommand;

use super::{Command, Error as CommandError};
use crate::AppContext;

#[derive(Subcommand)]
pub enum ServiceCommands {
    /// Installs the service on the system in prod mode
    Deploy,
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
    fn execute(self, app_ctx: &AppContext) -> Result<(), CommandError> {
        match self.command {
            ServiceCommands::Deploy => {
                todo!(
                    "Set PEPPY_ENV=PROD in the systemd/launchctl env var when the service is installed"
                );
            }
            ServiceCommands::Serve {
                messaging_engine,
                master_name,
            } => super::serve::ServeCommand {
                messaging_engine,
                master_name,
            }
            .execute(&app_ctx),
            ServiceCommands::Install {} => super::install::InstallCommand {}.execute(app_ctx),
        }
    }
}
