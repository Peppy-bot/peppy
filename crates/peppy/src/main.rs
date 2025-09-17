use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tracing::error;

use config::consts::AppEnv;
use peppy::{AppContext, Command, PEPPY_CONFIG_FILE, init, node, serve, service};

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
        #[arg(long)]
        config_path: Option<PathBuf>,

        /// Is set to `false` in dev mode and `true` in prod by default.
        /// Use `--strict` to force strict mode on, or `--not-strict` to force it off.
        /// This flag ensures that every `subscribes_to` section of the root node and its children are mapping to the specified nodes.
        /// If the nodes are missing, the `serve` command will stop with an error explaining the missing dependencies.
        #[arg(long)]
        strict: bool,

        /// Explicitly disable strict mode (overrides the default from env)
        #[arg(long = "not-strict", conflicts_with = "strict")]
        not_strict: bool,
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
        Commands::Init { node_name, in_dir } => {
            init::InitCommand { node_name, in_dir }.execute(&app_ctx)
        }
        Commands::Serve {
            engine,
            config_path,
            strict,
            not_strict,
        } => {
            let config_path = match config_path {
                Some(pth) => pth,
                None => std::env::current_dir().unwrap().join(PEPPY_CONFIG_FILE),
            };
            let strict = if strict {
                true
            } else if not_strict {
                false
            } else {
                match env {
                    AppEnv::Prod => true,
                    AppEnv::Dev => false,
                }
            };
            serve::ServeCommand {
                engine,
                root_config_path: config_path,
                strict,
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
