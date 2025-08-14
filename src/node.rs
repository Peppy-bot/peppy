use clap::Subcommand;
use std::path::PathBuf;

pub mod create;
pub mod list;

#[derive(Subcommand)]
pub enum NodeCommands {
    /// Create a new peppy node
    Create {
        /// Name of the node directory to create
        node_name: String,
        /// Programming language for the node, either `rust` or `python`. Defaults to `python`
        #[arg(long, default_value = "rust")]
        lang: String,
        /// Optional target directory (defaults to current directory)
        #[arg(long)]
        to_dir: Option<PathBuf>,
    },
    /// List nodes in the current system
    List {},
    /// Check that the root peppy.star node and its children are properly formed
    Check {},
}

pub fn handle_node_command(command: NodeCommands) {
    match command {
        NodeCommands::Create {
            node_name,
            lang,
            to_dir,
        } => {
            if let Err(e) = create::create(&node_name, &lang, to_dir) {
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
