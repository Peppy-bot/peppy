mod init;
mod list;
mod run;
mod types;

use std::path::PathBuf;
use std::sync::Arc;

use clap::Subcommand;
use config::peppy_config::BuildSystem;
use tracing::info;

use super::Command;
use crate::{context::AppContext, error::Error as CommandError};

use init::NodeBuilder;

pub use types::NodeName;

#[derive(Subcommand)]
pub enum NodeCommands {
    /// Create a new peppy node
    #[command(visible_alias = "init")]
    Create {
        /// Name of the node directory to create
        node_name: NodeName,
        /// Optional: target directory (defaults to current directory)
        #[arg(long)]
        to_dir: Option<PathBuf>,
        /// Build system for the node: `rust`, `python`, `cargo`, or `uv`
        #[arg(long, visible_alias = "lang", value_enum, default_value_t = BuildSystem::Rust)]
        build_system: BuildSystem,
    },
    /// Runs a specific node
    Run {
        /// Name of the node to start. If it isn't found in the current network, it will be pulled from the nodes.peppy.bot repo
        node_name: NodeName,
        /// Optional: path to the configuration file. If provided, will attempt to run that node directly from that path and add it to the node list
        #[arg(long)]
        configuration_file: Option<PathBuf>,
    },
    /// List nodes in the current node network
    List {},
}

pub struct NodeCommand {
    pub command: NodeCommands,
}

impl Command for NodeCommand {
    fn execute(self, ctx: &Arc<AppContext>) -> Result<(), CommandError> {
        match self.command {
            NodeCommands::Create {
                to_dir,
                build_system,
                node_name,
            } => {
                let mut node_builder = NodeBuilder::new(ctx, node_name).build_system(build_system);

                if let Some(dir) = to_dir {
                    node_builder = node_builder.to_dir(dir);
                }

                node_builder.build()
            }
            NodeCommands::Run {
                node_name,
                configuration_file,
            } => {
                info!("Running node {node_name}...");
                run::run_node(node_name, configuration_file);
                Ok(())
            }
            NodeCommands::List {} => {
                info!("Listing nodes...");
                list::list_nodes();
                Ok(())
            }
        }
    }
}
