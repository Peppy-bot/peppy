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
    /// Core-node name of the daemon to target (see `peppy info`). Defaults
    /// to the local daemon. The local daemon must be running either way (its
    /// router and federation carry the traffic), and a remote daemon must
    /// belong to the same organization.
    #[arg(long = "core-node", global = true, value_name = "NAME", value_parser = parse_core_node_target)]
    core_node: Option<String>,
}

/// Validates a `--core-node` value at the CLI boundary with the same `Name`
/// rules (and length cap) `service serve` applies to `--core-node-name`, so a
/// malformed target fails here with a clap error instead of becoming an
/// unreachable `target_core_node` deep in a request.
fn parse_core_node_target(value: &str) -> Result<String, String> {
    if config::runtime::Name::new(value).is_err()
        || value.len() > daemon_config::peppy_config::MAX_CORE_NODE_NAME_LEN
    {
        return Err(format!(
            "must be non-empty, at most {} characters, and use only characters from \"{}\"",
            daemon_config::peppy_config::MAX_CORE_NODE_NAME_LEN,
            config::consts::ALLOWED_CONFIG_CHARS
        ));
    }
    Ok(value.to_string())
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
        Ok(ctx) => Arc::new(ctx.with_core_node_override(cli.core_node)),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_node_flag_parses_after_the_subcommand() {
        // `global = true` is what lets the root flag ride after the
        // subcommand, the way users type it.
        let cli = Cli::try_parse_from(["peppy", "stack", "list", "--core-node", "robot-7"])
            .expect("global --core-node should parse after the subcommand");
        assert_eq!(cli.core_node.as_deref(), Some("robot-7"));
        assert!(matches!(
            cli.command,
            Commands::Stack {
                command: stack::StackCommands::List
            }
        ));
    }

    #[test]
    fn core_node_flag_parses_before_the_subcommand() {
        let cli = Cli::try_parse_from(["peppy", "--core-node", "robot-7", "info"])
            .expect("--core-node should parse at the root position too");
        assert_eq!(cli.core_node.as_deref(), Some("robot-7"));
        assert!(matches!(cli.command, Commands::Info {}));
    }

    #[test]
    fn core_node_flag_defaults_to_none() {
        let cli = Cli::try_parse_from(["peppy", "stack", "list"]).expect("plain parse");
        assert_eq!(cli.core_node, None, "absent flag targets the local daemon");
    }

    /// A malformed `--core-node` must be rejected at parse time — the same
    /// `Name` rules `service serve --core-node-name` enforces — rather than
    /// silently becoming an unreachable request target.
    #[test]
    fn core_node_flag_rejects_invalid_names() {
        let long = "x".repeat(daemon_config::peppy_config::MAX_CORE_NODE_NAME_LEN + 1);
        for bad in ["", "   ", "has space", "robot/7", long.as_str()] {
            assert!(
                Cli::try_parse_from(["peppy", "stack", "list", "--core-node", bad]).is_err(),
                "--core-node {bad:?} should be rejected"
            );
        }
    }
}
