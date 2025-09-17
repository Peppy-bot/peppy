use clap::Subcommand;

use super::{Command, Error as CommandError};
use crate::AppContext;

#[derive(Subcommand)]
pub enum ServiceCommands {
    /// Installs the service on the system in prod mode
    Deploy,
}

pub struct ServiceCommand {
    pub command: ServiceCommands,
}

impl Command for ServiceCommand {
    fn execute(self, _ctx: &AppContext) -> Result<(), CommandError> {
        match self.command {
            ServiceCommands::Deploy => {
                todo!(
                    "Set PEPPY_ENV=PROD in the systemd/launchctl env var when the service is installed"
                );
            }
        }
    }
}
