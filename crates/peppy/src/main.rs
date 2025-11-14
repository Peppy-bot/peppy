use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tracing::error;

use config::consts::AppEnv;
use peppy::{AppContext, Command, PEPPY_NODE_CONFIG_FILE, install, node, serve, service};

#[derive(Parser)]
#[command(name = "peppy")]
#[command(about = "A simple CLI tool")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    // Install the peppy daemon system-wide
    Install {
        /// Name of the node to initialize
        node_name: String,
        /// Optional target directory (defaults to current directory)
        #[arg(long)]
        in_dir: Option<PathBuf>,
        /// If provided, the daemon will use this launcher file to fire up the nodes
        #[arg(long)]
        launcher_file: Option<PathBuf>,
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

        /// Config file(s) for the selected messaging engine. Will use a default configuration if not provided
        #[arg(long)]
        config_path: Option<PathBuf>,
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
    let app_ctx = AppContext::default();

    let result = match cli.command {
        Commands::Install {
            node_name,
            in_dir,
            launcher_file,
        } => install::InstallCommand {
            node_name,
            in_dir,
            launcher_file,
        }
        .execute(&app_ctx),
        Commands::Serve {
            engine,
            config_path,
        } => {
            let config_path = match config_path {
                Some(pth) => pth,
                None => app_ctx.root_dir.join(PEPPY_NODE_CONFIG_FILE),
            };
            serve::ServeCommand {
                engine,
                root_config_path: config_path,
            }
            .execute(&app_ctx)
        }
        Commands::Service { command } => service::ServiceCommand { command }.execute(&app_ctx),
        Commands::Node { command } => node::NodeCommand { command }.execute(&app_ctx),
    };

    if let Err(e) = result {
        error!("Error: {}", e);
        std::process::exit(1);
    }
}
