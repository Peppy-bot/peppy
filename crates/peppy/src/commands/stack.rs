mod benchmark;
mod colors;
mod launch;
mod list;
mod reset;
mod table;

pub use list::{StackListReport, list_nodes_collecting};

use std::path::PathBuf;
use std::sync::Arc;

use clap::Subcommand;
use tracing::info;

use super::Command;
use super::node::{DEFAULT_BUILD_IDLE_TIMEOUT_SECS, DEFAULT_IDLE_TIMEOUT_SECS};
use crate::{context::AppContext, error::Error as CommandError};

#[derive(Subcommand)]
pub enum StackCommands {
    /// Launches a deployment, replacing the current node Stack
    Launch {
        /// Path to the peppy launcher configuration file
        launcher_config_path: PathBuf,
        /// Wire one of the launcher's declared `core_nodes` placeholders to a
        /// real federated core node: `<core-node-link>@<core-node>`. Repeatable,
        /// once per declared link. `@self` targets the daemon this command is
        /// sent to.
        ///
        /// Deliberately NOT spelled `--link`: `node run --link` wires a slot to
        /// a producer, while this wires a placeholder to a machine. The launcher
        /// file keeps them apart by position (`core_nodes` vs per-instance
        /// `links`), which a flag cannot do.
        #[arg(long = "place", value_name = "CORE_NODE_LINK@CORE_NODE", value_parser = parse_place_kv)]
        place: Vec<(String, String)>,
        /// Wire every declared core node link to this daemon, so a
        /// multi-machine launcher runs unmodified on one box. How you develop
        /// against a federated topology with no second machine.
        #[arg(long)]
        local: bool,
        /// Idle timeout in seconds for the node add phase (resets on git/http progress or sub-process output)
        #[arg(long, default_value_t = DEFAULT_IDLE_TIMEOUT_SECS)]
        node_add_idle_timeout_secs: u64,
        /// Idle timeout in seconds for the node build phase (resets on build output or
        /// image-download/write progress)
        #[arg(long, default_value_t = DEFAULT_BUILD_IDLE_TIMEOUT_SECS)]
        node_build_idle_timeout_secs: u64,
        /// Idle timeout in seconds for the node run-startup phase (resets on subprocess output until the node signals ready)
        #[arg(long, default_value_t = DEFAULT_IDLE_TIMEOUT_SECS)]
        node_run_idle_timeout_secs: u64,
        /// Optional absolute max timeout in seconds for the entire launch. If unset, only idle timeouts apply.
        #[arg(long)]
        max_timeout_secs: Option<u64>,
    },
    /// List the nodes in the current node stack
    List,
    /// Tear the node stack down to an empty state.
    ///
    /// Clears the targeted daemon alone: its stack slice, and any federated
    /// reservation holding the machine (`--core-node` picks a remote daemon;
    /// the default is the local one). When the target holds one slice of a
    /// launch the rest of the system is still running, says so.
    ///
    /// With `--federated`, also tears down every other machine that launch
    /// holds. Participants are REDISCOVERED by query rather than remembered:
    /// keyed on the launch the target's slice names, or on the target's own
    /// coordinator name when its slice is gone, so it works after a daemon
    /// restart and finds machines held by a reservation alone.
    Reset {
        /// Tear down every slice of the launch, from whichever machine this is
        /// run on, rather than just this daemon's.
        #[arg(long)]
        federated: bool,
    },
    /// Benchmark the latency of every interface wiring each node to its direct
    /// dependencies, measured against the already-running stack.
    ///
    /// Service/action numbers are real-payload-sized messaging round-trips (the
    /// user handler is never invoked); topic numbers are real producer→consumer
    /// delivery latency on live traffic (exact on a single host; cross-host needs
    /// PTP/NTP). Benchmarking never triggers a real handler or creates a goal.
    Benchmark {
        /// Timed samples per interface (after warmup).
        #[arg(long, default_value_t = 200)]
        samples: u32,
        /// Warmup samples per interface, discarded before measuring.
        #[arg(long, default_value_t = 20)]
        warmup: u32,
        /// Per-sample probe/observe timeout in milliseconds.
        #[arg(long, default_value_t = 2000)]
        per_sample_timeout_ms: u64,
    },
}

/// Parses `<core-node-link>@<core-node>`, sharing the `KEY@TARGET` grammar with
/// `node run --link` so both flags split the same way and report the same shape
/// of error. Only the grammar is shared; what each side means is not.
fn parse_place_kv(raw: &str) -> Result<(String, String), String> {
    crate::commands::node::parse_key_at_target(raw, "--place", "CORE_NODE_LINK@CORE_NODE")
}

pub struct StackCommand {
    pub command: StackCommands,
}

impl Command for StackCommand {
    fn execute(self, ctx: &Arc<AppContext>) -> Result<(), CommandError> {
        match self.command {
            StackCommands::List => list::list_nodes(ctx),
            StackCommands::Reset { federated } => reset::reset_stack(ctx, federated),
            StackCommands::Launch {
                launcher_config_path,
                place,
                local,
                node_add_idle_timeout_secs,
                node_build_idle_timeout_secs,
                node_run_idle_timeout_secs,
                max_timeout_secs,
            } => {
                info!("Launching stack...");
                launch::launch(
                    ctx,
                    launcher_config_path,
                    launch::PlacementArgs {
                        places: place,
                        local,
                    },
                    node_add_idle_timeout_secs,
                    node_build_idle_timeout_secs,
                    node_run_idle_timeout_secs,
                    max_timeout_secs,
                )
            }
            StackCommands::Benchmark {
                samples,
                warmup,
                per_sample_timeout_ms,
            } => {
                info!("Benchmarking stack...");
                benchmark::benchmark(ctx, samples, warmup, per_sample_timeout_ms)
            }
        }
    }
}
