mod launch;
mod list;

pub use list::list_nodes_collecting;

use std::path::PathBuf;
use std::sync::Arc;

use clap::Subcommand;
use tracing::info;

use super::Command;
use super::node::DEFAULT_IDLE_TIMEOUT_SECS;
use crate::{context::AppContext, error::Error as CommandError};

#[derive(Subcommand)]
pub enum StackCommands {
    /// Launches a deployment, replacing the current node Stack
    Launch {
        /// Path to the peppy launcher configuration file
        launcher_config_path: PathBuf,
        /// Idle timeout in seconds for the node add phase (resets on git/http progress or sub-process output)
        #[arg(long, default_value_t = DEFAULT_IDLE_TIMEOUT_SECS)]
        node_add_idle_timeout_secs: u64,
        /// Idle timeout in seconds for the node build phase (resets on build_cmd output)
        #[arg(long, default_value_t = DEFAULT_IDLE_TIMEOUT_SECS)]
        node_build_idle_timeout_secs: u64,
        /// Idle timeout in seconds for the node run-startup phase (resets on subprocess output until the node signals ready)
        #[arg(long, default_value_t = DEFAULT_IDLE_TIMEOUT_SECS)]
        node_run_idle_timeout_secs: u64,
        /// Optional absolute max timeout in seconds for the entire launch. If unset, only idle timeouts apply.
        #[arg(long)]
        max_timeout_secs: Option<u64>,
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
            StackCommands::List { dot_graph_path } => list::list_nodes(ctx, dot_graph_path),
            StackCommands::Launch {
                launcher_config_path,
                node_add_idle_timeout_secs,
                node_build_idle_timeout_secs,
                node_run_idle_timeout_secs,
                max_timeout_secs,
            } => {
                info!("Launching stack...");
                launch::launch(
                    ctx,
                    launcher_config_path,
                    node_add_idle_timeout_secs,
                    node_build_idle_timeout_secs,
                    node_run_idle_timeout_secs,
                    max_timeout_secs,
                )
            }
        }
    }
}
