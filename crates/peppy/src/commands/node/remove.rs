use std::io::{self, Write};
use std::sync::Arc;
use std::time::Duration;

use master_node::encoding::{NodeListRequest, NodeRemoveRequest};
use node_stack::SerializedNodeGraph;
use tracing::info;

use crate::context::AppContext;
use crate::error::{Error, Result};

const CALLER_INSTANCE_ID: &str = "peppy-cli";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub fn remove_node(
    ctx: &Arc<AppContext>,
    node_name: String,
    tag: String,
    stop_instances: bool,
    force: bool,
) -> Result<()> {
    crate::commands::block_on(remove_node_async(
        ctx,
        node_name,
        tag,
        stop_instances,
        force,
    ))
}

async fn remove_node_async(
    ctx: &Arc<AppContext>,
    node_name: String,
    tag: String,
    stop_instances: bool,
    force: bool,
) -> Result<()> {
    let daemon_state = ctx.read_daemon_state().map_err(|e| {
        Error::ExecutionFailed(format!(
            "Failed to read daemon state. Is the peppy daemon running? Error: {}",
            e
        ))
    })?;
    let master_node_name = daemon_state.master_node_name;

    ctx.connect().await?;
    let messenger_handle = ctx
        .messenger_handle()
        .ok_or_else(|| Error::ExecutionFailed("Failed to connect to daemon".to_string()))?;

    let mut stop_instances = stop_instances;
    if force {
        stop_instances = true;
    }
    if !stop_instances {
        let instance_ids =
            fetch_instance_ids(messenger_handle, &master_node_name, &node_name, &tag).await?;

        if !instance_ids.is_empty() {
            let confirm = confirm_removal(&node_name, &tag, &instance_ids)?;
            if !confirm {
                return Err(Error::ExecutionFailed(
                    "Node removal aborted by user".to_string(),
                ));
            }
            stop_instances = true;
        }
    }

    info!(
        "Calling node_remove for '{}:{}' on master '{}' (stop_instances={})...",
        node_name, tag, master_node_name, stop_instances
    );

    let remove_request =
        NodeRemoveRequest::new(&node_name, &tag).with_stop_instances(stop_instances);
    let remove_response = remove_request
        .poll(
            messenger_handle,
            &master_node_name,
            CALLER_INSTANCE_ID,
            &master_node_name,
            REQUEST_TIMEOUT,
        )
        .await
        .map_err(|e| {
            Error::ExecutionFailed(format!("Failed to call node_remove service: {}", e))
        })?;

    if !remove_response.success {
        return Err(Error::ExecutionFailed(
            remove_response
                .error_message
                .unwrap_or_else(|| "node_remove failed with no error message".to_string()),
        ));
    }

    info!("Removed node '{}:{}'", node_name, tag);
    Ok(())
}

async fn fetch_instance_ids(
    messenger: &peppylib::MessengerHandle,
    master_node_name: &str,
    node_name: &str,
    tag: &str,
) -> Result<Vec<String>> {
    let response = NodeListRequest::new(false)
        .poll(
            messenger,
            master_node_name,
            CALLER_INSTANCE_ID,
            master_node_name,
            REQUEST_TIMEOUT,
        )
        .await
        .map_err(|e| {
            Error::ExecutionFailed(format!(
                "Failed to check running instances before removal: {}",
                e
            ))
        })?;

    let graph: SerializedNodeGraph = serde_json::from_str(&response.graph_json)
        .map_err(|e| Error::ExecutionFailed(format!("Failed to parse graph JSON: {}", e)))?;

    Ok(graph
        .nodes
        .into_iter()
        .find(|node| node.name == node_name && node.tag == tag)
        .map(|node| node.instance_ids)
        .unwrap_or_default())
}

fn confirm_removal(node_name: &str, tag: &str, instance_ids: &[String]) -> Result<bool> {
    let count = instance_ids.len();
    let suffix = if count == 1 { "instance" } else { "instances" };
    let verb = if count == 1 { "is" } else { "are" };
    let ids = instance_ids
        .iter()
        .map(|id| format!("\"{}\"", id))
        .collect::<Vec<_>>()
        .join(", ");

    print!(
        "Are you sure you want to remove `{}:{}`? {} {} ({}) {} still running [y/n] ",
        node_name, tag, count, suffix, ids, verb
    );
    io::stdout().flush().map_err(|e| {
        Error::ExecutionFailed(format!("Failed to write confirmation prompt: {}", e))
    })?;

    let mut input = String::new();
    io::stdin().read_line(&mut input).map_err(|e| {
        Error::ExecutionFailed(format!("Failed to read confirmation response: {}", e))
    })?;

    let response = input.trim().to_ascii_lowercase();
    Ok(matches!(response.as_str(), "y" | "yes"))
}
