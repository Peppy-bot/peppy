use clap::Subcommand;
use std::path::PathBuf;

pub mod create;

#[derive(Subcommand)]
pub enum NodeCommands {
    /// Create a new peppy node
    Create {
        /// Name of the node directory to create
        node_name: String,
        /// Optional target directory (defaults to current directory)
        #[arg(long)]
        to_dir: Option<PathBuf>,
    },
    /// List nodes in the current system
    List {

    },
}

pub fn handle_node_command(command: NodeCommands) {
    match command {
        NodeCommands::Create { node_name, to_dir } => {
            if let Err(e) = create::create(&node_name, to_dir) {
                eprintln!("Failed to create node: {}", e);
                std::process::exit(1);
            }
        },
        NodeCommands::List {  } => {
            eprintln!("Listing nodes...");
        }
    }
}