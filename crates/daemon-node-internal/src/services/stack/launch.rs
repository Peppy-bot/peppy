use crate::Result;
use crate::encoding::{
    LaunchFeedback, LaunchFeedbackStep, LaunchGoal, LaunchGoalResponse, LaunchResult,
    NodeAddFeedback, NodeAddGoal, NodeAddGoalResponse, NodeAddResult, NodeSource,
    NodeStartFeedback, NodeStartGoal, NodeStartGoalResponse, NodeStartResult,
};
use crate::names;
use crate::services::node::resolve_node_config;
use chrono::Local;
use config::consts::{DEFAULT_MESSAGING_HOST, DEFAULT_MESSAGING_PORT, logs_dir_launch};
use config::peppy_config::{Deployment, DeploymentSource, PeppyLauncherParser};
use config::runtime::RuntimeConfig;
use node_stack::NodeStack;
use peppylib::messaging::{ActionCreation, ServiceRequestContext, TopicPublisher};
use peppylib::types::Payload;
use peppylib::{ActionMessenger, MessengerHandle, PeppyResult};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::debug;

pub async fn listen_for_stack_launch(
    messenger: &MessengerHandle,
    daemon_node_name: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    _node_startup_timeout: Duration,
    _node_start_health_timeout: Duration,
) -> Result<JoinHandle<Result<()>>> {
    let action = ActionMessenger::expose(
        messenger,
        daemon_node_name,
        instance_id,
        node_name,
        names::STACK_LAUNCH_ACTION,
    )
    .await?;

    let handle = tokio::spawn({
        let messenger = messenger.clone();
        let bound_daemon_node = daemon_node_name.to_string();
        let daemon_instance_id = instance_id.to_string();
        async move {
            run_launch_action_loop(
                action,
                node_stack,
                messenger,
                bound_daemon_node,
                daemon_instance_id,
            )
            .await
        }
    });

    Ok(handle)
}

/// State for tracking the current launch action.
#[derive(Default)]
enum LaunchActionState {
    /// No action is currently running.
    #[default]
    Idle,
    /// The goal was rejected (no result polling expected).
    Rejected,
    /// An action is currently running.
    Running,
    /// The action completed and the result is ready to be sent.
    Completed { result: LaunchResult },
    /// The result has been sent to the requester.
    ResultSent { result: LaunchResult },
}

struct ProcessLaunchContext {
    messenger: MessengerHandle,
    bound_daemon_node: String,
    daemon_instance_id: String,
    node_stack: Arc<NodeStack>,
    feedback_publisher: TopicPublisher,
    log_file: Arc<StdMutex<File>>,
    log_path: PathBuf,
    env_vars: Vec<(String, String)>,
    node_add_timeout_secs: u64,
    node_start_timeout_secs: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct NodeKey {
    name: String,
    tag: String,
}

impl NodeKey {
    fn new(name: impl Into<String>, tag: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            tag: tag.into(),
        }
    }

    fn label(&self) -> String {
        format!("{}:{}", self.name, self.tag)
    }
}

#[derive(Clone)]
struct PlannedDeployment {
    deployment: Deployment,
    source: NodeSource,
    node_name: String,
    node_tag: String,
    config: config::node::NodeConfig,
}

fn deployment_label(deployment: &Deployment) -> String {
    match &deployment.source {
        DeploymentSource::Local(spec) => format!("local:{}", spec.local.display()),
        DeploymentSource::Git(spec) => format!("git:{}@{}:{}", spec.repo, spec.ref_, spec.path),
        DeploymentSource::Url(spec) => format!("url:{}", spec.url),
    }
}

fn git_url_from_repo(repo: &str) -> std::result::Result<gix_url::Url, String> {
    gix_url::Url::try_from(repo)
        .or_else(|_| gix_url::Url::try_from(std::path::Path::new(repo)))
        .map_err(|e| format!("invalid git repo URL `{repo}`: {e}"))
}

fn node_source_from_deployment_source(
    deployment: &Deployment,
    nodes_directory: &std::path::Path,
) -> std::result::Result<NodeSource, String> {
    match &deployment.source {
        DeploymentSource::Local(spec) => {
            let resolved = if spec.local.is_absolute() {
                spec.local.clone()
            } else {
                nodes_directory.join(&spec.local)
            };
            Ok(NodeSource::Fs(resolved))
        }
        DeploymentSource::Git(spec) => {
            let repo_url = git_url_from_repo(&spec.repo)?;
            Ok(NodeSource::Git {
                repo_url,
                repo_path: spec.path.clone(),
                repo_ref: Some(spec.ref_.clone()),
            })
        }
        DeploymentSource::Url(spec) => {
            let url = url::Url::parse(&spec.url)
                .map_err(|e| format!("invalid HTTP URL `{}`: {e}", spec.url))?;
            Ok(NodeSource::Http { url })
        }
    }
}

/// Marker git_hash used for stack-launch operations.
/// When this marker is used, the node_add service skips git hash verification
/// and generates fresh peppygen files. This allows stack_launch to work with
/// local filesystem sources without requiring `peppy node sync` beforehand.
pub const STACK_LAUNCH_GIT_HASH: &str = "stack-launch";

