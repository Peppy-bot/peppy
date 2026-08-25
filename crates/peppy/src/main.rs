#![deny(unsafe_code)]

use std::sync::Arc;

use clap::{Parser, Subcommand};
use tracing::error;

use daemon_config::consts::{AppEnv, PEPPY_VERSION};
use peppy::{
    commands::{Command, container, info, mcp, node, platform, repo, service, stack},
    context::AppContext,
};

mod logging;

use logging::{LogStyle, init_tracing};

#[derive(Parser)]
#[command(name = "peppy")]
#[command(about = "The Peppy cli tool")]
#[command(version = PEPPY_VERSION)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
    /// Core-node name of the daemon to target (see `peppy info`). Defaults
    /// to the local daemon. The local daemon must be running either way (its
    /// router and federation carry the traffic), and a remote daemon must
    /// use the same workspace namespace.
    #[arg(long = "core-node", global = true, value_name = "NAME", value_parser = parse_core_node_target)]
    core_node: Option<String>,
}

/// Validates a `--core-node` value at the CLI boundary through the same shared
/// validator `service serve` and `peppy_config.json5` use, so a malformed
/// target fails here with a clap error instead of becoming an unreachable
/// `target_core_node` deep in a request. `self` is refused here too: it means
/// "the daemon this command targets", which is what omitting the flag already
/// says.
fn parse_core_node_target(value: &str) -> Result<String, String> {
    daemon_config::core_node_name::CoreNodeName::new(value)
        .map(|name| name.into_string())
        .map_err(|reason| reason.to_string())
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
    /// Platform account: log in, log out, show the current identity, and list the workspace's core nodes
    Platform {
        #[command(subcommand)]
        command: platform::PlatformCommands,
    },
    /// The built-in MCP server: serve a deployment's exposures, print an exposure's catalog
    Mcp {
        #[command(subcommand)]
        command: mcp::McpCommands,
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
            } | Commands::Mcp {
                command: mcp::McpCommands::Serve
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
        Commands::Platform { command } => platform::PlatformCommand { command }.execute(&app_ctx),
        Commands::Mcp { command } => mcp::McpCommand { command }.execute(&app_ctx),
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
    fn mcp_subcommands_parse() {
        let cli = Cli::try_parse_from(["peppy", "mcp", "serve"]).expect("mcp serve parses");
        assert!(matches!(
            cli.command,
            Commands::Mcp {
                command: mcp::McpCommands::Serve
            }
        ));
        let cli = Cli::try_parse_from(["peppy", "mcp", "catalog", "camera_and_recording:v1"])
            .expect("mcp catalog parses");
        let Commands::Mcp {
            command: mcp::McpCommands::Catalog { exposure },
        } = cli.command
        else {
            panic!("expected mcp catalog");
        };
        assert_eq!(exposure, "camera_and_recording:v1");
    }

    /// The exposure check rides `--check`; asking for it on a write is a
    /// parse error, so the flag cannot be read as "index the repositories".
    #[test]
    fn repo_index_include_repositories_requires_check() {
        let cli = Cli::try_parse_from([
            "peppy",
            "repo",
            "index",
            ".",
            "--check",
            "--include-repositories",
        ])
        .expect("the flag pair parses");
        assert!(matches!(
            cli.command,
            Commands::Repo {
                command: repo::RepoCommands::Index {
                    check: true,
                    include_repositories: true,
                    ..
                }
            }
        ));
        assert!(
            Cli::try_parse_from(["peppy", "repo", "index", ".", "--include-repositories"]).is_err(),
            "--include-repositories without --check is refused"
        );
    }

    /// Exposures are published by writing the document and listing it in a
    /// launcher; there is no publication command, and the parser knows no
    /// such word.
    #[test]
    fn repo_exposure_is_not_a_command() {
        let err = Cli::try_parse_from(["peppy", "repo", "exposure", "camera.json5"])
            .err()
            .expect("an unknown subcommand is a parse error");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }

    #[test]
    fn core_node_flag_defaults_to_none() {
        let cli = Cli::try_parse_from(["peppy", "stack", "list"]).expect("plain parse");
        assert_eq!(cli.core_node, None, "absent flag targets the local daemon");
    }

    /// `--version` is answered by the parser itself, before the app context
    /// or the daemon come into play, so a freshly extracted binary on a
    /// machine with no peppy state still states which release it is.
    #[test]
    fn version_flag_prints_the_release_tag() {
        for flag in ["--version", "-V"] {
            let err = Cli::try_parse_from(["peppy", flag])
                .err()
                .expect("--version ends parsing with a DisplayVersion error");
            assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
            assert_eq!(err.to_string().trim_end(), format!("peppy {PEPPY_VERSION}"));
        }
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

    /// The group is spelled `platform`, and only `platform`. The negative half
    /// is the point: `auth` is gone outright, with no alias, hidden command, or
    /// compatibility parser to fall back on, so it must fail to parse.
    #[test]
    fn platform_subcommands_parse() {
        for args in [
            vec!["peppy", "platform", "login", "--no-browser", "--yes"],
            vec!["peppy", "platform", "login", "--api-url", "http://x:3000"],
            vec!["peppy", "platform", "logout", "-y"],
            vec!["peppy", "platform", "logout", "--api-url", "http://x:3000"],
            vec!["peppy", "platform", "whoami", "--json"],
            vec!["peppy", "platform", "whoami", "--api-url", "http://x:3000"],
        ] {
            assert!(
                Cli::try_parse_from(args.clone()).is_ok(),
                "{args:?} should parse"
            );
        }

        assert!(Cli::try_parse_from(["peppy", "auth", "whoami"]).is_err());
        assert!(Cli::try_parse_from(["peppy", "auth", "login"]).is_err());
        assert!(Cli::try_parse_from(["peppy", "auth", "logout"]).is_err());
    }

    #[test]
    fn platform_status_is_an_alias_for_whoami() {
        let cli = Cli::try_parse_from(["peppy", "platform", "status"])
            .expect("the status alias should parse");
        assert!(matches!(
            cli.command,
            Commands::Platform {
                command: platform::PlatformCommands::Whoami { .. }
            }
        ));
    }
}
