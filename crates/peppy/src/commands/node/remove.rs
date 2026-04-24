use std::sync::Arc;
use std::time::Duration;

use core_node_api::encoding::{NodeRemoveRequest, StackListRequest};
use tracing::info;

use crate::commands::CALLER_INSTANCE_ID;
use crate::context::AppContext;
use crate::error::{Error, Result};

use peppylib::core_node::stack::stack_list;
use peppylib::core_node::transport::poll_node_remove;
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
    let conn = ctx.connect_to_daemon().await?;

    let mut stop_instances = stop_instances;
    if force {
        stop_instances = true;
    }
    if !stop_instances {
        let instance_ids =
            fetch_instance_ids(conn.messenger, &conn.core_node_name, &node_name, &tag).await?;

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
        "Calling node_remove for '{}:{}' on daemon '{}' (stop_instances={})...",
        node_name, tag, conn.core_node_name, stop_instances
    );

    let remove_request =
        NodeRemoveRequest::new(&node_name, &tag).with_stop_instances(stop_instances);
    let remove_response = poll_node_remove(
        &remove_request,
        conn.messenger,
        &conn.core_node_name,
        CALLER_INSTANCE_ID,
        &conn.core_node_name,
        REQUEST_TIMEOUT,
    )
    .await
    .map_err(|e| Error::ExecutionFailed(format!("Failed to call node_remove service: {}", e)))?;

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
    core_node_name: &str,
    node_name: &str,
    tag: &str,
) -> Result<Vec<String>> {
    let list = stack_list(
        &StackListRequest::new(false),
        messenger,
        core_node_name,
        CALLER_INSTANCE_ID,
        core_node_name,
        REQUEST_TIMEOUT,
    )
    .await?;

    Ok(list
        .graph
        .nodes
        .iter()
        .find(|node| node.name == node_name && node.tag == tag)
        .map(|node| {
            node.running_instance_ids()
                .into_iter()
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default())
}

fn confirm_removal(node_name: &str, tag: &str, instance_ids: &[String]) -> Result<bool> {
    use crate::commands::confirm::{confirm_prompt, format_instance_ids};

    let count = instance_ids.len();
    let suffix = if count == 1 { "instance" } else { "instances" };
    let verb = if count == 1 { "is" } else { "are" };
    let ids = format_instance_ids(instance_ids);

    let message = format!(
        "Are you sure you want to remove `{node_name}:{tag}`? \
         {count} {suffix} ({ids}) {verb} still running [y/n] ",
    );

    confirm_prompt(&message, None)
}
