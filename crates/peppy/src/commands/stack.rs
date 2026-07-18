mod benchmark;
mod colors;
mod launch;
mod list;
pub(crate) mod table;

pub use list::{StackListReport, list_nodes_collecting};

use std::path::PathBuf;
use std::sync::Arc;

use clap::Subcommand;
use tracing::info;

use super::Command;
use super::node::DEFAULT_IDLE_TIMEOUT_SECS;
use crate::{context::AppContext, error::Error as CommandError};

#[derive(Subcommand)]
pub enum StackCommands {
    /// Launches a deployment, replacing the current node Stack
    Launch {
        /// Path to the peppy launcher configuration file
        launcher_config_path: PathBuf,
        /// Idle timeout in seconds for the node add phase (resets on git/http progress or sub-process output)
        #[arg(long, default_value_t = DEFAULT_IDLE_TIMEOUT_SECS)]
        node_add_idle_timeout_secs: u64,
        /// Idle timeout in seconds for the node build phase (resets on build_cmd output)
        #[arg(long, default_value_t = DEFAULT_IDLE_TIMEOUT_SECS)]
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

pub struct StackCommand {
    pub command: StackCommands,
}

impl Command for StackCommand {
    fn execute(self, ctx: &Arc<AppContext>) -> Result<(), CommandError> {
        match self.command {
            StackCommands::List => list::list_nodes(ctx),
            StackCommands::Launch {
                launcher_config_path,
                node_add_idle_timeout_secs,
                node_build_idle_timeout_secs,
                node_run_idle_timeout_secs,
                max_timeout_secs,
            } => {
                info!("Launching stack...");
                launch::launch(
                    ctx,
                    launcher_config_path,
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
