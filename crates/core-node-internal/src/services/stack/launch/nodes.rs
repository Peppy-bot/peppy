//! The add-and-build phase of a stack change: every node of the plan present
//! and built on each machine that runs part of it, and every joinable node of
//! a launch on the coordinator, before any instance starts.

use super::federated::RemoteGoalFailure;
use super::feedback::publish_stdout;
use super::joinable::JoinableBuild;
use super::orchestrate::{add_node_directly, build_node_directly};
use super::{
    HostedNode, NodeKey, PhaseChange, PhaseGoal, PlannedDeployment, UnresolvedAdd, federated,
};
use crate::services::node::pins::encode_pins;
use crate::services::response::rephrased;
use crate::services::stack::action::StackChangeContext;
use crate::services::stack::{ChangeResult, attempt_failed};
use config::runtime::CoreNodeName;
use core_node_api::encoding::{
    LaunchFeedbackStep, NodeAddGoal, NodeAddLogEntry, NodeBuildGoal, NodeBuildLogEntry, NodeSource,
};
use daemon_config::repository::DeploymentRoot;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use peppylib::messaging::Failure;
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

/// Marker git_hash for an add whose bytes a pin already vouched for: the ones
/// stack launch issues, and the per-node sub-goals `add_batch` issues for any
/// pinned add (`peppy node add <name>:<tag>` included). The node_add service
/// skips git hash and codegen-fingerprint verification for it and generates
/// fresh peppygen files: those checks belong to `peppy node sync` workflows,
/// and these adds operate on a tree materialized from an already-verified pin.
pub(crate) const STACK_LAUNCH_GIT_HASH: &str = "stack-launch";

/// The build goal a peer receives for one deployment of `goal`: the launch's
/// identity, so the peer accepts it while reserved for that launch, and the
/// launch's `rebuild` switch, so a `--rebuild` launch ignores cached
/// artifacts on every machine, not only on the coordinator.
fn remote_build_goal(
    phase: &PhaseGoal,
    node_name: String,
    node_tag: String,
    budget: Duration,
) -> NodeBuildGoal {
    NodeBuildGoal::new(node_name, node_tag, budget.as_secs())
        .with_launch_id(&phase.launch_id)
        .with_rebuild(phase.rebuild)
}

/// The add source a deployment dispatches with: its root, encoded at the
/// point of dispatch beside the closure pins, so the local arm and the goal a
/// peer receives cannot disagree on how the pins travel.
fn pinned_source(
    key: &NodeKey,
    item: &PlannedDeployment,
) -> std::result::Result<NodeSource, String> {
    match &item.root {
        DeploymentRoot::Node(pin) => serde_json5::to_string(pin)
            .map(|pin_json5| NodeSource::Pinned { pin_json5 })
            .map_err(|e| format!("deployment {}: could not encode its pin: {e}", key.label())),
        DeploymentRoot::Exposures(pins) => encode_pins(pins)
            .map(|pins_json5| NodeSource::Exposures { pins_json5 })
            .map_err(|e| format!("deployment {}: {e}", key.label())),
    }
}

/// Whether a deployment is the built-in MCP server, which is registered by
/// its add and has no build stage.
fn is_built_in(item: &PlannedDeployment) -> bool {
    matches!(item.root, DeploymentRoot::Exposures(_))
}

/// Which core nodes host at least one instance of `key`, in a stable order.
///
/// A node is added and built on every machine that runs part of it, which is
/// what "several placed instances under one deployment" means operationally:
/// each daemon has to have the node present before it can start its share.
fn hosts_of(
    item: &PlannedDeployment,
    placements: &daemon_config::launcher::Placements,
) -> Vec<CoreNodeName> {
    let mut hosts: Vec<CoreNodeName> = item
        .deployment
        .instances
        .iter()
        .map(|instance| {
            placements
                .core_node_of(instance.instance_id.as_str())
                .clone()
        })
        .collect();
    hosts.sort();
    hosts.dedup();
    hosts
}

