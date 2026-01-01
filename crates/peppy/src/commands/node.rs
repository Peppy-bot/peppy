mod add;
mod init;
mod list;
mod run;
mod types;

use std::path::PathBuf;
use std::sync::Arc;

use clap::Subcommand;
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
    /// List nodes in the current node stack
    List {
        /// If specified, will save a dotgraph representation at the given path
        dot_graph_path: Option<PathBuf>,
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
            NodeCommands::Run {
                node_name,
                tag,
                args,
                instance_id,
            } => {
                info!("Running node {}:{}...", node_name, tag);
                run::run_node(ctx, node_name, tag, args, instance_id)
            }
            NodeCommands::List { dot_graph_path } => {
                info!("Listing nodes...");
                list::list_nodes(ctx, dot_graph_path)
            }
        }
    }
}
