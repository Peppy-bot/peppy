#![deny(unsafe_code)]

use std::sync::Arc;

use clap::{Parser, Subcommand};
use tracing::error;

use daemon_config::consts::AppEnv;
use peppy::{
    commands::{Command, auth, container, info, node, repo, service, stack},
    context::AppContext,
};

mod logging;

use logging::{LogStyle, init_tracing};

#[derive(Parser)]
#[command(name = "peppy")]
#[command(about = "The Peppy cli tool")]
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
    /// Node stack related commands
    Stack {
        #[command(subcommand)]
        command: stack::StackCommands,
    },
    /// Container runtime setup and status
    Container {
        #[command(subcommand)]
        command: container::ContainerCommands,
    },
    /// Manage node repositories
    #[command(visible_alias = "repositories")]
    Repo {
        #[command(subcommand)]
        command: repo::RepoCommands,
    },
    /// Authentication: log in, log out, and show the current identity
    Auth {
        #[command(subcommand)]
        command: auth::AuthCommands,
    },
    /// Display peppy version information
    Info {},
}

fn main() {
    // Set app env based on build profile (release = Prod, debug = Dev)
    let env = if cfg!(debug_assertions) {
        AppEnv::Dev
    } else {
        AppEnv::Prod
    };
    daemon_config::consts::set_app_env(env);

    let cli = Cli::parse();
    let log_style = if cfg!(debug_assertions)
        || matches!(
            &cli.command,
            Commands::Service {
                command: service::ServiceCommands::Serve { .. }
            }
        ) {
        LogStyle::Verbose
    } else {
        LogStyle::Compact
    };
    init_tracing(log_style);

    let app_ctx = match AppContext::from_current_dir() {
        Ok(ctx) => Arc::new(ctx),
        Err(e) => {
            error!("Error: {}", e);
            std::process::exit(1);
        }
    };

    let result = match cli.command {
        Commands::Service { command } => service::ServiceCommand { command }.execute(&app_ctx),
        Commands::Node { command } => node::NodeCommand { command }.execute(&app_ctx),
        Commands::Stack { command } => stack::StackCommand { command }.execute(&app_ctx),
        Commands::Container { command } => {
            container::ContainerCommand { command }.execute(&app_ctx)
        }
        Commands::Repo { command } => repo::RepoCommand { command }.execute(&app_ctx),
        Commands::Auth { command } => auth::AuthCommand { command }.execute(&app_ctx),
        Commands::Info {} => info::InfoCommand.execute(&app_ctx),
    };

    if let Err(e) = result {
        error!("Error: {}", e);
        std::process::exit(1);
    }
}
