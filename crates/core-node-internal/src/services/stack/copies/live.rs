//! What the machines a join touches must hold before it starts anything: the
//! nodes it reuses running unchanged since the launch resolved them, the
//! instances it depends on live, and no instance already answering to a name
//! the copy brings.

use super::super::action::StackChangeContext;
use super::super::launch::{HostedNode, NodeKey, PlannedDeployment};
use super::super::state::ActiveLaunch;
use super::super::{ChangeResult, STACK_QUERY_TIMEOUT};
use super::stack_list_on;
use daemon_config::launcher::Placements;
use std::collections::HashSet;

async fn graph_on(
    ctx: &StackChangeContext,
    host: &str,
) -> ChangeResult<core_node_api::SerializedNodeGraph> {
    if host == ctx.bound_core_node {
        return Ok(ctx.node_stack.to_serialized_graph());
    }
    let response = stack_list_on(ctx, host).await?;
    serde_json::from_str(&response.graph_json)
        .map_err(|e| format!("invalid stack report from `{host}`: {e}"))
}

/// What a join checks against the machines it touches.
pub(super) struct LiveCheck<'a> {
    pub active: &'a ActiveLaunch,
    /// The plan cut down to the instances the join touches.
    pub touched: &'a [PlannedDeployment],
    pub placements: &'a Placements,
    pub participants: &'a [String],
    pub new_ids: &'a HashSet<String>,
    pub required_ids: &'a HashSet<String>,
}

/// What the machines a join touches hold.
pub(super) struct LiveStack {
    /// The nodes of the plan already running where the plan places them,
    /// unchanged since launch: the join reuses them as they are.
    pub reusable: HashSet<HostedNode>,
    /// The participants that held no node before the join.
    pub fresh_hosts: Vec<String>,
}

/// Every machine the join touches holds what the record says: the nodes it
/// reuses run unchanged, the instances it needs are live, and nothing
/// named like its new instances exists.
pub(super) async fn check_live_stack(
    ctx: &StackChangeContext,
    check: LiveCheck<'_>,
) -> ChangeResult<LiveStack> {
    let LiveCheck {
        active,
        touched,
        placements,
        participants,
        new_ids,
        required_ids,
    } = check;
    let mut reusable = HashSet::new();
    let mut fresh_hosts = Vec::new();
    for host in std::iter::once(&ctx.bound_core_node).chain(participants) {
        let graph = graph_on(ctx, host).await?;
        let holds_nodes = graph
            .nodes
            .iter()
            .any(|node| node.stage != Some(core_node_api::NodeStage::Root));
        if host != &ctx.bound_core_node && !holds_nodes {
            fresh_hosts.push(host.clone());
        }
        for item in touched.iter().filter(|item| {
            item.deployment
                .instances
                .iter()
                .any(|instance| placements.of(instance.instance_id.as_str()) == host)
        }) {
            let Some(node) = graph
                .nodes
                .iter()
                .find(|node| node.name == item.node_name && node.tag == item.node_tag)
            else {
                continue;
            };
            if node.instances.is_empty() {
                continue;
            }
            let tracked =
                active.planned.iter().any(|old| {
                    old.node_name == item.node_name
                        && old.node_tag == item.node_tag
                        && old.deployment.instances.iter().any(|instance| {
                            active.placements.of(instance.instance_id.as_str()) == host
                        })
                });
            if !tracked {
                return Err(format!(
                    "{}:{} on `{host}` has instances outside this launcher; joining would replace their node. Choose an empty machine or remove that node first",
                    item.node_name, item.node_tag
                ));
            }
            let response = peppylib::core_node::transport::poll(
                &core_node_api::encoding::NodeInfoRequest::new(&item.node_name, &item.node_tag),
                &ctx.messenger,
                &ctx.bound_core_node,
                &ctx.core_instance_id,
                host,
                STACK_QUERY_TIMEOUT,
            )
            .await
            .map_err(|e| {
                format!(
                    "cannot inspect {}:{} on `{host}`: {e}",
                    item.node_name, item.node_tag
                )
            })?;
            if !matches!(response, core_node_api::encoding::NodeInfoResponse::Found(info) if info.config_integrity == item.config_sha256)
            {
                return Err(format!(
                    "{}:{} on `{host}` changed since launch; relaunch before joining another copy",
                    item.node_name, item.node_tag
                ));
            }
            reusable.insert(HostedNode {
                node: NodeKey::new(&item.node_name, &item.node_tag),
                core_node: config::runtime::CoreNodeName::new(host)
                    .map_err(|error| error.to_string())?,
            });
        }
        for instance in graph.nodes.iter().flat_map(|node| &node.instances) {
            if new_ids.contains(&instance.instance_id) {
                return Err(format!(
                    "instance `{}` already exists on `{host}`; choose another name for the copy",
                    instance.instance_id
                ));
            }
        }
        for instance in active
            .flat
            .deployments
            .iter()
            .flat_map(|d| &d.instances)
            .filter(|instance| required_ids.contains(instance.instance_id.as_str()))
            .filter(|instance| active.placements.of(instance.instance_id.as_str()) == host)
        {
            if !graph
                .nodes
                .iter()
                .flat_map(|node| &node.instances)
                .any(|live| live.instance_id == instance.instance_id.as_str())
            {
                return Err(format!(
                    "required instance `{}` is missing from `{host}`; restore its copy or relaunch the stack before adding this one",
                    instance.instance_id
                ));
            }
        }
    }
    Ok(LiveStack {
        reusable,
        fresh_hosts,
    })
}
