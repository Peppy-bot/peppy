use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use tracing::error;

use config::consts::AppEnv;
use peppy::{
    commands::{Command, launch, node, service},
    context::AppContext,
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
    /// Related to the peppy service (running in systemd/launchctl)
    Service {
        #[command(subcommand)]
        command: service::ServiceCommands,
    },
    /// Node-related commands
    Node {
        #[command(subcommand)]
        command: node::NodeCommands,
    },
    /// Launches a deployment, replacing the current node Stack
    Launch {
        /// Path to the peppy launcher configuration file
        launch_file: PathBuf,
    },
}

fn main() {
    // Set app env based on build profile (release = Prod, debug = Dev)
    let env = if cfg!(debug_assertions) {
        AppEnv::Dev
    } else {
        AppEnv::Prod
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
    let app_ctx = Arc::new(AppContext::default());

    let result = match cli.command {
        Commands::Service { command } => service::ServiceCommand { command }.execute(&app_ctx),
        Commands::Node { command } => node::NodeCommand { command }.execute(&app_ctx),
        Commands::Launch { launch_file } => launch::LaunchCommand {
            launcher_config_path: launch_file,
        }
        .execute(&app_ctx),
    };

    if let Err(e) = result {
        error!("Error: {}", e);
        std::process::exit(1);
    }
}
