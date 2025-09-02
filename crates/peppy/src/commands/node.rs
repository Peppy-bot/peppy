use clap::Subcommand;
use std::path::PathBuf;
use tracing::{error, info};

pub mod create;
pub mod list;
pub mod types;

use super::{Command, Error as CommandError};

use create::NodeBuilder;
use types::{Language, NodeName};

#[derive(Subcommand)]
pub enum NodeCommands {
    /// Create a new peppy node
    Create {
        /// Name of the node directory to create
        node_name: NodeName,
        /// Optional: Create the node with a more detailed configuration & defaults
        #[arg(long, default_value_t = false)]
        full: bool,
        /// Optional: Description for the node
        #[arg(long)]
        description: Option<String>,
        /// Optional: target directory (defaults to current directory)
        #[arg(long)]
        to_dir: Option<PathBuf>,
        /// Programming language for the node, either `rust` or `python`
        #[arg(long, default_value = "rust")]
        lang: Language,
    },
    /// List nodes in the current system
    List {},
    /// Check that the root peppy.star node and its children are properly formed
    Check {},
}

pub struct NodeCommand {
    pub command: NodeCommands,
}

impl Command for NodeCommand {
    fn execute(self) -> Result<(), CommandError> {
        match self.command {
            NodeCommands::Create {
                to_dir,
                lang,
                node_name,
                description,
                full,
            } => {
                let current_dir = std::env::current_dir()?;
                NodeBuilder::new(node_name)
                    .current_dir(current_dir)
                    .to_dir(to_dir)
                    .lang(lang)
                    .description(description)
                    .full(full)
                    .build()
            }
            NodeCommands::List {} => {
                info!("Listing nodes...");
                list::list_nodes();
                Ok(())
            }
            NodeCommands::Check {} => {
                info!("Checking nodes...");
                list::check();
                Ok(())
            }
        }
    }
}

pub fn handle_node_command(command: NodeCommands) {
    let node_command = NodeCommand { command };
    if let Err(e) = node_command.execute() {
        error!("Failed to execute node command: {}", e);
        std::process::exit(1);
    }
}
