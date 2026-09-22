//! What the machines a change touches must hold before it starts anything: the
//! nodes a join reuses running unchanged since the launch resolved them, the
//! instances it depends on live, no instance already answering to a name the
//! copy brings, and the machines of the sets it changes live on the federation.
//! A removal takes what it can: this module also splits its slots by whether
//! their machine is live, and tells the operator which sets stayed behind.

use super::super::action::StackChangeContext;
use super::super::launch::feedback::publish_stderr;
use super::super::launch::{HostedNode, NodeKey, PlannedDeployment, federated};
use super::super::state::ActiveLaunch;
use super::super::{ChangeResult, STACK_QUERY_TIMEOUT};
use super::changed_slots::ChangedSlot;
use super::stack_list_on;
use config::runtime::Name;
use core_node_api::encoding::LaunchFeedbackStep;
use daemon_config::launcher::Placements;
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// What `host` holds for `node`.
pub(super) async fn node_info_on(
    ctx: &StackChangeContext,
    host: &str,
    node: &NodeKey,
) -> ChangeResult<core_node_api::encoding::NodeInfoResponse> {
    peppylib::core_node::transport::poll(
        &core_node_api::encoding::NodeInfoRequest::new(&node.name, &node.tag),
        &ctx.messenger,
        &ctx.bound_core_node,
        &ctx.core_instance_id,
        host,
        STACK_QUERY_TIMEOUT,
    )
    .await
    .map_err(|e| format!("cannot inspect {} on `{host}`: {e}", node.label()))
}

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
    /// The set slots the join grows, whose instances must be running to take
    /// them.
    pub grown_slots: &'a [ChangedSlot],
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
        grown_slots,
    } = check;
    let mut reusable = HashSet::new();
    let mut fresh_hosts = Vec::new();
    for host in std::iter::once(&ctx.bound_core_node).chain(participants) {
        let graph = graph_on(ctx, host).await?;
        let holds_nodes = graph
            .nodes
            .iter()
            .any(|node| node.stage != core_node_api::NodeStage::Root);
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
            let response =
                node_info_on(ctx, host, &NodeKey::new(&item.node_name, &item.node_tag)).await?;
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
        for slot in grown_slots
            .iter()
            .filter(|slot| placements.of(slot.instance_id.as_str()) == host)
        {
            let state = graph
                .nodes
                .iter()
                .flat_map(|node| &node.instances)
                .find(|live| live.instance_id == slot.instance_id.as_str())
                .map(|live| live.state);
            match state {
                Some(core_node_api::InstanceState::Running) => {}
                Some(core_node_api::InstanceState::Starting) => {
                    return Err(format!(
                        "joining grows {}, but `{}` is starting on `{host}`; join again once it \
                         is running, or join an option whose fragments do not add to that slot",
                        slot.field(),
                        slot.instance_id
                    ));
                }
                _ => {
                    return Err(format!(
                        "joining grows {}, but `{}` is not running on `{host}`; relaunch the \
                         stack so `{host}` runs it again, or join an option whose fragments do \
                         not add to that slot",
                        slot.field(),
                        slot.instance_id
                    ));
                }
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

/// The core nodes live on the federation, this daemon included, asked only when
/// `hosts` names a machine other than this one: a change that touches this
/// machine alone answers every liveness question from its own name.
pub(super) async fn live_machines<'a>(
    ctx: &StackChangeContext,
    hosts: impl IntoIterator<Item = &'a str>,
) -> ChangeResult<BTreeSet<String>> {
    let mut live = BTreeSet::from([ctx.bound_core_node.clone()]);
    if hosts.into_iter().all(|host| host == ctx.bound_core_node) {
        return Ok(live);
    }
    live.extend(federated::live_core_nodes(&ctx.messenger).await?);
    Ok(live)
}

/// Whether `slot`'s instance runs on one of the `live` machines, which
/// [`live_machines`] always counts this daemon among.
fn runs_on_live_machine(
    slot: &ChangedSlot,
    placements: &Placements,
    live: &BTreeSet<String>,
) -> bool {
    live.contains(slot.host(placements))
}

/// Refuses a join that would grow a set whose instance runs on a machine that is
/// not live on the federation, naming the slot and the fix.
pub(super) fn check_hosts_live(
    copy: &Name,
    slots: &[ChangedSlot],
    placements: &Placements,
    live: &BTreeSet<String>,
) -> Result<(), String> {
    let Some(slot) = slots
        .iter()
        .find(|slot| !runs_on_live_machine(slot, placements, live))
    else {
        return Ok(());
    };
    let host = slot.host(placements);
    Err(format!(
        "joining `{copy}` would add members to {} on `{host}`, which is not live on the \
         federation. Bring `{host}`'s daemon back, logged into this workspace: if it still runs \
         `{}`, join again; if it does not, relaunch the stack. Or join an option whose fragments \
         do not add to that slot",
        slot.field(),
        slot.instance_id
    ))
}

/// Splits `slots` into those whose instance runs on a machine in `live`, and
/// those whose instance runs on a machine that is not.
pub(super) fn split_by_liveness(
    slots: Vec<ChangedSlot>,
    placements: &Placements,
    live: &BTreeSet<String>,
) -> (Vec<ChangedSlot>, Vec<ChangedSlot>) {
    slots
        .into_iter()
        .partition(|slot| runs_on_live_machine(slot, placements, live))
}

/// Tells the operator which sets a removal left untouched because the machine
/// holding them is not live, one line per machine.
pub(super) async fn report_offline_sets(
    ctx: &StackChangeContext,
    copy: &Name,
    slots: &[ChangedSlot],
    placements: &Placements,
) {
    let mut by_host: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for slot in slots {
        by_host
            .entry(slot.host(placements))
            .or_default()
            .push(slot.field());
    }
    for (host, fields) in by_host {
        publish_stderr(
            ctx,
            format!(
                "`{host}` is not live on the federation, so removing copy `{copy}` did not \
                 update these sets there: {}. If `{host}` comes back still running their \
                 instances, the next `peppy stack join` or `peppy stack remove` changing one of \
                 them delivers it whole; if it comes back without them, relaunch the stack",
                fields.join(", ")
            ),
            LaunchFeedbackStep::LauncherStep,
        )
        .await;
    }
}
