use clap::{Parser, Subcommand};
use std::path::PathBuf;

use peppy::commands::{init, node, pixi, serve};

#[derive(Parser)]
#[command(name = "peppy")]
#[command(about = "A simple CLI tool")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    // Create the initial peppy.star node in the current directory and install the peppy daemon if not already present
    Init {
        /// Optional target directory (defaults to current directory)
        #[arg(long)]
        in_dir: Option<PathBuf>,
    },
    /// Node-related commands
    Node {
        #[command(subcommand)]
        command: node::NodeCommands,
    },
    /// Run the peppy service that listen to node communication, node configuration file changes and also act as a Zenoh router.
    /// This is the background service that runs with the systemd peppy service.
    Serve {
        /// Host address to bind to
        #[arg(long, default_value = "127.0.0.1")]
        host: String,

        /// Port used for the Zenoh router
        #[arg(long, default_value = "7447")]
        zenoh_port: u16,
    },
    /// Give raw access to pixi commands (e.g. peppy pixi install, peppy pixi list) while using the environment in .peppy rather than .pixi
    Pixi {
        /// Arguments to pass to the pixi CLI
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Checks that the peppy service is running correctly, that the node configuration are valid and synchronizes the node interfaces libraries
    Sync {
        /// Path to the peppy.star file (defaults to ./peppy.star)
        #[arg(default_value = "peppy.star")]
        file: PathBuf,
    },
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Init { in_dir } => {
            let current_dir = if let Some(in_dir) = in_dir {
                in_dir
            } else {
                std::env::current_dir().expect("Failed to get current directory")
            };
            init::init(&current_dir).expect("Failed to initialize peppy.star");
        }
        Commands::Serve { host, zenoh_port } => {
            println!("Launching nodes on: {}:{}", &host, zenoh_port);
            serve::handle_serve(&host, zenoh_port);
        }
        Commands::Pixi { args } => {
            pixi::execute_pixi(&args, None);
        }
        Commands::Sync { file } => {
            let current_dir = std::env::current_dir().expect("Failed to get current directory");

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
