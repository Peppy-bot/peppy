use std::path::PathBuf;

use clap::Subcommand;
use config::consts::PEPPY_NODE_CONFIG_FILE;

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
        engine: String,

        /// Config file(s) for the selected messaging engine. Will use a default configuration if not provided
        #[arg(long)]
        config_path: Option<PathBuf>,
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
                engine,
                config_path,
            } => {
                let config_path = match config_path {
                    Some(pth) => pth,
                    None => app_ctx.root_dir.join(PEPPY_NODE_CONFIG_FILE),
                };
                super::serve::ServeCommand {
                    engine,
                    root_config_path: config_path,
                }
                .execute(&app_ctx)
            }
            ServiceCommands::Install {} => super::install::InstallCommand {}.execute(app_ctx),
        }
    }
}
