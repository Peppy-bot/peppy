mod builder;
mod core_node;
mod messaging_router;
mod router_federation;
mod shutdown_signal;

pub mod install;
pub mod reset;
pub mod serve;

use super::Command;
use crate::{context::AppContext, error::Error as CommandError};
use clap::{Subcommand, ValueEnum};
use std::sync::Arc;

/// Daemon-wide default for the time source nodes read through `PeppyClock`.
/// Per-instance launcher overrides win over this; this is the fallback for
/// instances that omit the framework block.
#[derive(Copy, Clone, Debug, ValueEnum, Default, PartialEq, Eq)]
pub enum ClockSource {
    /// Read OS wall time (`SystemTime::now()`-equivalent).
    #[default]
    Wall,
    /// Read timestamps from the daemon's cache, fed by an external publisher
    /// on the `clock` topic. Use this for simulators and bag replay.
    Sim,
}

impl ClockSource {
    /// Convenience: `true` when the daemon should treat sim as the default.
    pub fn use_sim_time(self) -> bool {
        matches!(self, ClockSource::Sim)
    }
}

#[derive(Subcommand)]
pub enum ServiceCommands {
    /// Run the peppy service that listen to node communication, node configuration file changes and also act as a Zenoh router.
    /// This is the background service that runs with the systemd peppy service.
    Serve {
        /// Messaging engine to use (zenoh by default)
        #[arg(long, default_value = "zenoh")]
        messaging_engine: String,
        /// Optional name for the core node
        #[arg(long)]
        core_node_name: Option<String>,
        /// Daemon-wide clock source. Per-instance `framework.use_sim_time`
        /// overrides this. Defaults to `wall`.
        #[arg(long, value_enum, default_value_t = ClockSource::Wall)]
        clock_source: ClockSource,
    },
    /// Install the peppy daemon as a background service (user-level by default; run with sudo for system-wide).
    Install {},
    /// Stop the peppy background service.
    Stop {},
    /// Uninstall the peppy background service.
    Uninstall {},
    /// Reset the current core node stack (clears all nodes except the core node).
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
                core_node_name,
                clock_source,
            } => serve::ServeCommand {
                messaging_engine,
                core_node_name,
                clock_source,
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