async fn publish_feedback(ctx: &ProcessLaunchContext, feedback: LaunchFeedback) {
    if let Ok(mut file) = ctx.log_file.lock() {
        let timestamp = Local::now().format("%Y-%m-%dT%H:%M:%S%.3f");
        let _ = writeln!(
            file,
            "[{}] [{}] {}",
            timestamp, feedback.stream, feedback.line
        );
    }

    if let Ok(payload) = feedback.encode() {
        let _ = ctx.feedback_publisher.publish(payload).await;
    }
}

async fn publish_stdout(
    ctx: &ProcessLaunchContext,
    line: impl Into<String>,
    step: LaunchFeedbackStep,
) {
    publish_feedback(ctx, LaunchFeedback::stdout(line, step)).await;
}

async fn publish_stderr(
    ctx: &ProcessLaunchContext,
    line: impl Into<String>,
    step: LaunchFeedbackStep,
) {
    publish_feedback(ctx, LaunchFeedback::stderr(line, step)).await;
}

async fn run_node_add_and_forward_feedback(
    ctx: &ProcessLaunchContext,
    node_add_goal: &NodeAddGoal,
    goal_timeout: Duration,
    result_timeout: Duration,
) -> std::result::Result<NodeAddResult, String> {
    let goal_payload = node_add_goal
        .encode()
        .map_err(|e| format!("failed to encode node_add goal: {e}"))?;

    let mut action_handle = ActionMessenger::send_goal(
        &ctx.messenger,
        &ctx.bound_daemon_node,
        &ctx.daemon_instance_id,
        &ctx.bound_daemon_node,
        names::NODE_ADD_ACTION,
        None,
        None,
        goal_payload,
        config::node::QoSProfile::default(),
        goal_timeout,
    )
    .await
    .map_err(|e| format!("failed to send node_add goal: {e}"))?;

    let goal_response_payload = action_handle.goal_response().payload();
    let goal_response = NodeAddGoalResponse::decode(&goal_response_payload)
        .map_err(|e| format!("failed to decode node_add goal response: {e}"))?;

    if !goal_response.accepted {
        return Err(goal_response
            .rejection_reason
            .unwrap_or_else(|| "node_add goal rejected".to_string()));
    }

    let deadline = tokio::time::Instant::now() + result_timeout;

    loop {
        // Drain feedback so the publisher doesn't block on a full channel.
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err("timeout waiting for node_add result".to_string());
            }
            let remaining = deadline - now;
            let drain_timeout = Duration::from_millis(50).min(remaining);
            match tokio::time::timeout(drain_timeout, action_handle.on_next_feedback()).await {
                Ok(Ok(msg)) => {
                    let payload = msg.payload();
                    if let Ok(feedback) = NodeAddFeedback::decode(&payload) {
                        let launch_feedback = if feedback.is_stdout() {
                            LaunchFeedback::stdout(feedback.line, LaunchFeedbackStep::AddingNode)
                        } else {
                            LaunchFeedback::stderr(feedback.line, LaunchFeedbackStep::AddingNode)
                        };
                        publish_feedback(ctx, launch_feedback).await;
                    }
                }
                Ok(Err(_)) => break,
                Err(_) => break,
            }
        }

        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err("timeout waiting for node_add result".to_string());
        }
        let remaining = deadline - now;
        let poll_timeout = Duration::from_millis(200).min(remaining);

        match ActionMessenger::request_result(&ctx.messenger, &action_handle, poll_timeout).await {
            Ok(msg) => {
                let payload = msg.payload();
                match NodeAddResult::decode(&payload) {
                    Ok(result) => {
                        // Drain any remaining feedback that may have arrived while polling.
                        loop {
                            let Ok(Some(msg)) = action_handle.try_next_feedback() else {
                                break;
                            };
                            let payload = msg.payload();
                            if let Ok(feedback) = NodeAddFeedback::decode(&payload) {
                                let launch_feedback = if feedback.is_stdout() {
                                    LaunchFeedback::stdout(
                                        feedback.line,
                                        LaunchFeedbackStep::AddingNode,
                                    )
                                } else {
                                    LaunchFeedback::stderr(
                                        feedback.line,
                                        LaunchFeedbackStep::AddingNode,
                                    )
                                };
                                publish_feedback(ctx, launch_feedback).await;
                            }
                        }
                        return Ok(result);
                    }
                    Err(err) => {
                        let pending = std::str::from_utf8(payload.as_ref())
                            .map(|text| text.starts_with("result pending"))
                            .unwrap_or(false);
                        if !pending {
                            return Err(format!("failed to decode node_add result: {err}"));
                        }
                    }
                }
            }
            Err(peppylib::PeppyError::ActionResultTimeout { .. }) => {}
            Err(err) => return Err(format!("failed to get node_add result: {err}")),
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn run_node_start_and_forward_feedback(
    ctx: &ProcessLaunchContext,
    node_start_goal: &NodeStartGoal,
    goal_timeout: Duration,
    result_timeout: Duration,
) -> std::result::Result<NodeStartResult, String> {
    let goal_payload = node_start_goal
        .encode()
        .map_err(|e| format!("failed to encode node_start goal: {e}"))?;

    let mut action_handle = ActionMessenger::send_goal(
        &ctx.messenger,
        &ctx.bound_daemon_node,
        &ctx.daemon_instance_id,
        &ctx.bound_daemon_node,
        names::NODE_START_ACTION,
        None,
        None,
        goal_payload,
        config::node::QoSProfile::default(),
        goal_timeout,
    )
    .await
    .map_err(|e| format!("failed to send node_start goal: {e}"))?;

    let goal_response_payload = action_handle.goal_response().payload();
    let goal_response = NodeStartGoalResponse::decode(&goal_response_payload)
        .map_err(|e| format!("failed to decode node_start goal response: {e}"))?;

    if !goal_response.accepted {
        return Err(goal_response
            .rejection_reason
            .unwrap_or_else(|| "node_start goal rejected".to_string()));
    }

    let deadline = tokio::time::Instant::now() + result_timeout;

    loop {
        // Drain feedback so the publisher doesn't block on a full channel.
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err("timeout waiting for node_start result".to_string());
            }
            let remaining = deadline - now;
            let drain_timeout = Duration::from_millis(50).min(remaining);
            match tokio::time::timeout(drain_timeout, action_handle.on_next_feedback()).await {
                Ok(Ok(msg)) => {
                    let payload = msg.payload();
                    if let Ok(feedback) = NodeStartFeedback::decode(&payload) {
                        let launch_feedback = if feedback.is_stdout() {
                            LaunchFeedback::stdout(feedback.line, LaunchFeedbackStep::StartingNode)
                        } else {
                            LaunchFeedback::stderr(feedback.line, LaunchFeedbackStep::StartingNode)
                        };
                        publish_feedback(ctx, launch_feedback).await;
                    }
                }
                Ok(Err(_)) => break,
                Err(_) => break,
            }
        }

        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err("timeout waiting for node_start result".to_string());
        }
        let remaining = deadline - now;
        let poll_timeout = Duration::from_millis(200).min(remaining);

        match ActionMessenger::request_result(&ctx.messenger, &action_handle, poll_timeout).await {
            Ok(msg) => {
                let payload = msg.payload();
                match NodeStartResult::decode(&payload) {
                    Ok(result) => {
                        // Drain any remaining feedback that may have arrived while polling.
                        loop {
                            let Ok(Some(msg)) = action_handle.try_next_feedback() else {
                                break;
                            };
                            let payload = msg.payload();
                            if let Ok(feedback) = NodeStartFeedback::decode(&payload) {
                                let launch_feedback = if feedback.is_stdout() {
                                    LaunchFeedback::stdout(
                                        feedback.line,
                                        LaunchFeedbackStep::StartingNode,
                                    )
                                } else {
                                    LaunchFeedback::stderr(
                                        feedback.line,
                                        LaunchFeedbackStep::StartingNode,
                                    )
                                };
                                publish_feedback(ctx, launch_feedback).await;
                            }
                        }
                        return Ok(result);
                    }
                    Err(err) => {
                        let pending = std::str::from_utf8(payload.as_ref())
                            .map(|text| text.starts_with("result pending"))
                            .unwrap_or(false);
                        if !pending {
                            return Err(format!("failed to decode node_start result: {err}"));
                        }
                    }
                }
            }
            Err(peppylib::PeppyError::ActionResultTimeout { .. }) => {}
            Err(err) => return Err(format!("failed to get node_start result: {err}")),
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn restore_stack(
    ctx: &ProcessLaunchContext,
    backup: &NodeStack,
    reason: String,
) -> LaunchResult {
    publish_stderr(
        ctx,
        format!("Launch failed: {reason}"),
        LaunchFeedbackStep::LauncherStep,
    )
    .await;

    if let Err(err) = ctx.node_stack.apply_from(backup) {
        let msg = format!("{reason}\n(also failed to restore previous stack: {err})");
        return LaunchResult::failure(&ctx.log_path, msg);
    }

    LaunchResult::failure(&ctx.log_path, reason)
}

/// Step 1: Parse launcher configuration from file path.
async fn parse_launcher_config(
    ctx: &ProcessLaunchContext,
    goal: &LaunchGoal,
) -> std::result::Result<(Vec<Deployment>, PathBuf), LaunchResult> {
    publish_stdout(
        ctx,
        "Parsing launcher configuration",
        LaunchFeedbackStep::LauncherStep,
    )
    .await;

    if !goal.peppy_launch_file_path.exists() {
        let msg = format!(
            "launch file does not exist: {}",
            goal.peppy_launch_file_path.display()
        );
        publish_stderr(ctx, &msg, LaunchFeedbackStep::LauncherStep).await;
        return Err(LaunchResult::failure(&ctx.log_path, msg));
    }

    if !goal.peppy_launch_file_path.is_file() {
        let msg = format!(
            "launch file path must be a file: {}",
            goal.peppy_launch_file_path.display()
        );
        publish_stderr(ctx, &msg, LaunchFeedbackStep::LauncherStep).await;
        return Err(LaunchResult::failure(&ctx.log_path, msg));
    }

    let peppy_launcher = match PeppyLauncherParser::from_path(&goal.peppy_launch_file_path) {
        Ok(cfg) => cfg,
        Err(e) => {
            publish_stderr(
                ctx,
                format!("Invalid launcher config: {e}"),
                LaunchFeedbackStep::LauncherStep,
            )
            .await;
            return Err(LaunchResult::failure(
                &ctx.log_path,
                format!("Invalid launcher config: {e}"),
            ));
        }
    };

    // Use the parent directory of the launch file as the nodes_directory.
    let nodes_directory = goal
        .peppy_launch_file_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    let deployments = peppy_launcher.deployments.clone();
    Ok((deployments, nodes_directory))
}

/// Step 2: Resolve deployments - retrieve node configs for each deployment.
async fn resolve_deployments(
    ctx: &ProcessLaunchContext,
    deployments: Vec<Deployment>,
    nodes_directory: &Path,
) -> std::result::Result<Vec<PlannedDeployment>, LaunchResult> {
    publish_stdout(
        ctx,
        format!("Resolving {} deployment(s)", deployments.len()),
        LaunchFeedbackStep::LauncherStep,
    )
    .await;

    let mut planned: Vec<PlannedDeployment> = Vec::new();
    let mut planning_errors: Vec<String> = Vec::new();
    let mut planned_keys: HashSet<NodeKey> = HashSet::new();

    for deployment in deployments.into_iter() {
        if deployment.instances.is_empty() {
            planning_errors.push(format!(
                "deployment {} must have at least one instance",
                deployment_label(&deployment)
            ));
            continue;
        }

        let source = match node_source_from_deployment_source(&deployment, nodes_directory) {
            Ok(source) => source,
            Err(err) => {
                planning_errors.push(format!(
                    "failed to resolve source for deployment {}: {err}",
                    deployment_label(&deployment)
                ));
                continue;
            }
        };

        publish_stdout(
            ctx,
            format!(
                "Retrieving node config for {}",
                deployment_label(&deployment)
            ),
            LaunchFeedbackStep::LauncherStep,
        )
        .await;

        let config = match resolve_node_config(source.clone()).await {
            Ok(config) => config,
            Err(err) => {
                planning_errors.push(format!(
                    "failed to retrieve node config for deployment {}: {err}",
                    deployment_label(&deployment)
                ));
                continue;
            }
        };

        let node_name = config.manifest.name.as_str().to_owned();
        let node_tag = config.manifest.tag.clone();

        let key = NodeKey::new(&node_name, &node_tag);
        if !planned_keys.insert(key.clone()) {
            planning_errors.push(format!(
                "duplicate deployment for node {} (resolved from {})",
                key.label(),
                deployment_label(&deployment)
            ));
            continue;
        }

        publish_stdout(
            ctx,
            format!(
                "Deployment {} resolved to {}:{}",
                deployment_label(&deployment),
                node_name,
                node_tag
            ),
            LaunchFeedbackStep::LauncherStep,
        )
        .await;

        planned.push(PlannedDeployment {
            deployment,
            source,
            node_name,
            node_tag,
            config,
        });
    }

    if !planning_errors.is_empty() {
        let msg = planning_errors.join("\n");
        publish_stderr(ctx, msg.clone(), LaunchFeedbackStep::LauncherStep).await;
        return Err(LaunchResult::failure(&ctx.log_path, msg));
    }

    Ok(planned)
}

/// Step 3: Validate dependencies and compute a stable topological order.
async fn validate_and_order_dependencies(
    ctx: &ProcessLaunchContext,
    planned: &[PlannedDeployment],
    root_config: &config::node::NodeConfig,
) -> std::result::Result<Vec<NodeKey>, LaunchResult> {
    publish_stdout(
        ctx,
        "Validating dependencies",
        LaunchFeedbackStep::LauncherStep,
    )
    .await;

    let root_key = NodeKey::new(
        root_config.manifest.name.as_str(),
        root_config.manifest.tag.as_str(),
    );

    let mut configs_by_key: HashMap<NodeKey, config::node::NodeConfig> = HashMap::new();
    configs_by_key.insert(root_key.clone(), root_config.clone());
    for item in planned {
        configs_by_key.insert(
            NodeKey::new(&item.node_name, &item.node_tag),
            item.config.clone(),
        );
    }

    let planned_keys: HashSet<NodeKey> = planned
        .iter()
        .map(|p| NodeKey::new(&p.node_name, &p.node_tag))
        .collect();

    // Validate all dependencies exist and expose the required interfaces.
    let dependency_errors: Vec<String> = planned
        .iter()
        .flat_map(|item| {
            node_stack::validate_dependency_specs(
                &item.config,
                &item.node_name,
                &item.node_tag,
                |name, tag| configs_by_key.get(&NodeKey::new(name, tag)).cloned(),
            )
        })
        .map(|e| e.to_string())
        .collect();

    if !dependency_errors.is_empty() {
        let msg = dependency_errors.join("\n");
        publish_stderr(ctx, msg.clone(), LaunchFeedbackStep::LauncherStep).await;
        return Err(LaunchResult::failure(&ctx.log_path, msg));
    }

    // Build the dependency graph for topological ordering.
    let mut deps_for: HashMap<NodeKey, HashSet<NodeKey>> = HashMap::new();
    for item in planned {
        let dependant_key = NodeKey::new(&item.node_name, &item.node_tag);
        let mut deps = HashSet::new();
        for spec in node_stack::collect_dependency_specs(&item.config) {
            let dep_key = NodeKey::new(&spec.node_name, &spec.node_tag);
            if dep_key != root_key && planned_keys.contains(&dep_key) {
                deps.insert(dep_key);
            }
        }
        deps_for.insert(dependant_key, deps);
    }

    // Stable topological sort using original plan order as tie-breaker.
    let ordered = topological_sort(planned, &deps_for)?;

    publish_stdout(
        ctx,
        format!(
            "Dependency order: {}",
            ordered
                .iter()
                .map(|k| k.label())
                .collect::<Vec<_>>()
                .join(" -> ")
        ),
        LaunchFeedbackStep::LauncherStep,
    )
    .await;

    Ok(ordered)
}

/// Perform a stable topological sort.
fn topological_sort(
    planned: &[PlannedDeployment],
    deps_for: &HashMap<NodeKey, HashSet<NodeKey>>,
) -> std::result::Result<Vec<NodeKey>, LaunchResult> {
    let mut in_degree: HashMap<NodeKey, usize> = HashMap::new();
    let mut dependents: HashMap<NodeKey, Vec<NodeKey>> = HashMap::new();

    for key in planned
        .iter()
        .map(|p| NodeKey::new(&p.node_name, &p.node_tag))
    {
        in_degree.entry(key.clone()).or_insert(0);
        dependents.entry(key).or_default();
    }

    for (dependant, deps) in deps_for {
        in_degree.insert(dependant.clone(), deps.len());
        for dep in deps {
            dependents
                .entry(dep.clone())
                .or_default()
                .push(dependant.clone());
        }
    }

    let order_index: HashMap<NodeKey, usize> = planned
        .iter()
        .enumerate()
        .map(|(idx, p)| (NodeKey::new(&p.node_name, &p.node_tag), idx))
        .collect();

    let mut ready: Vec<NodeKey> = in_degree
        .iter()
        .filter(|(_, deg)| **deg == 0)
        .map(|(k, _)| k.clone())
        .collect();
    ready.sort_by_key(|k| order_index.get(k).copied().unwrap_or(usize::MAX));

    let mut queue: VecDeque<NodeKey> = ready.into();
    let mut ordered: Vec<NodeKey> = Vec::new();

    while let Some(node) = queue.pop_front() {
        ordered.push(node.clone());
        let Some(children) = dependents.get(&node) else {
            continue;
        };
        for child in children {
            if let Some(deg) = in_degree.get_mut(child) {
                *deg = deg.saturating_sub(1);
                if *deg == 0 {
                    queue.push_back(child.clone());
                }
            }
        }
        // Keep stable ordering when multiple nodes become ready at once.
        let mut drained: Vec<NodeKey> = queue.drain(..).collect();
        drained.sort_by_key(|k| order_index.get(k).copied().unwrap_or(usize::MAX));
        queue = drained.into();
    }

    if ordered.len() != planned.len() {
        let mut remaining: Vec<String> = in_degree
            .into_iter()
            .filter(|(_, deg)| *deg > 0)
            .map(|(k, _)| k.label())
            .collect();
        remaining.sort();
        let msg = format!(
            "unable to resolve dependency order (cycle suspected). Remaining nodes: {}",
            remaining.join(", ")
        );
        return Err(LaunchResult::failure(PathBuf::new(), msg));
    }

    Ok(ordered)
}

/// Step 4: Snapshot current stack and clear it.
async fn snapshot_and_clear_stack(
    ctx: &ProcessLaunchContext,
) -> std::result::Result<NodeStack, LaunchResult> {
    let backup_stack = {
        let root = ctx.node_stack.root();
        let backup = NodeStack::new(root.config().clone(), None, root.root_path());
        if let Err(err) = backup.apply_from(&ctx.node_stack) {
            let msg = format!("failed to snapshot current stack: {err}");
            publish_stderr(ctx, msg.clone(), LaunchFeedbackStep::LauncherStep).await;
            return Err(LaunchResult::failure(&ctx.log_path, msg));
        }
        backup
    };

    publish_stdout(
        ctx,
        "Clearing current node stack",
        LaunchFeedbackStep::LauncherStep,
    )
    .await;
    ctx.node_stack.reset();

    Ok(backup_stack)
}

/// Step 5: Add every node to the node stack in dependency order.
async fn add_nodes_to_stack(
    ctx: &ProcessLaunchContext,
    ordered: &[NodeKey],
    planned_by_key: &HashMap<NodeKey, PlannedDeployment>,
    backup_stack: &NodeStack,
) -> std::result::Result<(), LaunchResult> {
    publish_stdout(
        ctx,
        "Adding nodes to the stack...",
        LaunchFeedbackStep::LauncherStep,
    )
    .await;

    let node_add_result_timeout = Duration::from_secs(ctx.node_add_timeout_secs);
    let goal_timeout = Duration::from_secs(30);

    for key in ordered {
        let Some(item) = planned_by_key.get(key) else {
            continue;
        };

        publish_stdout(
            ctx,
            format!("Adding {}", key.label()),
            LaunchFeedbackStep::AddingNode,
        )
        .await;

        let timeout_secs = node_add_result_timeout.as_secs();
        let node_add_goal = match &item.source {
            NodeSource::Fs(path) => {
                NodeAddGoal::new(path.clone(), STACK_LAUNCH_GIT_HASH, timeout_secs)
            }
            NodeSource::Git {
                repo_url,
                repo_path,
                repo_ref,
            } => NodeAddGoal::new_git(
                repo_url.clone(),
                repo_path.clone(),
                repo_ref.clone(),
                STACK_LAUNCH_GIT_HASH,
                timeout_secs,
            ),
            NodeSource::Http { url } => {
                NodeAddGoal::new_http(url.clone(), STACK_LAUNCH_GIT_HASH, timeout_secs)
            }
        }
        .with_env_vars(ctx.env_vars.clone());

        match run_node_add_and_forward_feedback(
            ctx,
            &node_add_goal,
            goal_timeout,
            node_add_result_timeout,
        )
        .await
        {
            Ok(result) => {
                if !result.success {
                    let reason = result
                        .error_message
                        .unwrap_or_else(|| "node_add failed".to_string());
                    return Err(restore_stack(ctx, backup_stack, reason).await);
                }
            }
            Err(err) => {
                return Err(restore_stack(ctx, backup_stack, err).await);
            }
        }
    }

    Ok(())
}

/// Step 6: Start every instance in dependency order.
async fn start_node_instances(
    ctx: &ProcessLaunchContext,
    ordered: &[NodeKey],
    planned_by_key: &HashMap<NodeKey, PlannedDeployment>,
    backup_stack: &NodeStack,
) -> std::result::Result<(), LaunchResult> {
    publish_stdout(ctx, "Starting nodes...", LaunchFeedbackStep::LauncherStep).await;

    let node_start_result_timeout = Duration::from_secs(ctx.node_start_timeout_secs);
    let goal_timeout = Duration::from_secs(30);

    // Compute runtime config host/port.
    let (messaging_host, messaging_port) = ctx
        .messenger
        .messaging_endpoint()
        .await
        .unwrap_or((DEFAULT_MESSAGING_HOST.to_string(), DEFAULT_MESSAGING_PORT));

    for key in ordered {
        let Some(item) = planned_by_key.get(key) else {
            continue;
        };

        for instance in &item.deployment.instances {
            let instance_id = instance.instance_id.as_str();
            publish_stdout(
                ctx,
                format!("Starting {} instance {}", key.label(), instance_id),
                LaunchFeedbackStep::StartingNode,
            )
            .await;

            let node_instance = config::runtime::NodeInstance {
                instance_id: instance.instance_id.clone(),
                arguments: instance.arguments.clone(),
            };
            let runtime_config = match RuntimeConfig::new(
                messaging_host.as_str(),
                messaging_port,
                node_instance,
                item.node_name.as_str(),
                ctx.bound_daemon_node.as_str(),
            ) {
                Ok(cfg) => cfg,
                Err(e) => {
                    return Err(restore_stack(ctx, backup_stack, e.to_string()).await);
                }
            };

            let runtime_config_json5 = match serde_json5::to_string(&runtime_config) {
                Ok(json) => json,
                Err(e) => {
                    return Err(restore_stack(
                        ctx,
                        backup_stack,
                        format!("failed to serialize runtime config: {e}"),
                    )
                    .await);
                }
            };

            let node_start_goal = NodeStartGoal::new(
                &runtime_config_json5,
                item.node_name.as_str(),
                item.node_tag.as_str(),
                node_start_result_timeout.as_secs(),
            )
            .with_env_vars(ctx.env_vars.clone());

            match run_node_start_and_forward_feedback(
                ctx,
                &node_start_goal,
                goal_timeout,
                node_start_result_timeout,
            )
            .await
            {
                Ok(result) => {
                    if !result.success {
                        let reason = result
                            .error_message
                            .unwrap_or_else(|| "node_start failed".to_string());
                        return Err(restore_stack(ctx, backup_stack, reason).await);
                    }
                }
                Err(err) => {
                    return Err(restore_stack(ctx, backup_stack, err).await);
                }
            }
        }
    }

    Ok(())
}

async fn run_launch_action_loop(
    mut action: ActionCreation,
    node_stack: Arc<NodeStack>,
    messenger: MessengerHandle,
    bound_daemon_node: String,
    daemon_instance_id: String,
) -> Result<()> {
    let state = Arc::new(Mutex::new(LaunchActionState::default()));

    loop {
        // Wait for a goal request
        let goal_result = action
            .goal_service
            .handle_next_request({
                let feedback_publisher = &action.feedback_publisher;
                let node_stack = Arc::clone(&node_stack);
                let state = Arc::clone(&state);
                let messenger = messenger.clone();
                let bound_daemon_node = bound_daemon_node.clone();
                let daemon_instance_id = daemon_instance_id.clone();
                move |context| {
                    let feedback_publisher = feedback_publisher.clone();
                    let node_stack = Arc::clone(&node_stack);
                    let state = Arc::clone(&state);
                    let messenger = messenger.clone();
                    let bound_daemon_node = bound_daemon_node.clone();
                    let daemon_instance_id = daemon_instance_id.clone();

                    async move {
                        handle_goal_request(
                            context,
                            feedback_publisher,
                            node_stack,
                            state,
                            messenger,
                            bound_daemon_node,
                            daemon_instance_id,
                        )
                        .await
                    }
                }
            })
            .await;

        match goal_result {
            Ok(true) => {
                // Check if the goal was rejected (no result polling expected)
                {
                    let mut state_guard = state.lock().await;
                    if matches!(*state_guard, LaunchActionState::Rejected) {
                        // Goal was rejected, reset to Idle and wait for next goal
                        *state_guard = LaunchActionState::Idle;
                        continue;
                    }
                }

                // Goal accepted, now wait for result, cancel, or new goal requests.
                loop {
                    tokio::select! {
                        cancel_result = action.cancel_service.handle_next_request({
                            let state = Arc::clone(&state);
                            move |context| {
                                let state = Arc::clone(&state);
                                async move { handle_cancel_request(context, state).await }
                            }
                        }) => {
                            match cancel_result {
                                Ok(true) => {}
                                Ok(false) => return Ok(()),
                                Err(e) => {
                                    debug!("Cancel service error: {}", e);
                                    return Err(e.into());
                                }
                            }
                        }
                        result_result = action.result_service.handle_next_request({
                            let state = Arc::clone(&state);
                            move |context| {
                                let state = Arc::clone(&state);
                                async move { handle_result_request(context, state).await }
                            }
                        }) => {
                            match result_result {
                                Ok(true) => {
                                    // Only reset and accept a new goal after we've delivered the final result.
                                    let mut state_guard = state.lock().await;
                                    if matches!(*state_guard, LaunchActionState::ResultSent { .. }) {
                                        *state_guard = LaunchActionState::default();
                                        break;
                                    }
                                }
                                Ok(false) => return Ok(()),
                                Err(e) => {
                                    debug!("Result service error: {}", e);
                                    return Err(e.into());
                                }
                            }
                        }
                        goal_result = action.goal_service.handle_next_request({
                            let feedback_publisher = &action.feedback_publisher;
                            let node_stack = Arc::clone(&node_stack);
                            let state = Arc::clone(&state);
                            let messenger = messenger.clone();
                            let bound_daemon_node = bound_daemon_node.clone();
                            let daemon_instance_id = daemon_instance_id.clone();
                            move |context| {
                                let feedback_publisher = feedback_publisher.clone();
                                let node_stack = Arc::clone(&node_stack);
                                let state = Arc::clone(&state);
                                let messenger = messenger.clone();
                                let bound_daemon_node = bound_daemon_node.clone();
                                let daemon_instance_id = daemon_instance_id.clone();
                                async move {
                                    handle_goal_request(
                                        context,
                                        feedback_publisher,
                                        node_stack,
                                        state,
                                        messenger,
                                        bound_daemon_node,
                                        daemon_instance_id,
                                    )
                                    .await
                                }
                            }
                        }) => {
                            match goal_result {
                                Ok(true) => {
                                    let mut state_guard = state.lock().await;
                                    if matches!(*state_guard, LaunchActionState::Rejected) {
                                        *state_guard = LaunchActionState::Idle;
                                    }
                                }
                                Ok(false) => return Ok(()),
                                Err(e) => {
                                    debug!("Goal service error: {}", e);
                                    return Err(e.into());
                                }
                            }
                        }
                    }
                }
            }
            Ok(false) => {
                debug!("Goal service closed");
                return Ok(());
            }
            Err(e) => {
                debug!("Goal service error: {}", e);
                return Err(e.into());
            }
        }
    }
}

async fn handle_goal_request(
    context: ServiceRequestContext,
    feedback_publisher: TopicPublisher,
    node_stack: Arc<NodeStack>,
    state: Arc<Mutex<LaunchActionState>>,
    messenger: MessengerHandle,
    bound_daemon_node: String,
    daemon_instance_id: String,
) -> PeppyResult<Payload> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    // Check if already running and mark as running if not
    {
        let mut state_guard = state.lock().await;
        if matches!(*state_guard, LaunchActionState::Running) {
            let response = LaunchGoalResponse::rejected("action already in progress");
            return response
                .encode()
                .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
                    identifier: "launch_goal".to_string(),
                    reason: format!("Failed to encode response: {}", e),
                });
        }
        *state_guard = LaunchActionState::Running;
    }

    let goal = match LaunchGoal::decode(payload.as_ref()) {
        Ok(g) => g,
        Err(e) => {
            let mut state_guard = state.lock().await;
            *state_guard = LaunchActionState::Rejected;
            let response = LaunchGoalResponse::rejected(format!("invalid payload: {}", e));
            return response
                .encode()
                .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
                    identifier: "launch_goal".to_string(),
                    reason: format!("Failed to encode response: {}", e),
                });
        }
    };

    debug!("Received `stack_launch` goal from {sender_instance_id}");

    // Create log file with timestamp-based filename
    let log_dir = logs_dir_launch();
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        let error_msg = format!("Failed to create logs directory: {}", e);
        debug!("Failed to create logs directory {:?}: {}", log_dir, e);
        let mut state_guard = state.lock().await;
        *state_guard = LaunchActionState::Rejected;
        let response = LaunchGoalResponse::rejected(&error_msg);
        return response
            .encode()
            .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
                identifier: "launch_goal".to_string(),
                reason: format!("Failed to encode response: {}", e),
            });
    }

    let timestamp = Local::now().format("%Y%m%d_%H%M%S_%3f");
    let log_filename = format!("launch_{}.log", timestamp);
    let log_path = log_dir.join(&log_filename);
    let log_file = match File::create(&log_path) {
        Ok(file) => Arc::new(StdMutex::new(file)),
        Err(e) => {
            let error_msg = format!("Failed to create log file: {}", e);
            debug!("Failed to create log file {:?}: {}", log_path, e);
            let mut state_guard = state.lock().await;
            *state_guard = LaunchActionState::Rejected;
            let response = LaunchGoalResponse::rejected(&error_msg);
            return response
                .encode()
                .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
                    identifier: "launch_goal".to_string(),
                    reason: format!("Failed to encode response: {}", e),
                });
        }
    };

    debug!("Created log file for stack launch: {}", log_path.display());

    // Process the launch operation in a separate task to not block goal response
    let state_clone = Arc::clone(&state);
    let log_path_clone = log_path.clone();
    tokio::spawn(async move {
        let env_vars = goal.env_vars.clone();
        let node_add_timeout_secs = goal.node_add_timeout_secs;
        let node_start_timeout_secs = goal.node_start_timeout_secs;
        let ctx = ProcessLaunchContext {
            messenger,
            bound_daemon_node,
            daemon_instance_id,
            node_stack,
            feedback_publisher,
            log_file,
            log_path: log_path_clone.clone(),
            env_vars,
            node_add_timeout_secs,
            node_start_timeout_secs,
        };
        let result = process_launch(goal, ctx).await;
        let mut state_guard = state_clone.lock().await;
        *state_guard = LaunchActionState::Completed { result };
    });

    let response = LaunchGoalResponse::accepted(&log_path);
    response
        .encode()
        .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
            identifier: "launch_goal".to_string(),
            reason: format!("Failed to encode response: {}", e),
        })
}

