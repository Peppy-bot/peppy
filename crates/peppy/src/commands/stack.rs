mod launch;
mod list;

use std::path::PathBuf;
use std::sync::Arc;

use clap::Subcommand;
use tracing::info;

use super::Command;
use crate::{context::AppContext, error::Error as CommandError};

#[derive(Subcommand)]
pub enum StackCommands {
    /// Launches a deployment, replacing the current node Stack
    Launch {
        /// Path to the peppy launcher configuration file
        launcher_config_path: PathBuf,
    },
    /// List the nodes in the current node stack
    List {
        /// If specified, will save a dotgraph representation at the given path
        dot_graph_path: Option<PathBuf>,
    },
}

pub struct StackCommand {
    pub command: StackCommands,
}

impl Command for StackCommand {
    fn execute(self, ctx: &Arc<AppContext>) -> Result<(), CommandError> {
        match self.command {
            StackCommands::List { dot_graph_path } => {
                info!("Listing nodes...");
                list::list_nodes(ctx, dot_graph_path)
            }
            StackCommands::Launch {
                launcher_config_path,
            } => {
                info!("Launching stack...");
                launch::launch(ctx, launcher_config_path)
            }
        }
    }
}
