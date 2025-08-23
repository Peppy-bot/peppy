use clap::{Parser, Subcommand};
use std::path::PathBuf;

use peppy::commands::{
    Command,
    init::InitCommand,
    node::{NodeCommand, NodeCommands},
    pixi::PixiCommand,
    serve::ServeCommand,
    sync::SyncCommand,
};

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
        command: NodeCommands,
    },
    /// Run the peppy service that listen to node communication, node configuration file changes and also act as a Zenoh router.
    /// This is the background service that runs with the systemd peppy service.
    Serve {
        /// Messaging engine to use (zenoh by default)
        #[arg(long, default_value = "zenoh")]
        engine: String,

        /// Host address to bind to
        #[arg(long, default_value = "127.0.0.1")]
        host: Option<String>,

        /// Port used for the messaging router
        #[arg(long, default_value = "7447")]
        port: Option<u16>,
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

    let result = match cli.command {
        Commands::Init { in_dir } => InitCommand { in_dir }.execute(),
        Commands::Serve { engine, host, port } => ServeCommand { engine, host, port }.execute(),
        Commands::Pixi { args } => PixiCommand { args }.execute(),
        Commands::Sync { file } => SyncCommand { file }.execute(),
        Commands::Node { command } => NodeCommand { command }.execute(),
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
