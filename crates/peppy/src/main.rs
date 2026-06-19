#![deny(unsafe_code)]

use std::sync::Arc;

use clap::{Parser, Subcommand};
use tracing::error;

use config::consts::AppEnv;
use peppy::{
    commands::{Command, container, info, login, logout, node, repo, service, stack, whoami},
    context::AppContext,
};

mod logging;

use logging::{LogStyle, init_tracing};

#[derive(Parser)]
#[command(name = "peppy")]
#[command(about = "The peppyOS cli tool")]
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
    /// Log in to Peppy via the browser (OAuth device flow)
    Login {
        /// Profile to log into (e.g. `dev`/`prod`); defaults per build.
        #[arg(long)]
        env: Option<String>,
        /// Override the backend base URL (else profile default / PEPPY_API_URL).
        #[arg(long = "api-url")]
        api_url: Option<String>,
        /// Print the verification URL/code instead of opening a browser.
        #[arg(long = "no-browser")]
        no_browser: bool,
    },
    /// Log out: revoke the access token on the backend and clear local credentials
    Logout {
        #[arg(long)]
        env: Option<String>,
        #[arg(long = "api-url")]
        api_url: Option<String>,
    },
    /// Show the current Peppy identity, profile, and token status
    #[command(visible_alias = "status")]
    Whoami {
        #[arg(long)]
        env: Option<String>,
        #[arg(long = "api-url")]
        api_url: Option<String>,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
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
    config::consts::set_app_env(env);

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
        Commands::Login {
            env,
            api_url,
            no_browser,
        } => login::LoginCommand {
            env,
            api_url,
            no_browser,
            credentials_file: None,
        }
        .execute(&app_ctx),
        Commands::Logout { env, api_url } => logout::LogoutCommand {
            env,
            api_url,
            credentials_file: None,
        }
        .execute(&app_ctx),
        Commands::Whoami { env, api_url, json } => whoami::WhoamiCommand {
            env,
            api_url,
            json,
            credentials_file: None,
        }
        .execute(&app_ctx),
        Commands::Info {} => info::InfoCommand.execute(&app_ctx),
    };

    if let Err(e) = result {
        error!("Error: {}", e);
        std::process::exit(1);
    }
}
