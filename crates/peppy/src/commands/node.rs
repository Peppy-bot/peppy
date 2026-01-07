mod add;
mod init;
mod remove;
mod run;
mod runtime_config;
mod stop;
mod sync;
mod types;

use std::path::PathBuf;
use std::sync::Arc;

use clap::{ArgGroup, Subcommand};
use config::peppy_config::BuildSystem;
use tracing::info;

use super::Command;
use crate::{context::AppContext, error::Error as CommandError};

use init::NodeBuilder;

pub use types::NodeName;

/// Parses a key=value argument string into a tuple
fn parse_key_value_arg(s: &str) -> Result<(String, String), String> {
    let pos = s
        .find('=')
        .ok_or_else(|| format!("invalid argument format '{}': expected key=value", s))?;
    let key = s[..pos].trim().to_string();
    let value = s[pos + 1..].trim().to_string();
    if key.is_empty() {
        return Err(format!("invalid argument '{}': key cannot be empty", s));
    }
    Ok((key, value))
}

#[derive(Subcommand)]
pub enum NodeCommands {
    /// Create a new peppy node
    #[command(visible_alias = "create")]
    Init {
        /// Name of the node directory to create
        node_name: NodeName,
        /// Optional: target directory (defaults to current directory)
        #[arg(long)]
        to_dir: Option<PathBuf>,
        /// Build system for the node: `rust`, `python`, `cargo`, or `uv`
        #[arg(long, visible_alias = "lang", value_enum, default_value_t = BuildSystem::Rust)]
        build_system: BuildSystem,
    },
    /// Add a node to the node stack based on its peppy.json5 file
    Add {
        /// Path to the node configuration file
        #[arg(long)]
        peppy_json5: PathBuf,
        /// If set, will attempt to spawn an instance directly after adding the node to the node stack
        #[arg(long)]
        run: bool,
        /// Runtime arguments as key=value pairs (e.g., resolution=1280x720 frequency=30)
        /// These are passed to the node via PEPPY_RUNTIME_CONFIG when run is true
        #[arg(value_parser = parse_key_value_arg)]
        args: Vec<(String, String)>,
        /// Optional: specify a deterministic instance ID
        #[arg(long, hide = true)]
        instance_id: Option<String>,
    },
    /// Regenerate the node's interface code (peppygen) based on peppy.json5
    Sync {
        /// Build system for the node: `rust`, `python`, `cargo`, or `uv`
        #[arg(long, visible_alias = "lang", value_enum, default_value_t = BuildSystem::Rust)]
        build_system: BuildSystem,
    },
    /// Runs an instance from a node added to the node stack
    Run {
        /// Name of the node to spawn
        #[arg(long)]
        node_name: String, // Finds the `NodeConfig` in the node stack that matches this name
        /// Tag of the node
        #[arg(long)]
        tag: String,
        /// Runtime arguments as key=value pairs (e.g., resolution=1280x720 frequency=30)
        /// These are passed to the node via PEPPY_RUNTIME_CONFIG
        #[arg(value_parser = parse_key_value_arg)]
        args: Vec<(String, String)>,
        /// Optional: specify a deterministic instance ID
        #[arg(long, hide = true)]
        instance_id: Option<String>,
    },
    /// Prints out the runtime config of a node instance
    #[command(group(ArgGroup::new("node_source").required(true).args(["node_name", "peppy_json5"])))]
    RuntimeConfig {
        /// Name of the node
        #[arg(long)]
        node_name: Option<String>,
        /// Path to the node configuration file
        #[arg(long)]
        peppy_json5: Option<PathBuf>,
    },
    /// Stop a running node instance
    Stop {
        /// Instance ID of the node to stop
        #[arg(long)]
        instance_id: String,
    },
    /// Remove a node from the node stack
    Remove {
        /// Name of the node to remove
        #[arg(long)]
        node_name: NodeName,
        /// When set, stop all instances running on this node before removing the node itself. Without this flag, the command fails if the node has running instances
        #[arg(long)]
        stop_instances: bool,
    },
}

pub struct NodeCommand {
    pub command: NodeCommands,
}

impl Command for NodeCommand {
    fn execute(self, ctx: &Arc<AppContext>) -> Result<(), CommandError> {
        match self.command {
            NodeCommands::Init {
                to_dir,
                build_system,
                node_name,
            } => {
                let mut node_builder = NodeBuilder::new(ctx, node_name).build_system(build_system);

                if let Some(dir) = to_dir {
                    node_builder = node_builder.to_dir(dir);
                }

                node_builder.build()
            }
            NodeCommands::Add {
                peppy_json5,
                run,
                args,
                instance_id,
            } => {
                info!("Adding node {}...", peppy_json5.display());
                add::add_node(ctx, peppy_json5, run, args, instance_id)
            }
            NodeCommands::Sync { build_system } => {
                info!("Syncing node interfaces...");
                sync::sync_node(ctx, build_system)
            }
            NodeCommands::Run {
                node_name,
                tag,
                args,
                instance_id,
            } => {
                info!("Running node {}:{}...", node_name, tag);
                run::run_node(ctx, node_name, tag, args, instance_id)
            }
            NodeCommands::RuntimeConfig {
                node_name,
                peppy_json5,
            } => {
                info!("Printing runtime config...");
                runtime_config::print_runtime_config(ctx, node_name, peppy_json5)
            }
            NodeCommands::Stop { instance_id } => {
                info!("Stopping node instance {}...", instance_id);
                stop::stop_node(ctx, instance_id)
            }
            NodeCommands::Remove {
                node_name,
                stop_instances,
            } => {
                info!("Remove node {}...", node_name.as_str());
                remove::remove_node(ctx, node_name, stop_instances)
            }
        }
    }
}
