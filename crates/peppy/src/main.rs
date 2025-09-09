use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tracing::error;

use config::consts::AppEnv;
use peppy::{Command, init, node, pixi, serve, service};

#[derive(Parser)]
#[command(name = "peppy")]
#[command(about = "A simple CLI tool")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    // Create the initial peppy.json5 node in the current directory and install the peppy daemon if not already present
    Init {
        /// Name of the node to initialize
        node_name: String,
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
        /// Messaging engine to use (zenoh by default)
        #[arg(long, default_value = "zenoh")]
        engine: String,

        /// Config file(s) for the selected engine. Will use a default configuration if not provided
        #[arg(long, default_value = None)]
        config_path: Option<PathBuf>,
    },
    /// Give raw access to pixi commands (e.g. peppy pixi install, peppy pixi list) while using the environment in .peppy rather than .pixi
    Pixi {
        /// Arguments to pass to the pixi CLI
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Commands related to the peppy service (running in `dev` or in systemd/launchctl)
    Service {
        #[command(subcommand)]
        command: service::ServiceCommands,
    },
}

fn main() {
    // Set app env based on PEPPY_ENV environment variable
    let env = if std::env::var("PEPPY_ENV").unwrap_or_default() == "PROD" {
        AppEnv::Prod
    } else {
        AppEnv::Dev
    };
    config::consts::set_app_env(env);

    // Initialize tracing subscriber with environment filter
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Init { node_name, in_dir } => init::InitCommand { node_name, in_dir }.execute(),
        Commands::Serve {
            engine,
            config_path,
        } => serve::ServeCommand {
            engine,
            config_path,
        }
        .execute(),
        Commands::Pixi { args } => pixi::PixiCommand { args }.execute(),
        Commands::Service { command } => service::ServiceCommand { command }.execute(),
        Commands::Node { command } => node::NodeCommand { command }.execute(),
    };

    if let Err(e) = result {
        error!("Error: {}", e);
        std::process::exit(1);
    }
}
