use clap::{Parser, Subcommand};
use std::process::Command;
use std::path::PathBuf;

mod node;

#[derive(Parser)]
#[command(name = "peppy")]
#[command(about = "A simple CLI tool")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Node-related commands
    Node {
        #[command(subcommand)]
        command: node::NodeCommands,
    },
    /// Run the peppy service that listen to nodes, also used as a Zenoh router
    Serve {
        #[arg(short, long)]
        name: String,
    },
    /// Run pixi commands directly (e.g. peppy pixi install, peppy pixi list)
    Pixi {
        /// Arguments to pass to the pixi CLI
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Syncs a peppy.star file as the root file for nodes definition
    Sync {
        /// Path to the peppy.star file (defaults to ./peppy.star)
        #[arg(default_value = "peppy.star")]
        file: PathBuf,
    }
}

fn main() {
    let cli = Cli::parse();
    
    match cli.command {
        Commands::Serve { name } => {
            println!("Launching nodes with: {}", name);
        }
        Commands::Pixi { args } => {
            let status = Command::new("pixi")
                .args(&args)
                .status()
                .unwrap_or_else(|e| {
                    eprintln!("Failed to execute pixi: {}", e);
                    std::process::exit(1);
                });
            
            if !status.success() {
                std::process::exit(status.code().unwrap_or(1));
            }
        }
        Commands::Sync { file } => {
            let current_dir = std::env::current_dir()
                .expect("Failed to get current directory");
            
            let full_path = if file.is_relative() {
                current_dir.join(file.strip_prefix("./").unwrap_or(&file))
            } else {
                file
            };
            
            println!("Syncing file: {}", full_path.display());
            // TODO: Implement the actual sync logic here
        }
        Commands::Node { command } => {
            node::handle_node_command(command);
        }
    }
}
