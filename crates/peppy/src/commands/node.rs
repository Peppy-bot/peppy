mod list;
mod run;
mod types;

use clap::{Subcommand, ValueEnum};
use core::fmt;
use std::path::PathBuf;
use tracing::info;

use super::{Command, Error as CommandError};
use crate::AppContext;

use create::NodeBuilder;

pub mod create;
pub use types::NodeName;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum Language {
    Python,
    #[default]
    Rust,
}

impl fmt::Display for Language {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Language::Python => write!(f, "python"),
            Language::Rust => write!(f, "rust"),
        }
    }
}

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
        #[arg(long, value_enum, default_value_t = Language::Rust)]
        lang: Language,
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
    fn execute(self, ctx: &AppContext) -> Result<(), CommandError> {
        match self.command {
            NodeCommands::Create {
                to_dir,
                lang,
                node_name,
                description,
                full,
            } => {
                let mut node_builder = NodeBuilder::new(ctx, node_name)
                    .lang(lang)
                    .description(description)
                    .full(full);

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
