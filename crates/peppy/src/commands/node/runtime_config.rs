use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use config::node::NodeConfigParser;
use config::runtime::Name;
use config::runtime::{NodeInstanceConfig, RuntimeConfig};
use core_node_api::encoding::StackListRequest;
use names_generator2::get_random;
use rand::rng;
use tracing::info;

use crate::commands::CALLER_INSTANCE_ID;
use crate::context::AppContext;
use crate::error::{Error, Result};

use super::run::args_to_node_arguments;
use peppylib::core_node::transport::poll;

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
    let conn = ctx.connect_to_daemon().await?;

    // The printed config embeds this session's messaging endpoint and the
    // local daemon's identity — exactly what a locally spawned node receives.
    // For a remote target it would describe an instance no node there could
    // run, so the override is refused (mirrors `peppy node run`).
    crate::commands::reject_remote_target_for_local_endpoint(&conn, "peppy node runtime-config")?;

    // Validate that the node is present in the node stack so the output corresponds to a runnable node.
    let response = poll(
        &StackListRequest::new(),
        conn.messenger,
        &conn.core_node_name,
        CALLER_INSTANCE_ID,
        &conn.target_core_node,
        REQUEST_TIMEOUT,
    )
    .await?;

    let graph = crate::commands::parse_stack_graph(&response.graph_json)?;

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
        return Err(Error::ExecutionFailed(format!(
            "Node '{}' has multiple tags in the stack: {}. Specify the desired tag.",
            node_name,
            tags.join(", ")
        )));
    }

    let (messaging_host, messaging_port) =
        crate::commands::resolve_messaging_endpoint(conn.messenger).await;

    let instance_id = get_random(rng());
    let node_tag = matching_nodes
        .first()
        .map(|n| n.tag.as_str())
        .ok_or_else(|| {
            Error::ExecutionFailed(format!(
                "Node '{node_name}' has no tagged entries in the stack"
            ))
        })?;
    let runtime_config = RuntimeConfig::new(
        messaging_host.as_str(),
        messaging_port,
        NodeInstanceConfig {
            arguments: args_to_node_arguments(&args),
            ..NodeInstanceConfig::new(
                Name::new(instance_id).map_err(|e| Error::PeppyConfig(e.into()))?,
            )
        },
        node_name,
        node_tag,
        conn.core_node_name,
    )
    .map_err(Error::PeppyConfig)?;

    // Reflect the daemon's organization namespace, exactly as `apply_daemon_defaults`
    // stamps it onto a launched node, so this inspection output matches the session
    // namespace a real node would open under. Reuse the namespace captured when the
    // connection was established (above) rather than reading the state again, which
    // could race a restart and pair this generation's stack lookup with another
    // generation's namespace.
    let mut runtime_config = runtime_config;
    runtime_config.discovery.namespace = Some(
        config::namespace::Namespace::parse(&conn.namespace).map_err(|error| {
            Error::ExecutionFailed(format!(
                "the daemon state carries an invalid namespace: {error}"
            ))
        })?,
    );

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