/// How many nodes this machine builds at the same time in one stack change.
///
/// The builds of two nodes share nothing but the build caches, which are made
/// for concurrent builds. Two at a time already hides most of a stack's build
/// time behind its longest build: a simulation stack spends about half of its
/// build time on its engine alone, and its other nodes build one after
/// another beside it. More would put several large compilations in memory at
/// once, which a small machine does not have.
const MAX_CONCURRENT_LOCAL_BUILDS: usize = 2;

/// Adds and builds every node of the change, grouped by the machine that
/// will run it.
///
/// The groups run CONCURRENTLY. That split is deliberate: nothing orders one
/// machine's fetch-and-build against another's, and fetching plus building is
/// where a launch spends nearly all of its wall clock, so serializing across
/// machines would make a two-machine launch twice as slow for no invariant.
/// Within a group the nodes are added in dependency order, the order a
/// single-machine launch starts them in. The coordinator's group then adds
/// each joinable node of a launch that the launch does not already add there.
/// This machine then builds what it added, [`MAX_CONCURRENT_LOCAL_BUILDS`] at
/// a time, in the order it added them; a peer builds each node right after
/// its add, as its daemon admits one build at a time.
///
/// `unresolved` collects the nodes whose add on a peer outlived its budget:
/// whether each landed is known only by asking that peer.
#[allow(clippy::too_many_arguments)] // Distinct inputs; bundling them would only move the list.
pub(in crate::services::stack) async fn add_nodes_to_stack(
    ctx: &StackChangeContext,
    phase: &PhaseGoal,
    change: PhaseChange<'_>,
    ordered: &[NodeKey],
    planned_by_key: &HashMap<NodeKey, PlannedDeployment>,
    placements: &daemon_config::launcher::Placements,
    add_log_paths: &mut Vec<NodeAddLogEntry>,
    build_log_paths: &mut Vec<NodeBuildLogEntry>,
    unresolved: &mut Vec<UnresolvedAdd>,
) -> ChangeResult<()> {
    publish_stdout(
        ctx,
        "Adding nodes to the stack...",
        LaunchFeedbackStep::LauncherStep,
    )
    .await;

    let by_core_node = add_groups(change, ordered, planned_by_key, placements);
    let groups = futures::future::join_all(
        by_core_node
            .iter()
            .map(|(core_node, nodes)| add_and_build_group(ctx, phase, change, core_node, nodes)),
    )
    .await;

    let mut failure: Option<Failure> = None;
    for group in groups {
        add_log_paths.extend(group.add_logs);
        build_log_paths.extend(group.build_logs);
        unresolved.extend(group.unresolved);
        // Report the FIRST failure but keep collecting every group's logs: a
        // launch that failed on one machine still produced logs on the others,
        // and those are usually what explains it.
        if let Some(reason) = group.failure {
            failure.get_or_insert(reason);
        }
    }

    match failure {
        Some(reason) => Err(reason),
        None => Ok(()),
    }
}

/// The nodes each machine adds and builds for the change, each group in the
/// order the machine adds them: every machine that hosts an instance of a
/// node of `ordered` adds that node, in dependency order, and the
/// coordinator then adds each joinable node of the change that it does not
/// already add, so a node is added and built once per machine.
fn add_groups<'a>(
    change: PhaseChange<'a>,
    ordered: &[NodeKey],
    planned_by_key: &'a HashMap<NodeKey, PlannedDeployment>,
    placements: &daemon_config::launcher::Placements,
) -> BTreeMap<CoreNodeName, Vec<GroupNode<'a>>> {
    let mut by_core_node: BTreeMap<CoreNodeName, Vec<GroupNode<'a>>> = BTreeMap::new();
    for key in ordered {
        let Some(item) = planned_by_key.get(key) else {
            continue;
        };
        for host in hosts_of(item, placements) {
            by_core_node
                .entry(host)
                .or_default()
                .push(GroupNode::planned(item));
        }
    }
    let joinable = change.joinable_builds();
    if joinable.is_empty() {
        return by_core_node;
    }
    let coordinator = by_core_node
        .entry(placements.coordinator_name().clone())
        .or_default();
    for build in joinable {
        let key = build.planned.key();
        if !coordinator.iter().any(|node| node.key == key) {
            coordinator.push(GroupNode::joinable(build));
        }
    }
    by_core_node
}