/// Process a stack launch request.
///
/// This function orchestrates the complete launch sequence:
/// 1. Parse launcher configuration
/// 2. Resolve deployments
/// 3. Validate dependencies and compute order
/// 4. Snapshot and clear stack
/// 5. Add nodes in dependency order
/// 6. Start instances in dependency order
async fn process_launch(goal: LaunchGoal, ctx: ProcessLaunchContext) -> LaunchResult {
    // Step 1: Parse launcher configuration
    let (deployments, nodes_directory) = match parse_launcher_config(&ctx, &goal).await {
        Ok(result) => result,
        Err(launch_result) => return launch_result,
    };

    // Step 2: Resolve deployments
    let planned = match resolve_deployments(&ctx, deployments, &nodes_directory).await {
        Ok(result) => result,
        Err(launch_result) => return launch_result,
    };

    // Step 3: Validate dependencies and compute topological order
    let root_config = ctx.node_stack.root().config().clone();
    let ordered = match validate_and_order_dependencies(&ctx, &planned, &root_config).await {
        Ok(result) => result,
        Err(launch_result) => return launch_result,
    };

    // Step 4: Snapshot and clear stack (the snapshot helps in case an `add_cmd` or `start_cmd` fails on one of the nodes)
    let backup_stack = match snapshot_and_clear_stack(&ctx).await {
        Ok(result) => result,
        Err(launch_result) => return launch_result,
    };

    // Build lookup map
    let planned_by_key: HashMap<NodeKey, PlannedDeployment> = planned
        .into_iter()
        .map(|item| (NodeKey::new(&item.node_name, &item.node_tag), item))
        .collect();

    // Step 5: Add nodes in dependency order
    if let Err(launch_result) =
        add_nodes_to_stack(&ctx, &ordered, &planned_by_key, &backup_stack).await
    {
        return launch_result;
    }

    // Step 6: Start instances in dependency order
    if let Err(launch_result) =
        start_node_instances(&ctx, &ordered, &planned_by_key, &backup_stack).await
    {
        return launch_result;
    }

    publish_stdout(&ctx, "Launch complete", LaunchFeedbackStep::LauncherStep).await;
    LaunchResult::success(&ctx.log_path)
}

