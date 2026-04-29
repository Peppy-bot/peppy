use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use config::launcher::Name;
use config::node::NodeConfigParser;
use config::runtime::{NodeInstanceConfig, RuntimeConfig};
use core_node_api::SerializedNodeGraph;
use core_node_api::encoding::StackListRequest;
use names_generator2::get_random;
use rand::rng;
use tracing::info;

use crate::commands::CALLER_INSTANCE_ID;
use crate::context::AppContext;
use crate::error::{Error, Result};

use super::run::args_to_node_arguments;
use peppylib::core_node::transport::poll_stack_list;

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
            Ok(config.manifest_name().to_string())
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
    let conn = ctx.connect_to_daemon().await?;

    // Validate that the node is present in the node stack so the output corresponds to a runnable node.
    let response = poll_stack_list(
        &StackListRequest::new(false),
        conn.messenger,
        &conn.core_node_name,
        CALLER_INSTANCE_ID,
        &conn.core_node_name,
        REQUEST_TIMEOUT,
    )
    .await?;

    let graph: SerializedNodeGraph = serde_json::from_str(&response.graph_json)
        .map_err(|e| Error::ExecutionFailed(format!("failed to parse stack graph JSON: {e}")))?;

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

    let (messaging_host, messaging_port) = match conn.messenger.messaging_endpoint().await {
        Some(endpoint) => endpoint,
        None => (
            config::consts::DEFAULT_MESSAGING_HOST.to_string(),
            conn.messenger.messaging_port().await,
        ),
    };

    let instance_id = get_random(rng());
    let runtime_config = RuntimeConfig::new(
        messaging_host.as_str(),
        messaging_port,
        NodeInstanceConfig {
            instance_id: Name::new(instance_id).map_err(|e| Error::PeppyConfig(e.into()))?,
            arguments: args_to_node_arguments(&args),
            framework: Default::default(),
        },
        node_name,
        conn.core_node_name,
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