/// One node of a machine's add-and-build group.
struct GroupNode<'a> {
    key: NodeKey,
    item: &'a PlannedDeployment,
    /// What a joinable node is built for; `None` for a node the change runs.
    purpose: Option<String>,
}

impl<'a> GroupNode<'a> {
    fn planned(item: &'a PlannedDeployment) -> Self {
        Self {
            key: item.key(),
            item,
            purpose: None,
        }
    }

    fn joinable(build: &'a JoinableBuild) -> Self {
        Self {
            key: build.planned.key(),
            item: &build.planned,
            purpose: Some(build.purpose()),
        }
    }

    /// The node's add line: its label, its machine and, for a joinable
    /// node, what it is built for.
    fn add_line(&self, core_node: &CoreNodeName) -> String {
        match &self.purpose {
            None => format!("Adding {} on `{core_node}`", self.key.label()),
            Some(purpose) => format!("Adding {} on `{core_node}` {purpose}", self.key.label()),
        }
    }
}

/// The failure of `step` ("add" or "build") of the node `label`, of the
/// kind of `failure`. A joinable node's failure says what it was built for
/// and names the flag that skips it.
fn node_failure(step: &str, label: &str, purpose: Option<&str>, failure: &Failure) -> Failure {
    let reason = failure.message();
    rephrased(
        failure,
        match purpose {
            None => format!("failed to {step} node {label}: {reason}"),
            Some(purpose) => format!(
                "failed to {step} node {label} {purpose}: {reason}; `--no-prebuild` skips the \
                 nodes built for stack joins"
            ),
        },
    )
}

/// What one machine's add-and-build group produced.
#[derive(Default)]
struct GroupOutcome {
    add_logs: Vec<NodeAddLogEntry>,
    build_logs: Vec<NodeBuildLogEntry>,
    unresolved: Vec<UnresolvedAdd>,
    failure: Option<Failure>,
}

