mod list;
mod run;
mod types;

use clap::Subcommand;
use config::Language;
use std::path::PathBuf;
use tracing::info;

use super::{Command, Error as CommandError};

use create::NodeBuilder;

pub mod create;
pub use types::NodeName;

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
    fn execute(self) -> Result<(), CommandError> {
        match self.command {
            NodeCommands::Create {
                to_dir,
                lang,
                node_name,
                description,
                full,
            } => NodeBuilder::new(node_name)
                .to_dir(to_dir)
                .lang(lang)
                .description(description)
                .full(full)
                .build(),
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