async fn handle_cancel_request(
    _context: ServiceRequestContext,
    state: Arc<Mutex<LaunchActionState>>,
) -> PeppyResult<Payload> {
    let state_guard = state.lock().await;
    if matches!(*state_guard, LaunchActionState::Running) {
        Ok(Payload::from_static(
            b"cancel acknowledged (operation cannot be interrupted)",
        ))
    } else {
        Ok(Payload::from_static(
            b"cancel acknowledged (no operation in progress)",
        ))
    }
}

async fn handle_result_request(
    _context: ServiceRequestContext,
    state: Arc<Mutex<LaunchActionState>>,
) -> PeppyResult<Payload> {
    let mut state_guard = state.lock().await;

    match std::mem::replace(&mut *state_guard, LaunchActionState::Idle) {
        LaunchActionState::Running => {
            *state_guard = LaunchActionState::Running;
            Ok(Payload::from_static(
                b"result pending: operation still in progress",
            ))
        }
        LaunchActionState::Completed { result } => {
            let payload =
                result
                    .encode()
                    .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
                        identifier: "launch_result".to_string(),
                        reason: format!("Failed to encode result: {}", e),
                    })?;
            *state_guard = LaunchActionState::ResultSent { result };
            Ok(payload)
        }
        LaunchActionState::ResultSent { result } => {
            let payload =
                result
                    .encode()
                    .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
                        identifier: "launch_result".to_string(),
                        reason: format!("Failed to encode result: {}", e),
                    })?;
            *state_guard = LaunchActionState::ResultSent { result };
            Ok(payload)
        }
        LaunchActionState::Idle | LaunchActionState::Rejected => {
            Ok(Payload::from_static(b"result pending: no result available"))
        }
    }
}
