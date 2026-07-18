//! User-managed, network-level zenoh router federation.

mod ca;
mod federate;
mod list;
mod remove;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Subcommand;
use daemon::control as daemon_control;
use daemon_config::consts::PeppyDirs;

use super::Command;
use crate::context::AppContext;
use crate::error::{Error, Result};

/// Client-side budget for a cached daemon status query. Kept strictly larger
/// than the daemon's `STATUS_ACK_TIMEOUT` so the daemon can always reply a
/// definite status first.
const STATUS_TIMEOUT: Duration = Duration::from_secs(2);

/// The managed-mode gate shared by `federate` and `remove`: the poke read
/// timeout when this daemon manages its router, or an error because an
/// operator-run router owns federation.
fn managed_poke_read_timeout(
    dirs: &PeppyDirs,
    config: &daemon_config::peppy_config::PeppyConfig,
) -> Result<Duration> {
    let Some(connect_timeout_secs) =
        crate::commands::auth::federation_poke_timeout_secs(dirs, config)
    else {
        return Err(Error::ExecutionFailed(
            "this daemon uses an operator-run router; federation belongs to the operator"
                .to_string(),
        ));
    };
    Ok(Duration::from_secs(connect_timeout_secs) + daemon_control::POKE_READ_SLACK)
}

fn parse_peer_endpoint(value: &str) -> std::result::Result<federation::FederationPeer, String> {
    federation::FederationPeer::new(value, None).map_err(|error| error.to_string())
}

#[derive(Subcommand)]
pub enum FederationCommands {
    /// List router federations and visible core nodes
    List {
        /// Emit stable machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Federate with another Peppy daemon's mTLS router listener
    Federate {
        /// Peer router locator, for example tls/robot.example:7449
        #[arg(value_parser = parse_peer_endpoint)]
        endpoint: federation::FederationPeer,
    },
    /// Remove a federation by core-node name or exact endpoint
    Remove {
        target: String,
        /// Skip confirmation when removing platform-backend
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Create and issue Peppy fleet certificates
    Ca {
        #[command(subcommand)]
        command: CaCommands,
    },
}

#[derive(Subcommand)]
pub enum CaCommands {
    /// Initialize a fleet certificate authority
    Init,
    /// Issue a dual-use server and client identity
    Issue {
        /// DNS name or IP address to include as a certificate SAN
        #[arg(long = "host", required = true)]
        hosts: Vec<String>,
        /// Output directory; defaults to this machine's federation identity dir
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

pub struct FederationCommand {
    pub command: FederationCommands,
}

impl Command for FederationCommand {
    fn execute(self, ctx: &Arc<AppContext>) -> Result<()> {
        match self.command {
            FederationCommands::List { json } => list::ListCommand {
                json,
                peppy_dirs: None,
            }
            .execute(ctx),
            FederationCommands::Federate { endpoint } => federate::FederateCommand {
                endpoint,
                peppy_dirs: None,
            }
            .execute(ctx),
            FederationCommands::Remove { target, yes } => remove::RemoveCommand {
                target,
                yes,
                peppy_dirs: None,
            }
            .execute(ctx),
            FederationCommands::Ca { command } => match command {
                CaCommands::Init => ca::init(),
                CaCommands::Issue { hosts, out } => ca::issue(hosts, out),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::FederationCommands;

    #[derive(Parser)]
    struct TestCli {
        #[command(subcommand)]
        command: FederationCommands,
    }

    #[test]
    fn federate_rejects_non_tls_endpoints_at_parse_time() {
        assert!(TestCli::try_parse_from(["test", "federate", "tcp/peer:7449"]).is_err());
    }

    #[test]
    fn federation_command_shapes_parse() {
        for args in [
            vec!["test", "list", "--json"],
            vec!["test", "remove", "peer-a", "--yes"],
            vec![
                "test",
                "ca",
                "issue",
                "--host",
                "peer.example",
                "--host",
                "127.0.0.1",
            ],
        ] {
            TestCli::try_parse_from(args).expect("federation command parses");
        }
    }
}
