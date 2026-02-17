use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use config::node::NodeConfigParser;
use config::peppy_config::Name;
use config::runtime::{NodeInstance, RuntimeConfig};
use daemon_node::encoding::NodeListRequest;
use names_generator2::get_random;
use node_stack::SerializedNodeGraph;
use rand::rng;
use tracing::info;

use crate::context::AppContext;
use crate::error::{Error, Result};

use super::start::args_to_node_arguments;

const CALLER_INSTANCE_ID: &str = "peppy-cli";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Prints the contents of `PEPPY_RUNTIME_CONFIG` that would be passed to a node process.
pub fn print_runtime_config(
    ctx: &Arc<AppContext>,
    node_name: Option<String>,
    node_dir: Option<PathBuf>,
    args: Vec<(String, String)>,
) -> Result<()> {
    let node_name = resolve_node_name(node_name, node_dir)?;
    crate::commands::block_on(print_runtime_config_async(ctx, node_name, args))
}

/// Resolves the node name from either the direct name or by reading the peppy.json5 file in the directory.
fn resolve_node_name(node_name: Option<String>, node_dir: Option<PathBuf>) -> Result<String> {
    match (node_name, node_dir) {
        (Some(name), None) => Ok(name),
        (None, Some(dir)) => {
            let peppy_json5_path = dir.join("peppy.json5");
            let config =
                NodeConfigParser::from_path(&peppy_json5_path).map_err(Error::PeppyConfig)?;
            Ok(config.manifest.name.as_str().to_string())
        }
        _ => Err(Error::ExecutionFailed(
            "Exactly one of --node-name or node_dir must be provided".to_string(),
        )),
    }
}

async fn print_runtime_config_async(
    ctx: &Arc<AppContext>,
    node_name: String,
    args: Vec<(String, String)>,
) -> Result<()> {
    let daemon_state = ctx.read_daemon_state().map_err(|e| {
        Error::ExecutionFailed(format!(
            "Failed to read daemon state. Is the peppy daemon running? Error: {}",
            e
        ))
    })?;
    let daemon_node_name = daemon_state.daemon_node_name;

    ctx.connect().await?;
    let messenger_handle = ctx
        .messenger_handle()
        .ok_or_else(|| Error::ExecutionFailed("Failed to connect to daemon".to_string()))?;

    // Validate that the node is present in the node stack so the output corresponds to a runnable node.
    let response = NodeListRequest::new(false)
        .poll(
            messenger_handle,
            &daemon_node_name,
            CALLER_INSTANCE_ID,
            &daemon_node_name,
            REQUEST_TIMEOUT,
        )
        .await
        .map_err(|e| Error::ExecutionFailed(format!("Failed to call node_list service: {}", e)))?;

    let graph: SerializedNodeGraph = serde_json::from_str(&response.graph_json)
        .map_err(|e| Error::ExecutionFailed(format!("Failed to parse graph JSON: {}", e)))?;

    let matching_nodes: Vec<_> = graph.nodes.iter().filter(|n| n.name == node_name).collect();
    if matching_nodes.is_empty() {
        return Err(Error::ExecutionFailed(format!(
            "Node '{}' not found in node stack",
            node_name
        )));
    }

    if matching_nodes.len() > 1 {
        let mut tags = matching_nodes
            .iter()
            .map(|n| n.tag.as_str())
            .collect::<Vec<_>>();
        tags.sort_unstable();
        info!(
            "Node '{}' has multiple tags in the stack: {}",
            node_name,
            tags.join(", ")
        );
    }

    let (messaging_host, messaging_port) = messenger_handle
        .messaging_endpoint()
        .await
        .unwrap_or_else(|| {
            (
                config::consts::DEFAULT_MESSAGING_HOST.to_string(),
                daemon_state.messaging_port,
            )
        });

    let instance_id = get_random(rng());
    let runtime_config = RuntimeConfig::new(
        messaging_host.as_str(),
        messaging_port,
        NodeInstance {
            instance_id: Name::new(instance_id).map_err(|e| Error::PeppyConfig(e.into()))?,
            arguments: args_to_node_arguments(&args),
        },
        node_name,
        daemon_node_name,
    )
    .map_err(Error::PeppyConfig)?;

    let runtime_config_json = serde_json::to_string(&runtime_config).map_err(|e| {
        Error::ExecutionFailed(format!("Failed to serialize runtime config: {}", e))
    })?;

    info!(
        "{}={}",
        config::consts::RUNTIME_CONFIG_VAR_NAME,
        runtime_config_json
    );

    Ok(())
}