async fn add_and_build_group(
    ctx: &StackChangeContext,
    phase: &PhaseGoal,
    change: PhaseChange<'_>,
    core_node: &CoreNodeName,
    nodes: &[GroupNode<'_>],
) -> GroupOutcome {
    let mut outcome = GroupOutcome::default();
    let mut added_here = Vec::new();

    for node in nodes {
        if change.reuses(&node.key, core_node.as_str()) {
            continue;
        }

        publish_stdout(
            ctx,
            node.add_line(core_node),
            LaunchFeedbackStep::AddingNode,
        )
        .await;

        let added = if core_node.as_str() == ctx.bound_core_node.as_str() {
            add_locally(ctx, phase, node, &mut outcome).await
        } else {
            add_and_build_remotely(ctx, phase, core_node, &node.key, node.item, &mut outcome)
                .await
                .map(|()| None)
        };
        match added {
            Ok(Some(node)) => added_here.push(node),
            Ok(None) => {}
            Err(reason) => {
                outcome.failure = Some(reason);
                return outcome;
            }
        }
    }

    build_locally(ctx, phase, &added_here, &mut outcome).await;
    outcome
}

/// A node this machine added for the change and still has to build.
struct AddedNode {
    label: String,
    node_name: String,
    node_tag: String,
    /// What a joinable node is built for; `None` for a node the change runs.
    purpose: Option<String>,
}

/// Adds one node on this machine. Returns the node to build, or `None` for
/// the built-in MCP server, which its add registers ready.
async fn add_locally(
    ctx: &StackChangeContext,
    phase: &PhaseGoal,
    node: &GroupNode<'_>,
    outcome: &mut GroupOutcome,
) -> ChangeResult<Option<AddedNode>> {
    let GroupNode { key, item, purpose } = node;
    // The identical source and pins a peer would receive: a launch adds
    // one set of bytes wherever a deployment lands, so the local arm
    // must not get to differ from the dispatched one. The environment is
    // the one deliberate difference: the caller's env vars describe this
    // machine, so they apply here and stay off the goals a peer receives.
    let (source, pins) = pinned_source(key, item)
        .and_then(|source| encode_pins(&item.closure_pins).map(|pins| (source, pins)))
        .map_err(attempt_failed)?;
    let node_add_goal = NodeAddGoal::for_internal_execution(source, STACK_LAUNCH_GIT_HASH)
        .with_launch_id(&phase.launch_id)
        .with_env_vars(ctx.env_vars.clone())
        .with_pins(pins);

    let (result, log_path) = add_node_directly(ctx, node_add_goal).await;

    if let Some(path) = log_path {
        outcome.add_logs.push(NodeAddLogEntry {
            node_label: key.label(),
            log_path: path,
            failed: result.is_err(),
            core_node: ctx.bound_core_node.as_str().to_owned(),
        });
    }

    let added = result
        .map_err(|failure| node_failure("add", &key.label(), purpose.as_deref(), &failure))?;

    if is_built_in(item) {
        return Ok(None);
    }
    Ok(Some(AddedNode {
        label: key.label(),
        node_name: added.node_name,
        node_tag: added.node_tag,
        purpose: purpose.clone(),
    }))
}

/// Builds the nodes this machine added, [`MAX_CONCURRENT_LOCAL_BUILDS`] at a
/// time (see [`build_added_nodes`]).
///
/// Stack launch chains directly from add into build, since the launcher's
/// contract is "the stack is up and running"; an `Added` entity isn't
/// actually buildable from the user's perspective until `node build` has
/// run.
async fn build_locally(
    ctx: &StackChangeContext,
    phase: &PhaseGoal,
    nodes: &[AddedNode],
    outcome: &mut GroupOutcome,
) {
    build_added_nodes(
        nodes,
        ctx.bound_core_node.as_str(),
        MAX_CONCURRENT_LOCAL_BUILDS,
        |node| {
            build_node_directly(
                ctx,
                &phase.launch_id,
                node.node_name.clone(),
                node.node_tag.clone(),
                phase.rebuild,
            )
        },
        outcome,
    )
    .await;
}

/// Runs `build` on each of `nodes`, at most `max_at_once` at the same time,
/// starting them in the order of `nodes`, and records each build's log and
/// failure in `outcome`.
///
/// After a build fails, no other build starts, and the builds already running
/// finish: what they build is in the artifact cache for the next launch. The
/// failure recorded is the one of the first node of `nodes` that failed, and
/// the build logs are recorded in that order too, whichever build ended
/// first.
async fn build_added_nodes<F, Fut>(
    nodes: &[AddedNode],
    core_node: &str,
    max_at_once: usize,
    build: F,
    outcome: &mut GroupOutcome,
) where
    F: Fn(&AddedNode) -> Fut,
    Fut: Future<Output = (ChangeResult<()>, Option<PathBuf>)>,
{
    let mut waiting = nodes.iter().enumerate();
    let mut running = FuturesUnordered::new();
    let mut ended = Vec::with_capacity(nodes.len());
    let mut a_build_failed = false;
    loop {
        while !a_build_failed && running.len() < max_at_once {
            let Some((position, node)) = waiting.next() else {
                break;
            };
            running.push(build_one(position, node, core_node, &build));
        }
        let Some(ended_build) = running.next().await else {
            break;
        };
        a_build_failed |= ended_build.failure.is_some();
        ended.push(ended_build);
    }

    ended.sort_by_key(|ended_build| ended_build.position);
    for ended_build in ended {
        outcome.build_logs.extend(ended_build.log);
        if let Some(reason) = ended_build.failure {
            outcome.failure.get_or_insert(reason);
        }
    }
}

/// What one build of [`build_added_nodes`] ended with.
struct EndedBuild {
    /// The node's position in the add order.
    position: usize,
    log: Option<NodeBuildLogEntry>,
    failure: Option<Failure>,
}

async fn build_one<F, Fut>(
    position: usize,
    node: &AddedNode,
    core_node: &str,
    build: &F,
) -> EndedBuild
where
    F: Fn(&AddedNode) -> Fut,
    Fut: Future<Output = (ChangeResult<()>, Option<PathBuf>)>,
{
    let (result, log_path) = build(node).await;
    EndedBuild {
        position,
        log: log_path.map(|path| NodeBuildLogEntry {
            node_label: node.label.clone(),
            log_path: path,
            failed: result.is_err(),
            core_node: core_node.to_owned(),
        }),
        failure: result
            .err()
            .map(|failure| node_failure("build", &node.label, node.purpose.as_deref(), &failure)),
    }
}

/// Adds and builds one node on a peer, over the wire.
///
/// The goal carries the same pinned source and closure pins the local arm
/// uses: the peer materializes the coordinator's decision, reusing its own
/// content on a fingerprint match and fetching the pinned commit otherwise,
/// and never resolves a name against its own cache.
///
/// Each accepted goal's log entry lands in this launch's log lists exactly as
/// a local one does, stamped with the peer's core node because the path names
/// a file on that machine's filesystem. The peer's output is also relayed
/// live into this launch's feedback stream, attributed to its core node.
///
/// Neither goal carries the caller's forwarded environment. Those values
/// (PATH first among them) describe the coordinator's machine, and a build
/// that resolves its tools through another machine's PATH fails on hosts
/// that have the toolchain installed. The peer's daemon supplies its own
/// environment to whatever these goals spawn.
async fn add_and_build_remotely(
    ctx: &StackChangeContext,
    phase: &PhaseGoal,
    core_node: &CoreNodeName,
    key: &NodeKey,
    item: &PlannedDeployment,
    outcome: &mut GroupOutcome,
) -> ChangeResult<()> {
    // A real budget, unlike the in-process path's zero: this goal passes
    // through the peer's own concurrency gate, which reports the remaining time
    // when it refuses a second caller.
    let add_goal = NodeAddGoal::from_source(
        pinned_source(key, item).map_err(attempt_failed)?,
        STACK_LAUNCH_GIT_HASH,
        ctx.idle_timeouts.add.as_secs(),
    )
    .with_launch_id(&phase.launch_id)
    .with_pins(encode_pins(&item.closure_pins).map_err(attempt_failed)?);
    let added =
        match federated::run_remote_goal(ctx, core_node.as_str(), &add_goal, ctx.idle_timeouts.add)
            .await
        {
            Ok(run) => {
                outcome.add_logs.push(NodeAddLogEntry {
                    node_label: key.label(),
                    log_path: run.log_path,
                    failed: run.outcome.is_err(),
                    core_node: core_node.to_string(),
                });
                if matches!(run.outcome, Err(RemoteGoalFailure::Unresolved(_))) {
                    outcome.unresolved.push(UnresolvedAdd {
                        hosted: HostedNode {
                            node: key.clone(),
                            core_node: core_node.clone(),
                        },
                        config_sha256: item.config_sha256.clone(),
                    });
                }
                run.outcome.map_err(|failure| failure.failure().clone())
            }
            Err(failure) => Err(failure),
        }
        .map_err(|failure| {
            rephrased(
                &failure,
                format!(
                    "failed to add node {}: {}",
                    item.node_name,
                    failure.message()
                ),
            )
        })?;

    if is_built_in(item) {
        return Ok(());
    }

    let build_goal = remote_build_goal(
        phase,
        added.node_name,
        added.node_tag,
        ctx.idle_timeouts.build,
    );
    match federated::run_remote_goal(
        ctx,
        core_node.as_str(),
        &build_goal,
        ctx.idle_timeouts.build,
    )
    .await
    {
        Ok(run) => {
            outcome.build_logs.push(NodeBuildLogEntry {
                node_label: key.label(),
                log_path: run.log_path,
                failed: run.outcome.is_err(),
                core_node: core_node.to_string(),
            });
            run.outcome.map_err(|failure| failure.failure().clone())
        }
        Err(failure) => Err(failure),
    }
    .map_err(|failure| {
        rephrased(
            &failure,
            format!(
                "failed to build node {}: {}",
                item.node_name,
                failure.message()
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_node_api::encoding::LaunchGoal;
    use std::sync::Mutex;
    use tokio::sync::{mpsc, oneshot};

    fn added(name: &str) -> AddedNode {
        AddedNode {
            label: format!("{name}:v1"),
            node_name: name.to_owned(),
            node_tag: "v1".to_owned(),
            purpose: None,
        }
    }

    type BuildResult = ChangeResult<()>;

    /// A build that failed with `reason`.
    fn failed(reason: &str) -> BuildResult {
        Err(attempt_failed(reason))
    }

    /// The message of the failure `outcome` records.
    fn failure_message(outcome: &GroupOutcome) -> Option<&str> {
        outcome.failure.as_ref().map(Failure::message)
    }

    /// The ends of the [`HeldBuilds`] by node name: [`finish`] ends one.
    type Enders = HashMap<String, oneshot::Sender<BuildResult>>;

    /// Fake builds a test drives by hand: each build reports its node's name
    /// on `started` when it starts, then waits until the test ends it with
    /// [`finish`].
    struct HeldBuilds {
        started: mpsc::UnboundedSender<String>,
        endings: Mutex<HashMap<String, oneshot::Receiver<BuildResult>>>,
    }

    impl HeldBuilds {
        fn new(names: &[&str]) -> (Self, mpsc::UnboundedReceiver<String>, Enders) {
            let (started, started_rx) = mpsc::unbounded_channel();
            let mut endings = HashMap::new();
            let mut enders = HashMap::new();
            for name in names {
                let (ender, ending) = oneshot::channel();
                endings.insert((*name).to_owned(), ending);
                enders.insert((*name).to_owned(), ender);
            }
            let builds = Self {
                started,
                endings: Mutex::new(endings),
            };
            (builds, started_rx, enders)
        }

        async fn build(&self, node_name: String) -> (BuildResult, Option<PathBuf>) {
            let ending = self
                .endings
                .lock()
                .unwrap()
                .remove(&node_name)
                .expect("each node builds once");
            self.started.send(node_name.clone()).unwrap();
            let result = ending.await.expect("the test ends every build it starts");
            (
                result,
                Some(PathBuf::from(format!("/logs/{node_name}.log"))),
            )
        }
    }

    fn finish(enders: &mut Enders, name: &str, result: BuildResult) {
        enders.remove(name).unwrap().send(result).unwrap();
    }

    fn logged(outcome: &GroupOutcome) -> Vec<(String, bool)> {
        outcome
            .build_logs
            .iter()
            .map(|entry| (entry.node_label.clone(), entry.failed))
            .collect()
    }

    #[tokio::test]
    async fn two_builds_run_at_once_and_the_third_starts_when_one_ends() {
        let nodes = [added("waldo"), added("backbone"), added("camera")];
        let (builds, mut started, mut enders) = HeldBuilds::new(&["waldo", "backbone", "camera"]);
        let mut outcome = GroupOutcome::default();

        let run = build_added_nodes(
            &nodes,
            "here",
            2,
            |node| builds.build(node.node_name.clone()),
            &mut outcome,
        );
        let drive = async {
            assert_eq!(started.recv().await.unwrap(), "waldo");
            assert_eq!(started.recv().await.unwrap(), "backbone");
            tokio::task::yield_now().await;
            assert!(
                started.try_recv().is_err(),
                "a third build must wait for a free slot"
            );

            finish(&mut enders, "backbone", Ok(()));
            assert_eq!(started.recv().await.unwrap(), "camera");
            finish(&mut enders, "camera", Ok(()));
            finish(&mut enders, "waldo", Ok(()));
        };
        tokio::join!(run, drive);

        assert_eq!(outcome.failure, None);
        assert_eq!(
            logged(&outcome),
            [
                ("waldo:v1".to_owned(), false),
                ("backbone:v1".to_owned(), false),
                ("camera:v1".to_owned(), false),
            ]
        );
        assert!(
            outcome
                .build_logs
                .iter()
                .all(|entry| entry.core_node == "here")
        );
    }

    #[tokio::test]
    async fn no_build_starts_after_a_failure_and_the_running_build_finishes() {
        let nodes = [added("waldo"), added("backbone"), added("camera")];
        let (builds, mut started, mut enders) = HeldBuilds::new(&["waldo", "backbone", "camera"]);
        let mut outcome = GroupOutcome::default();

        let run = build_added_nodes(
            &nodes,
            "here",
            2,
            |node| builds.build(node.node_name.clone()),
            &mut outcome,
        );
        let drive = async {
            assert_eq!(started.recv().await.unwrap(), "waldo");
            assert_eq!(started.recv().await.unwrap(), "backbone");
            finish(&mut enders, "backbone", failed("cargo failed"));
            tokio::task::yield_now().await;
            finish(&mut enders, "waldo", Ok(()));
        };
        tokio::join!(run, drive);

        assert!(
            started.try_recv().is_err(),
            "camera must not start after backbone failed"
        );
        assert_eq!(
            failure_message(&outcome),
            Some("failed to build node backbone:v1: cargo failed")
        );
        assert_eq!(
            logged(&outcome),
            [
                ("waldo:v1".to_owned(), false),
                ("backbone:v1".to_owned(), true),
            ]
        );
    }

    #[tokio::test]
    async fn the_failure_reported_is_the_first_node_s_whichever_failed_first() {
        let nodes = [added("waldo"), added("backbone")];
        let (builds, mut started, mut enders) = HeldBuilds::new(&["waldo", "backbone"]);
        let mut outcome = GroupOutcome::default();

        let run = build_added_nodes(
            &nodes,
            "here",
            2,
            |node| builds.build(node.node_name.clone()),
            &mut outcome,
        );
        let drive = async {
            assert_eq!(started.recv().await.unwrap(), "waldo");
            assert_eq!(started.recv().await.unwrap(), "backbone");
            finish(&mut enders, "backbone", failed("second"));
            tokio::task::yield_now().await;
            finish(&mut enders, "waldo", failed("first"));
        };
        tokio::join!(run, drive);

        assert_eq!(
            failure_message(&outcome),
            Some("failed to build node waldo:v1: first")
        );
        assert_eq!(
            logged(&outcome),
            [
                ("waldo:v1".to_owned(), true),
                ("backbone:v1".to_owned(), true),
            ]
        );
    }

    #[tokio::test]
    async fn one_build_at_a_time_builds_in_order() {
        let nodes = [added("waldo"), added("backbone")];
        let (builds, mut started, mut enders) = HeldBuilds::new(&["waldo", "backbone"]);
        let mut outcome = GroupOutcome::default();

        let run = build_added_nodes(
            &nodes,
            "here",
            1,
            |node| builds.build(node.node_name.clone()),
            &mut outcome,
        );
        let drive = async {
            assert_eq!(started.recv().await.unwrap(), "waldo");
            tokio::task::yield_now().await;
            assert!(started.try_recv().is_err());
            finish(&mut enders, "waldo", Ok(()));
            assert_eq!(started.recv().await.unwrap(), "backbone");
            finish(&mut enders, "backbone", Ok(()));
        };
        tokio::join!(run, drive);

        assert_eq!(outcome.failure, None);
        assert_eq!(outcome.build_logs.len(), 2);
    }

    #[tokio::test]
    async fn a_joinable_build_failure_says_what_the_node_is_built_for_and_how_to_skip_it() {
        let node = AddedNode {
            purpose: Some("for stack joins of `so101_sim`".to_owned()),
            ..added("so101_backbone")
        };
        let mut outcome = GroupOutcome::default();

        build_added_nodes(
            std::slice::from_ref(&node),
            "here",
            2,
            |_| async { (failed("cargo failed"), None) },
            &mut outcome,
        )
        .await;

        assert_eq!(
            failure_message(&outcome),
            Some(
                "failed to build node so101_backbone:v1 for stack joins of `so101_sim`: cargo \
                 failed; `--no-prebuild` skips the nodes built for stack joins"
            )
        );
        let refusal = node_failure(
            "add",
            "waldo:v1",
            None,
            &crate::services::stack::rejected("no such node"),
        );
        assert_eq!(
            refusal.message(),
            "failed to add node waldo:v1: no such node",
            "a node the change runs names no flag"
        );
        assert_eq!(
            refusal.kind().as_str(),
            "rejected",
            "the failure keeps the kind of the add's own"
        );
    }

    fn name(value: &str) -> config::runtime::Name {
        config::runtime::Name::new(value).expect("a name")
    }

    /// The add line of every node of each machine's group, by machine.
    fn add_lines(
        groups: &BTreeMap<CoreNodeName, Vec<GroupNode<'_>>>,
    ) -> Vec<(String, Vec<String>)> {
        groups
            .iter()
            .map(|(core_node, nodes)| {
                (
                    core_node.to_string(),
                    nodes.iter().map(|node| node.add_line(core_node)).collect(),
                )
            })
            .collect()
    }

    /// The coordinator adds each joinable node after the launch's own nodes
    /// there, and once: a node the launch adds on the coordinator is not
    /// added again, and a node the launch places only on another machine is
    /// added on the coordinator too.
    #[test]
    fn the_coordinator_adds_each_joinable_node_it_does_not_already_add() {
        use crate::services::stack::fixtures::{placements_with, planned_deployment};
        let engine = planned_deployment("waldo", &[("waldo_inst", None)]);
        let shared = planned_deployment("initializer", &[("alpha_init_inst", None)]);
        let elsewhere = planned_deployment("camera", &[("alpha_cam_inst", None)]);
        let brain = planned_deployment("brain", &[("composition_check_brain_inst", None)]);
        let placements = placements_with("cn-desk", &[("alpha_cam_inst", "cn-gpu")]);
        let ordered = [engine.key(), shared.key(), elsewhere.key()];
        let planned_by_key: HashMap<NodeKey, PlannedDeployment> =
            [engine, shared.clone(), elsewhere.clone()]
                .into_iter()
                .map(|item| (item.key(), item))
                .collect();
        let builds = [
            JoinableBuild::new(shared, vec![name("openarm_sim"), name("so101_sim")]),
            JoinableBuild::new(elsewhere, vec![name("openarm_sim")]),
            JoinableBuild::new(brain, vec![name("openarm_sim")]),
        ];
        let copies = daemon_config::launcher::CopyMembership::default();
        let launch = PhaseChange::Launch {
            copies: &copies,
            joinable: &builds,
        };

        let groups = add_groups(launch, &ordered, &planned_by_key, &placements);

        assert_eq!(
            add_lines(&groups),
            [
                (
                    "cn-desk".to_owned(),
                    vec![
                        "Adding waldo:v1 on `cn-desk`".to_owned(),
                        "Adding initializer:v1 on `cn-desk`".to_owned(),
                        "Adding camera:v1 on `cn-desk` for stack joins of `openarm_sim`".to_owned(),
                        "Adding brain:v1 on `cn-desk` for stack joins of `openarm_sim`".to_owned(),
                    ]
                ),
                (
                    "cn-gpu".to_owned(),
                    vec!["Adding camera:v1 on `cn-gpu`".to_owned()]
                ),
            ]
        );

        // With no joinable node, a coordinator that hosts nothing adds
        // nothing.
        let launch = PhaseChange::Launch {
            copies: &copies,
            joinable: &[],
        };
        let groups = add_groups(launch, &ordered[2..], &planned_by_key, &placements);
        assert_eq!(
            add_lines(&groups),
            [(
                "cn-gpu".to_owned(),
                vec!["Adding camera:v1 on `cn-gpu`".to_owned()]
            )]
        );
    }

    #[test]
    fn remote_build_goal_carries_the_launch_rebuild_switch() {
        let launch = LaunchGoal::new(
            core_node_api::encoding::LauncherOrigin::repository("openarm_v2"),
            "launch-abc123",
            core_node_api::encoding::StackBudgets::new(1, 1, 1, None),
        )
        .with_rebuild(true);

        let build = remote_build_goal(
            &PhaseGoal {
                launch_id: launch.launch_id.clone(),
                rebuild: launch.rebuild,
            },
            "waldo".to_string(),
            "v1".to_string(),
            Duration::from_secs(90),
        );

        assert_eq!(build.node_name, "waldo");
        assert_eq!(build.node_tag, "v1");
        assert_eq!(build.timeout_secs, 90);
        assert_eq!(build.launch_id.as_deref(), Some("launch-abc123"));
        assert!(build.rebuild, "the launch's --rebuild reaches the peer");
        assert!(
            !build.force,
            "a dispatched build never supersedes a peer's own build"
        );

        let plain = remote_build_goal(
            &PhaseGoal {
                launch_id: launch.launch_id.clone(),
                rebuild: false,
            },
            "waldo".to_string(),
            "v1".to_string(),
            Duration::from_secs(90),
        );
        assert!(!plain.rebuild);
    }
}
