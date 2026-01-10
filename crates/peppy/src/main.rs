use std::sync::Arc;

use clap::{Parser, Subcommand};
use tracing::error;
use tracing_subscriber::EnvFilter;

use config::consts::AppEnv;
use peppy::{
    commands::{Command, node, service, stack},
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
    /// Node stack related commands
    Stack {
        #[command(subcommand)]
        command: stack::StackCommands,
    },
}

#[derive(Clone, Copy)]
enum LogStyle {
    Verbose,
    Compact,
}

fn default_env_filter(default_directive: &str) -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_directive))
}

fn init_tracing(style: LogStyle) {
    let env_filter = match style {
        LogStyle::Verbose => default_env_filter("info"),
        LogStyle::Compact => default_env_filter("peppy=info"),
    };

    let builder = tracing_subscriber::fmt().with_env_filter(env_filter);

    match style {
        LogStyle::Verbose => builder.init(),
        LogStyle::Compact => builder
            .with_ansi(false)
            .without_time()
            .with_level(false)
            .with_target(false)
            .init(),
    }
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

    let app_ctx = Arc::new(AppContext::default());

    let result = match cli.command {
        Commands::Service { command } => service::ServiceCommand { command }.execute(&app_ctx),
        Commands::Node { command } => node::NodeCommand { command }.execute(&app_ctx),
        Commands::Stack { command } => stack::StackCommand { command }.execute(&app_ctx),
    };

    if let Err(e) = result {
        error!("Error: {}", e);
        std::process::exit(1);
    }
}
