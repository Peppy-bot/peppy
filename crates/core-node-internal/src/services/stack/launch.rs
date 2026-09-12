pub(super) mod clock;
pub(super) mod federated;
pub(super) mod feedback;
pub(super) mod nodes;
pub(super) mod orchestrate;
pub(super) mod phases;
pub(super) mod preflight;
pub(super) mod resolve;
pub(super) mod start;
pub(super) mod watchers;

use self::clock::{TimeSource, advise_on_clock_shape};
use self::feedback::{publish_stderr, publish_stdout};
use self::nodes::add_nodes_to_stack;
use self::orchestrate::{
    fail_and_clear_stack, teardown_and_reset_stack, validate_and_order_dependencies,
};
use self::preflight::preflight_change;
use self::resolve::{ParsedLaunch, parse_launcher_config, resolve_deployments};
use self::start::start_node_instances;
use self::watchers::{LifecycleWatchers, lifecycle_watchers};
use super::action::StackChangeContext;
use super::container_mounts::{LocalMounts, hosts_container_nodes, prepare_local_container_mounts};
use super::state::{ActiveLaunch, StackCopy};
use config::runtime::{CoreNodeName, Name};
use core_node_api::encoding::LaunchIdentity;
use core_node_api::encoding::{
    LaunchFeedbackStep, LaunchGoal, LaunchResult, NodeAddLogEntry, NodeBuildLogEntry,
    NodeRunLogEntry,
};
use daemon_config::launcher::{Deployment, Placements};
use std::collections::{HashMap, HashSet};

/// Which change the node phases are running for: a whole stack's launch, or
/// a join adding one copy to it.
#[derive(Clone, Copy)]
pub(super) enum PhaseChange<'a> {
    Launch,
    Join(&'a JoinScope),
}

impl PhaseChange<'_> {
    /// Whether the phases add to a running stack: a join reuses the nodes
    /// that already run where it places instances and extends the
    /// observations already registered.
    pub(super) fn appends(&self) -> bool {
        matches!(self, Self::Join(_))
    }

    fn reuses(&self, node: &NodeKey, core_node: &str) -> bool {
        match self {
            Self::Launch => false,
            Self::Join(scope) => scope.reuses(node, core_node),
        }
    }
}

/// What the phases need of the goal that started them.
pub(super) struct PhaseGoal {
    pub(super) launch_id: String,
    pub(super) rebuild: bool,
}

/// One node on one machine.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct HostedNode {
    pub(super) node: NodeKey,
    pub(super) core_node: CoreNodeName,
}

/// A node whose add on a peer outlived its budget: whether the peer holds
/// it is known by asking, and the fingerprint tells the join's entity from
/// one that was there before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct UnresolvedAdd {
    pub(super) hosted: HostedNode,
    pub(super) config_sha256: String,
}

/// The copy a join is adding, read by the launch phases that run for the
/// join and by its rollback: which nodes already run and are reused as
/// they are, which nodes the join brings to which machines, which machines
/// held nothing before it, and what its rollback hands back to each machine.
pub(super) struct JoinScope {
    pub(super) name: Name,
    pub(super) copy: StackCopy,
    pub(super) existing_nodes: HashSet<HostedNode>,
    pub(super) planned_nodes: HashSet<HostedNode>,
    pub(super) fresh_hosts: Vec<String>,
    /// The machines the join asked to hold this launch's slice.
    pub(super) participants: Vec<String>,
    /// Where the join's plan runs every instance, so each source's watchers
    /// reach the machine hosting it.
    pub(super) placements: Placements,
    /// The watchers of the plan before the join, for every source the join
    /// re-pointed.
    pub(super) restored_watchers: LifecycleWatchers,
}

impl JoinScope {
    fn reuses(&self, node: &NodeKey, core_node: &str) -> bool {
        self.existing_nodes
            .iter()
            .any(|hosted| &hosted.node == node && hosted.core_node.as_str() == core_node)
    }

