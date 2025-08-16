use clap::Subcommand;
use std::path::PathBuf;

pub mod create;
pub mod deps;
pub mod list;

#[derive(Subcommand)]
pub enum NodeCommands {
    /// Create a new peppy node
    Create {
        /// Optional: Description for the node
        description: Option<String>,
        /// Optional: target directory (defaults to current directory)
        #[arg(long)]
        to_dir: Option<PathBuf>,
        /// Programming language for the node, either `rust` or `python`. Defaults to `python`
        #[arg(long, default_value = "rust")]
        lang: String,
        /// Name of the node directory to create
        node_name: String,
    },
    /// List nodes in the current system
    List {},
    /// Check that the root peppy.star node and its children are properly formed
    Check {},
}

pub fn handle_node_command(command: NodeCommands) {
    match command {
        NodeCommands::Create {
            to_dir,
            lang,
            node_name,
            description,
        } => {
            let current_dir = std::env::current_dir().unwrap();
            if let Err(e) = create::create(
                &current_dir,
                to_dir.as_deref(),
                &node_name,
                &lang,
                description.as_deref(),
            ) {
                eprintln!("Failed to create node: {}", e);
                std::process::exit(1);
            }
        }
        NodeCommands::List {} => {
            eprintln!("Listing nodes...");
            list::list_nodes();
        }
        NodeCommands::Check {} => {
            eprintln!("Checking nodes...");
            list::check();
        }
    }
}
