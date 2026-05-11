use std::sync::Arc;
use std::time::Duration;

use core_node_api::SerializedNodeGraph;
use core_node_api::encoding::{NodeRemoveRequest, StackListRequest};
use tracing::info;

use crate::commands::CALLER_INSTANCE_ID;
use crate::context::AppContext;
use crate::error::{Error, Result};

use peppylib::core_node::transport::{poll_node_remove, poll_stack_list};
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub fn remove_node(
    ctx: &Arc<AppContext>,
    node_name: String,
    tag: String,
    variant: Option<String>,
    stop_instances: bool,
    force: bool,
) -> Result<()> {
    crate::commands::block_on(remove_node_async(
        ctx,
        node_name,
        tag,
        variant,
        stop_instances,
        force,
    ))
}

async fn remove_node_async(
    ctx: &Arc<AppContext>,
    node_name: String,
    tag: String,
    variant: Option<String>,
    stop_instances: bool,
    force: bool,
) -> Result<()> {
    let conn = ctx.connect_to_daemon().await?;

    // Resolve the variant: if the caller didn't specify one, fetch the
    // stack and disambiguate. A bare `name:tag` is allowed only when
    // exactly one variant is present; otherwise we error with the list of
    // available variants so the user can re-run with the explicit
    // `--variant <name>` (or `name:tag@variant`) form.
    let resolved_variant = match variant {
        Some(v) => v,
        None => {
            resolve_single_variant(conn.messenger, &conn.core_node_name, &node_name, &tag).await?
        }
    };

    let mut stop_instances = stop_instances;
    if force {
        stop_instances = true;
    }
    if !stop_instances {
        let instance_ids = fetch_instance_ids(
            conn.messenger,
            &conn.core_node_name,
            &node_name,
            &tag,
            &resolved_variant,
        )
        .await?;

        if !instance_ids.is_empty() {
            let confirm = confirm_removal(&node_name, &tag, &resolved_variant, &instance_ids)?;
            if !confirm {
                return Err(Error::ExecutionFailed(
                    "Node removal aborted by user".to_string(),
                ));
            }
            stop_instances = true;
        }
    }

    info!(
        "Calling node_remove for `{}` on daemon `{}` (stop_instances={})...",
        format_label(&node_name, &tag, &resolved_variant),
        conn.core_node_name,
        stop_instances
    );

    let remove_request = NodeRemoveRequest::new(&node_name, &tag)
        .with_variant(&resolved_variant)
        .with_stop_instances(stop_instances);
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

    info!(
        "Removed node `{}`",
        format_label(&node_name, &tag, &resolved_variant)
    );
    Ok(())
}

/// Renders `name:tag` for the default variant, `name:tag@variant` otherwise.
fn format_label(node_name: &str, tag: &str, variant: &str) -> String {
    if variant == config::runtime::DEFAULT_VARIANT {
        format!("{node_name}:{tag}")
    } else {
        format!("{node_name}:{tag}@{variant}")
    }
}

/// Lists the variant labels present in the stack for `(node_name, tag)`.
async fn list_variants(
    messenger: &peppylib::MessengerHandle,
    core_node_name: &str,
    node_name: &str,
    tag: &str,
) -> Result<Vec<String>> {
    let response = poll_stack_list(
        &StackListRequest::new(false),
        messenger,
        core_node_name,
        CALLER_INSTANCE_ID,
        core_node_name,
        REQUEST_TIMEOUT,
    )
    .await?;

    let graph: SerializedNodeGraph = serde_json::from_str(&response.graph_json)
        .map_err(|e| Error::ExecutionFailed(format!("failed to parse stack graph JSON: {e}")))?;

    Ok(graph
        .nodes
        .iter()
        .filter(|node| node.name == node_name && node.tag == tag)
        .map(|node| node.variant.clone())
        .collect())
}

/// When the caller omits `--variant`, the bare `name:tag` form must
/// resolve to exactly one variant in the stack. Otherwise the request is
/// ambiguous and we surface the available variants so the user can pick
/// one without re-running blind.
async fn resolve_single_variant(
    messenger: &peppylib::MessengerHandle,
    core_node_name: &str,
    node_name: &str,
    tag: &str,
) -> Result<String> {
    let mut variants = list_variants(messenger, core_node_name, node_name, tag).await?;
    match variants.len() {
        0 => Err(Error::ExecutionFailed(format!(
            "Node `{node_name}:{tag}` not found in node stack"
        ))),
        1 => Ok(variants.remove(0)),
        _ => {
            variants.sort();
            Err(Error::ExecutionFailed(format!(
                "`{node_name}:{tag}` matches multiple variants in the stack ({}); \
                 specify one with `--variant <name>` or `{node_name}:{tag}@<variant>`",
                variants.join(", ")
            )))
        }
    }
}

async fn fetch_instance_ids(
    messenger: &peppylib::MessengerHandle,
    core_node_name: &str,
    node_name: &str,
    tag: &str,
    variant: &str,
) -> Result<Vec<String>> {
    let response = poll_stack_list(
        &StackListRequest::new(false),
        messenger,
        core_node_name,
        CALLER_INSTANCE_ID,
        core_node_name,
        REQUEST_TIMEOUT,
    )
    .await?;

    let graph: SerializedNodeGraph = serde_json::from_str(&response.graph_json)
        .map_err(|e| Error::ExecutionFailed(format!("failed to parse stack graph JSON: {e}")))?;

    Ok(graph
        .nodes
        .iter()
        .find(|node| node.name == node_name && node.tag == tag && node.variant == variant)
        .map(|node| {
            node.running_instance_ids()
                .into_iter()
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default())
}

fn confirm_removal(
    node_name: &str,
    tag: &str,
    variant: &str,
    instance_ids: &[String],
) -> Result<bool> {
    use crate::commands::confirm::{confirm_prompt, format_instance_ids};

    let count = instance_ids.len();
    let suffix = if count == 1 { "instance" } else { "instances" };
    let verb = if count == 1 { "is" } else { "are" };
    let ids = format_instance_ids(instance_ids);
    let label = format_label(node_name, tag, variant);

    let message = format!(
        "Are you sure you want to remove `{label}`? \
         {count} {suffix} ({ids}) {verb} still running [y/n] ",
    );

    confirm_prompt(&message, None)
}