    /// The nodes the join brought: every planned node that was not
    /// already running where the join placed it.
    pub(super) fn added_nodes(&self) -> impl Iterator<Item = &HostedNode> {
        self.planned_nodes
            .iter()
            .filter(|hosted| !self.existing_nodes.contains(hosted))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct NodeKey {
    pub(super) name: String,
    pub(super) tag: String,
}

impl NodeKey {
    pub(super) fn new(name: impl Into<String>, tag: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            tag: tag.into(),
        }
    }

    pub(super) fn label(&self) -> String {
        format!("{}:{}", self.name, self.tag)
    }
}

#[derive(Debug, Clone)]
pub(super) struct PlannedDeployment {
    pub(super) deployment: Deployment,
    pub(super) node_name: String,
    pub(super) node_tag: String,
    pub(super) config: config::node::NodeConfig,
    /// This daemon's fingerprint of the resolved manifest, echoed onto every
    /// instance dispatched to a peer so a peer whose entity moved between
    /// dispatch and start refuses rather than running a config the plan was
    /// never checked against.
    pub(super) config_sha256: String,
    /// What the deployment runs: the root pin of a node's resolved closure,
    /// or the pinned exposures of the built-in MCP server.
    pub(super) root: daemon_config::repository::DeploymentRoot,
    /// The rest of the closure: for a node, dependency-node pins plus, once
    /// `mint_doc_pins` has run, the contract and pairing document pins every
    /// add of this deployment carries; for the built-in server, the contract
    /// pins its exposures reference.
    pub(super) closure_pins: Vec<daemon_config::repository::PinnedItem>,
    /// Every manifest in the deployment's closure, root first. What the
    /// doc-pin minting walks after the graph validation has had first say.
    pub(super) pin_manifests: Vec<config::node::Manifest>,
}

/// Process a stack launch request.
///
/// This function orchestrates the complete launch sequence:
/// 1. Parse launcher configuration
/// 2. Resolve deployments and mint their node pins
/// 3. Validate dependencies and compute order, then mint the doc pins
/// 4. Federated preflight, carrying the pins
/// 5. Snapshot and clear stack
/// 6. Add nodes in dependency order
/// 7. Prepare this machine's container host mounts
/// 8. Start instances in dependency order
pub(super) async fn process_launch(goal: LaunchGoal, ctx: StackChangeContext) -> LaunchResult {
    // Step 1: Parse the launcher and bind its core node links to machines.
    let ParsedLaunch {
        prepared,
        composed,
        placements,
    } = match parse_launcher_config(&ctx, &goal).await {
        Ok(result) => result,
        Err(launch_result) => return launch_result,
    };

    let copies = composed.copies().to_vec();
    let daemon_config::launcher::ComposedLaunch {
        launcher: flat,
        selection,
        ..
    } = composed;

    // Step 2: Resolve every deployment, once, on this daemon, minting the
    // node pins the whole launch runs. Resolution touches no other machine
    // and tears nothing down, so refusing here is free, and the
    // reservations below need the pins to carry.
    let mut planned = match resolve_deployments(&ctx, flat.deployments.clone(), &placements).await {
        Ok(result) => result,
        Err(launch_result) => return launch_result,
    };

    // A launch that starts nothing records the launcher and stops: its
    // copies arrive by stack join.
    if planned.is_empty() {
        teardown_and_reset_stack(&ctx).await;
        ctx.slice_ownership
            .record_slice(LaunchIdentity::new(&goal.launch_id, &ctx.bound_core_node));
        *ctx.slice_ownership.active.lock() = Some(ActiveLaunch::new(
            &goal.launch_id,
            prepared,
            flat,
            selection,
            placements,
            planned,
        ));
        publish_stdout(
            &ctx,
            "Launcher active; add copies with peppy stack join OPTION -i NAME".to_owned(),
            LaunchFeedbackStep::LauncherStep,
        )
        .await;
        return LaunchResult::success(&ctx.log_path);
    }

    // Step 3: Validate dependencies and compute one global topological order,
    // across every machine. There is exactly one planner. Runs before the
    // doc pins are minted so a graph refusal, which points at the launcher
    // and the manifests, has first say over a document missing from this
    // machine's caches.
    let root_config = ctx.node_stack.root().read().config().clone();
    let (ordered, resolved_slot_bindings, planned_pairings, planned_observations) =
        match validate_and_order_dependencies(&ctx, &planned, &root_config, &placements).await {
            Ok(result) => result,
            Err(launch_result) => return launch_result,
        };

    // Step 3b: Pin the contract and pairing documents every manifest in the
    // launch names. Still before any reservation, so a document this
    // machine cannot pin refuses the launch while it has cost nothing.
    if let Err(launch_result) = resolve::mint_doc_pins(&ctx, &mut planned, &placements).await {
        return launch_result;
    }

    // Step 4: The clock the launch runs on, and the federated preflight:
    // reachability, reservations carrying each participant's pins, and the
    // bind sources each machine's containers need, all BEFORE anything is
    // torn down. A refusal at this point has cost no machine, including
    // this one, its stack.
    let change = match preflight_change(
        &ctx,
        &goal.launch_id,
        &planned,
        &planned,
        &placements,
        None,
    )
    .await
    {
        Ok(change) => change,
        Err(reason) => {
            publish_stderr(&ctx, reason.clone(), LaunchFeedbackStep::LauncherStep).await;
            return LaunchResult::failure(&ctx.log_path, reason);
        }
    };
    advise_on_clock_shape(&ctx, &planned, &change.clock).await;
    let (established, watchers) =
        match change
            .reserved
            .established_clock(&change.clock)
            .and_then(|clock| {
                Ok((
                    clock,
                    lifecycle_watchers(&planned_observations, &placements)?,
                ))
            }) {
            Ok(established) => established,
            Err(reason) => {
                publish_stderr(&ctx, reason.clone(), LaunchFeedbackStep::LauncherStep).await;
                return release_and_fail(
                    change.reserved,
                    LaunchResult::failure(&ctx.log_path, reason),
                )
                .await;
            }
        };

    // Step 4b: A planned instance id must not collide with a participant's
    // own root entity, which occupies that machine's namespace before this
    // launch touches it.
    let refusals = change.reserved.root_instance_collisions(
        &planned
            .iter()
            .flat_map(|item| &item.deployment.instances)
            .map(|instance| instance.instance_id.as_str())
            .collect(),
    );
    if !refusals.is_empty() {
        let msg = daemon_config::format_bulleted(&refusals);
        publish_stderr(&ctx, msg.clone(), LaunchFeedbackStep::LauncherStep).await;
        return release_and_fail(change.reserved, LaunchResult::failure(&ctx.log_path, msg)).await;
    }

    // Step 5: The commit point. Every participant is reserved and the whole
    // plan is validated, so now, and only now, do stacks get replaced. Peers
    // first: if one refuses, this daemon still has its own stack. Each one is
    // handed the bind sources its slice needs, to prepare while it is empty.
    let participants = change.reserved.core_nodes();
    if let Err(refusal) = federated::begin_participant_slices(
        &ctx,
        &goal.launch_id,
        &participants,
        &change.mounts,
        &watchers,
        &placements,
        false,
    )
    .await
    {
        publish_stderr(
            &ctx,
            refusal.reason.clone(),
            LaunchFeedbackStep::LauncherStep,
        )
        .await;
        federated::clear_participant_slices(&ctx, &refusal.holders_among(&participants)).await;
        return release_and_fail(
            change.reserved,
            LaunchResult::failure(&ctx.log_path, refusal.reason),
        )
        .await;
    }
    teardown_and_reset_stack(&ctx).await;

    // Record which launch this daemon's slice belongs to, so the slice is
    // self-describing from here on and `stack reset` / a relaunch can
    // rediscover the whole launch by query. Each participant recorded its own
    // when it began its slice.
    ctx.slice_ownership.record_slice(LaunchIdentity::new(
        goal.launch_id.clone(),
        ctx.bound_core_node.as_str(),
    ));

    let mut active = ActiveLaunch::new(
        &goal.launch_id,
        prepared,
        flat,
        selection,
        placements.clone(),
        planned.clone(),
    )
    .with_clock(established)
    .with_watchers(watchers)
    .with_time_source(TimeSource::of(
        &ctx,
        &planned,
        &placements,
        &change.reserved,
    ));
    active.record_copies(copies, &ordered);
    let phase = PhaseGoal {
        launch_id: goal.launch_id.clone(),
        rebuild: goal.rebuild,
    };
    // Build lookup map
    let planned_by_key: HashMap<NodeKey, PlannedDeployment> = planned
        .into_iter()
        .map(|item| (NodeKey::new(&item.node_name, &item.node_tag), item))
        .collect();

    let mut add_log_paths: Vec<NodeAddLogEntry> = Vec::new();
    let mut build_log_paths: Vec<NodeBuildLogEntry> = Vec::new();
    let mut run_log_paths: Vec<NodeRunLogEntry> = Vec::new();

    // Step 6: Add and build, one group per machine. The groups run
    // concurrently because nothing orders one machine's add against another's,
    // and fetching plus building is where a launch spends its time; within a
    // group the dependency order is preserved exactly as on a single machine.
    //
    // Steps 6-8 short-circuit: each phase appends to the log vectors before
    // returning `Err`, so the logs collected up to a failure are reported
    // whichever phase failed.
    let outcome = async {
        add_nodes_to_stack(
            &ctx,
            &phase,
            PhaseChange::Launch,
            &ordered,
            &planned_by_key,
            &placements,
            &mut add_log_paths,
            &mut build_log_paths,
            // A failed launch clears every participant's slice, and with it
            // whatever a peer holds for an add that outlived its budget.
            &mut Vec::new(),
        )
        .await?;

        // Step 7: Prepare this machine's Lima host mounts before the first
        // container starts. Updating Lima's mount table can restart the VM;
        // doing it lazily during a later instance start would kill containers
        // already launched by this stack operation.
        prepare_local_container_mounts(
            &ctx,
            hosts_container_nodes(
                ordered.iter().filter_map(|key| planned_by_key.get(key)),
                &placements,
                &ctx.bound_core_node,
            ),
            change
                .mounts
                .get(ctx.bound_core_node.as_str())
                .cloned()
                .unwrap_or_default(),
            LocalMounts::WholeStack,
        )
        .await?;

        // Step 8: Start instances in dependency order.
        start_node_instances(
            &ctx,
            &phase,
            PhaseChange::Launch,
            &ordered,
            &planned_by_key,
            &mut run_log_paths,
            &resolved_slot_bindings,
            &planned_pairings,
            &planned_observations,
            &placements,
            change.fleet.as_ref(),
        )
        .await
    }
    .await;

    if let Err(reason) = outcome {
        let mut launch_result = fail_and_clear_stack(&ctx, reason, &participants).await;
        launch_result.node_add_logs = add_log_paths;
        launch_result.node_build_logs = build_log_paths;
        launch_result.node_run_logs = run_log_paths;
        return release_and_fail(change.reserved, launch_result).await;
    }

    *ctx.slice_ownership.active.lock() = Some(active);
    publish_stdout(&ctx, "Launch complete", LaunchFeedbackStep::LauncherStep).await;
    // Release every participant now that the launch is done. The SLICE record
    // stays: the reservation guards the launch, the slice describes its result,
    // and rediscovery needs the latter long after the former is gone.
    change.reserved.release().await;
    LaunchResult::success(&ctx.log_path).with_node_logs(
        add_log_paths,
        build_log_paths,
        run_log_paths,
    )
}

/// Releases every participant and returns the failure.
///
/// Every failure path funnels through here so a launch can never end while
/// still holding a machine. Whether the participants' stacks were also cleared
/// is a separate question the caller answers, because it depends on whether
/// anything had been dispatched to them yet.
async fn release_and_fail(
    reserved: federated::ReservedParticipants,
    launch_result: LaunchResult,
) -> LaunchResult {
    reserved.release().await;
    launch_result
}
