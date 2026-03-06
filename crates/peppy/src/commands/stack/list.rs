use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use core_node::encoding::NodeListRequest;
use node_stack::SerializedNodeGraph;
use tracing::info;

use crate::context::AppContext;
use crate::error::{Error, Result};

const CALLER_INSTANCE_ID: &str = "peppy-cli";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub fn list_nodes(ctx: &Arc<AppContext>, dot_graph_path: Option<PathBuf>) -> Result<()> {
    crate::commands::block_on(list_nodes_async(ctx, dot_graph_path))
}

async fn list_nodes_async(ctx: &Arc<AppContext>, dot_graph_path: Option<PathBuf>) -> Result<()> {
    let daemon_state = ctx.read_daemon_state().map_err(|e| {
        Error::ExecutionFailed(format!(
            "Failed to read daemon state. Is the peppy daemon running? Error: {}",
            e
        ))
    })?;
    let core_node_name = daemon_state.core_node_name;

    ctx.connect().await?;
    let messenger_handle = ctx
        .messenger_handle()
        .ok_or_else(|| Error::ExecutionFailed("Failed to connect to daemon".to_string()))?;

    info!(
        "Requesting node stack graph from daemon '{}'...",
        core_node_name
    );

    let response = NodeListRequest::new(dot_graph_path.is_some())
        .poll(
            messenger_handle,
            &core_node_name,
            CALLER_INSTANCE_ID,
            &core_node_name,
            REQUEST_TIMEOUT,
        )
        .await
        .map_err(|e| Error::ExecutionFailed(format!("Failed to call node_list service: {}", e)))?;

    let graph: SerializedNodeGraph = serde_json::from_str(&response.graph_json)
        .map_err(|e| Error::ExecutionFailed(format!("Failed to parse graph JSON: {}", e)))?;

    // Sort nodes by label for consistent output, with core node first
    let mut nodes = graph.nodes.clone();
    nodes.sort_by(|a, b| {
        let a_is_daemon = a.label().starts_with(&core_node_name);
        let b_is_daemon = b.label().starts_with(&core_node_name);
        match (a_is_daemon, b_is_daemon) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.label().cmp(&b.label()),
        }
    });

    info!("Node stack:");
    for node in &nodes {
        info!(
            "  - {} ({}) ({})",
            node.label(),
            node.fs_root_path,
            node.instance_info()
        );
    }

    // Sort edges by (from_label, to_label) for consistent output
    let mut edges = graph.edges.clone();
    edges.sort_by(|a, b| {
        let a_key = (a.from.label(), a.to.label());
        let b_key = (b.from.label(), b.to.label());
        a_key.cmp(&b_key)
    });

    info!("Dependencies:");
    if edges.is_empty() {
        info!("  (none)");
    } else {
        for edge in &edges {
            info!("  - {} -> {}", edge.from.label(), edge.to.label());
        }
    }

    if let (Some(path), Some(dot_graph)) = (dot_graph_path, response.dot_graph) {
        std::fs::write(&path, dot_graph).map_err(|e| {
            Error::ExecutionFailed(format!(
                "Failed to write DOT graph to {}: {}",
                path.display(),
                e
            ))
        })?;
        info!("DOT graph saved to {}", path.display());
    }
    Ok(())
}
