use clap::Subcommand;
use std::path::PathBuf;

pub mod create;
pub mod error;
pub mod list;
pub mod types;

use super::{Command, CommandError};
use create::NodeBuilder;
use types::{Language, NodeName};

#[derive(Subcommand)]
pub enum NodeCommands {
    /// Create a new peppy node
    Create {
        /// Name of the node directory to create
        node_name: NodeName,
        /// Optional: Description for the node
        #[arg(long)]
        description: Option<String>,
        /// Optional: target directory (defaults to current directory)
        #[arg(long)]
        to_dir: Option<PathBuf>,
        /// Programming language for the node, either `rust` or `python`. Defaults to `rust`
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
            } => {
                let current_dir = std::env::current_dir()?;
                NodeBuilder::new(node_name)
                    .current_dir(current_dir)
                    .to_dir(to_dir)
                    .lang(lang)
                    .description(description)
                    .build()
            }
            NodeCommands::List {} => {
                eprintln!("Listing nodes...");
                list::list_nodes();
                Ok(())
            }
            NodeCommands::Check {} => {
                eprintln!("Checking nodes...");
                list::check();
                Ok(())
            }
        }
    }
}

pub fn handle_node_command(command: NodeCommands) {
    let node_command = NodeCommand { command };
    if let Err(e) = node_command.execute() {
        eprintln!("Failed to execute node command: {}", e);
        std::process::exit(1);
    }
}
