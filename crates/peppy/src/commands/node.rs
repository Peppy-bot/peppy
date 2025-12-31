mod add;
mod init;
mod list;
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
    #[command(visible_alias = "create")]
    Init {
        /// Name of the node directory to create
        node_name: NodeName,
        /// Optional: target directory (defaults to current directory)
        #[arg(long)]
        to_dir: Option<PathBuf>,
        /// Build system for the node: `rust`, `python`, `cargo`, or `uv`
        #[arg(long, visible_alias = "lang", value_enum, default_value_t = BuildSystem::Rust)]
        build_system: BuildSystem,
    },
    /// Add a node to the node stack based on its peppy.json5 file
    Add {
        /// Path to the node configuration file
        #[arg(long)]
        peppy_json5: PathBuf,
        /// If set, will attempt to spawn an instance directly after adding the node to the node stack
        #[arg(long)]
        run: bool,
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
            NodeCommands::Init {
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
            NodeCommands::Add { peppy_json5, run } => {
                info!("Adding node {}...", peppy_json5.display());
                add::add_node(peppy_json5, run);
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
