use crate::Result;
use crate::encoding::{
    LaunchFeedback, LaunchFeedbackStep, LaunchGoal, LaunchGoalResponse, LaunchResult, NodeAddGoal,
    NodeAddLogEntry, NodeAddResult, NodeSource, NodeStartGoal, NodeStartLogEntry, NodeStartResult,
};
use crate::names;
use crate::services::action_loop::{ActionResult, ActionState, GoalHandler, run_action_loop};
use crate::services::node::{
    FeedbackLine, FeedbackStream, NodeAddActionContext, NodeBuildActionContext,
    NodeStartActionContext, create_action_log_file, log_label_from_source, resolve_node_config,
    run_node_add, run_node_build_for_entity, run_node_start, write_error_to_log,
};
use chrono::Local;
use config::consts::{DEFAULT_MESSAGING_HOST, DEFAULT_MESSAGING_PORT, PeppyDirs};
use config::launcher::{Deployment, DeploymentSource, PeppyLauncherParser, VariantSource};
use config::runtime::RuntimeConfig;
use node_stack::NodeStack;
use parking_lot::Mutex as StdMutex;
use peppylib::messaging::{ServiceRequestContext, TopicPublisher};
use peppylib::types::Payload;
use peppylib::{ActionMessenger, MessengerHandle, PeppyResult};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tracing::debug;

#[derive(Clone, Copy)]
pub struct StackLaunchTimeouts {
    pub node_startup: Duration,
    pub node_start_health: Duration,
}

pub async fn listen_for_stack_launch(
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    peppy_dirs: PeppyDirs,
    timeouts: StackLaunchTimeouts,
) -> Result<JoinHandle<Result<()>>> {
    let action = ActionMessenger::expose(
        messenger,
        core_node_name,
        instance_id,
        node_name,
        names::STACK_LAUNCH_ACTION,
    )
    .await?;

    let handler = LaunchGoalHandler {
        context: LaunchActionContext {
            node_stack,
            messenger: messenger.clone(),
            bound_core_node: core_node_name.to_string(),
            core_instance_id: instance_id.to_string(),
            peppy_dirs,
            timeouts,
        },
    };

    let handle = tokio::spawn(async move { run_action_loop(action, handler).await });

    Ok(handle)
}

impl ActionResult for LaunchResult {
    fn identifier() -> &'static str {
        "launch_result"
    }

    fn encode_result(&self) -> crate::Result<Payload> {
        self.encode()
    }
}

#[derive(Clone)]
struct LaunchGoalHandler {
    context: LaunchActionContext,
}

impl GoalHandler for LaunchGoalHandler {
    type Result = LaunchResult;

    async fn handle_goal(
        &self,
        context: ServiceRequestContext,
        feedback_publisher: TopicPublisher,
        state: Arc<Mutex<ActionState<LaunchResult>>>,
    ) -> PeppyResult<Payload> {
        handle_goal_request(context, feedback_publisher, state, self.context.clone()).await
    }
}

struct ProcessLaunchContext {
    messenger: MessengerHandle,
    bound_core_node: String,
    core_instance_id: String,
    node_stack: Arc<NodeStack>,
    peppy_dirs: PeppyDirs,
    feedback_publisher: TopicPublisher,
    log_file: Arc<StdMutex<File>>,
    log_path: PathBuf,
    env_vars: Vec<(String, String)>,
    timeouts: StackLaunchTimeouts,
    max_timeout_secs: u64,
}

#[derive(Clone)]
struct LaunchActionContext {
    node_stack: Arc<NodeStack>,
    messenger: MessengerHandle,
    bound_core_node: String,
    core_instance_id: String,
    peppy_dirs: PeppyDirs,
    timeouts: StackLaunchTimeouts,
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
    variant: Option<NodeSource>,
    node_name: String,
    node_tag: String,
    config: config::node::NodeConfig,
}

fn deployment_label(deployment: &Deployment) -> String {
    let base = match &deployment.source {
        DeploymentSource::Local(spec) => format!("local:{}", spec.local.display()),
        DeploymentSource::Git(spec) => format!("git:{}@{}:{}", spec.repo, spec.ref_, spec.path),
        DeploymentSource::Url(spec) => format!("url:{}", spec.url),
    };
    match deployment.source.variant() {
        Some(VariantSource::Name(v)) => format!("{base} [variant:{name}]", name = v.name),
        Some(VariantSource::Git(v)) => format!("{base} [variant:git:{}]", v.repo),
        Some(VariantSource::Url(v)) => format!("{base} [variant:url:{}]", v.url),
        None => base,
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
) -> std::result::Result<(NodeSource, Option<NodeSource>), String> {
    let source = match &deployment.source {
        DeploymentSource::Local(spec) => {
            let resolved = if spec.local.is_absolute() {
                spec.local.clone()
            } else {
                nodes_directory.join(&spec.local)
            };
            NodeSource::Fs(resolved)
        }
        DeploymentSource::Git(spec) => {
            let repo_url = git_url_from_repo(&spec.repo)?;
            NodeSource::Git {
                repo_url,
                repo_path: spec.path.clone(),
                repo_ref: Some(spec.ref_.clone()),
            }
        }
        DeploymentSource::Url(spec) => {
            let url = url::Url::parse(&spec.url)
                .map_err(|e| format!("invalid HTTP URL `{}`: {e}", spec.url))?;
            NodeSource::Http {
                url,
                sha256: Some(spec.sha256.clone()),
            }
        }
    };

    let variant = deployment
        .source
        .variant()
        .map(variant_source_to_node_source)
        .transpose()?;

    Ok((source, variant))
}

fn variant_source_to_node_source(
    variant: &VariantSource,
) -> std::result::Result<NodeSource, String> {
    match variant {
        VariantSource::Name(v) => Ok(NodeSource::Fs(std::path::PathBuf::from(&v.name))),
        VariantSource::Git(v) => {
            let repo_url = git_url_from_repo(&v.repo)?;
            Ok(NodeSource::Git {
                repo_url,
                repo_path: v.path.clone().unwrap_or_default(),
                repo_ref: v.ref_.clone(),
            })
        }
        VariantSource::Url(v) => {
            let url = url::Url::parse(&v.url)
                .map_err(|e| format!("invalid variant HTTP URL `{}`: {e}", v.url))?;
            Ok(NodeSource::Http {
                url,
                sha256: v.sha256.clone(),
            })
        }
    }
}

/// Marker git_hash used for stack-launch operations.
/// When this marker is used, the node_add service skips git hash verification
/// and generates fresh peppygen files. This allows stack_launch to work with
/// local filesystem sources without requiring `peppy node sync` beforehand.
pub const STACK_LAUNCH_GIT_HASH: &str = "stack-launch";

async fn publish_feedback(ctx: &ProcessLaunchContext, feedback: LaunchFeedback) {
    {
        let mut file = ctx.log_file.lock();
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

/// Spawns a feedback forwarding task that reads `FeedbackLine` values from the
/// channel and publishes them as `LaunchFeedback` to the launch feedback topic.
///
/// Returns the sender end (to pass into the process context) and a join handle
/// for the consumer task. Drop the sender to signal completion, then await the
/// handle to drain remaining messages.
fn spawn_feedback_forwarder(
    feedback_publisher: &TopicPublisher,
    step: LaunchFeedbackStep,
    log_file: &Arc<StdMutex<File>>,
) -> (mpsc::UnboundedSender<FeedbackLine>, JoinHandle<()>) {
    let (feedback_tx, mut feedback_rx) = mpsc::unbounded_channel::<FeedbackLine>();
    let publisher = feedback_publisher.clone();
    let log_file = Arc::clone(log_file);
    let handle = tokio::spawn(async move {
        while let Some(line) = feedback_rx.recv().await {
            node_stack::build_io::write_feedback_log_line(&log_file, line.stream, &line.line);

            let launch_feedback = match line.stream {
                FeedbackStream::Stdout => LaunchFeedback::stdout(&line.line, step.clone()),
                FeedbackStream::Stderr => LaunchFeedback::stderr(&line.line, step.clone()),
                // Warnings bypass the per-node scrolling step and surface as
                // persistent LauncherStep stderr lines so the operator sees
                // them even after the step buffer scrolls past.
                FeedbackStream::Warning => {
                    LaunchFeedback::stderr(&line.line, LaunchFeedbackStep::LauncherStep)
                }
            };
            if let Ok(payload) = launch_feedback.encode() {
                let _ = publisher.publish(payload).await;
            }
        }
    });
    (feedback_tx, handle)
}

async fn add_node_directly(
    ctx: &ProcessLaunchContext,
    node_add_goal: NodeAddGoal,
) -> (std::result::Result<NodeAddResult, String>, Option<PathBuf>) {
    // Create log file before source resolution so clone/download output is captured.
    let log_label = log_label_from_source(&node_add_goal.source);
    let log_dir = ctx.peppy_dirs.logs_dir_add();
    let timestamp = Local::now().format("%Y%m%d_%H%M%S_%3f").to_string();
    let log_filename = format!("{}_{}.log", log_label, timestamp);
    let (log_file, log_path) = match create_action_log_file(&log_dir, &log_filename) {
        Ok(r) => r,
        Err(e) => return (Err(e), None),
    };

    let (feedback_tx, forwarder_handle) = spawn_feedback_forwarder(
        &ctx.feedback_publisher,
        LaunchFeedbackStep::AddingNode,
        &ctx.log_file,
    );

    let action_context = NodeAddActionContext {
        node_stack: Arc::clone(&ctx.node_stack),
        messenger: ctx.messenger.clone(),
        bound_core_node: ctx.bound_core_node.clone(),
        core_instance_id: ctx.core_instance_id.clone(),
        peppy_dirs: ctx.peppy_dirs.clone(),
    };

    let log_file_for_timeout = log_file.clone();
    let log_path_for_timeout = log_path.clone();
    let max_timeout = Duration::from_secs(ctx.max_timeout_secs);

    let result = match tokio::time::timeout(
        max_timeout,
        run_node_add(
            node_add_goal,
            action_context,
            feedback_tx,
            log_file,
            log_path,
            timestamp,
        ),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            write_error_to_log(&log_file_for_timeout, "max timeout exceeded");
            NodeAddResult::failure(&log_path_for_timeout, "timeout: max timeout exceeded")
        }
    };

    // Wait for feedback forwarder to drain.
    let _ = forwarder_handle.await;

    let final_log_path = Some(result.log_path.clone());
    if result.success {
        (Ok(result), final_log_path)
    } else {
        let err = result
            .error_message
            .clone()
            .unwrap_or_else(|| "node_add failed".to_string());
        (Err(err), final_log_path)
    }
}

async fn build_node_directly(
    ctx: &ProcessLaunchContext,
    node_name: String,
    node_tag: String,
    env_vars: Vec<(String, String)>,
) -> std::result::Result<(), String> {
    let log_dir = ctx.peppy_dirs.logs_dir_build();
    let timestamp = Local::now().format("%Y%m%d_%H%M%S_%3f").to_string();
    let log_filename = format!("{}_{}_{}.log", node_name, node_tag, timestamp);
    let (log_file, log_path) =
        create_action_log_file(&log_dir, &log_filename).map_err(|e| e.to_string())?;

    let (feedback_tx, forwarder_handle) = spawn_feedback_forwarder(
        &ctx.feedback_publisher,
        LaunchFeedbackStep::AddingNode,
        &ctx.log_file,
    );

    let action_context = NodeBuildActionContext {
        node_stack: Arc::clone(&ctx.node_stack),
        peppy_dirs: ctx.peppy_dirs.clone(),
    };

    let max_timeout = Duration::from_secs(ctx.max_timeout_secs);
    let log_file_for_timeout = log_file.clone();
    let log_path_for_timeout = log_path.clone();

    let result = match tokio::time::timeout(
        max_timeout,
        run_node_build_for_entity(
            node_name.clone(),
            node_tag.clone(),
            env_vars,
            action_context,
            feedback_tx,
            log_file,
            log_path,
        ),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            write_error_to_log(&log_file_for_timeout, "max timeout exceeded");
            crate::encoding::NodeBuildResult::failure(
                &log_path_for_timeout,
                "timeout: max timeout exceeded",
            )
        }
    };

    let _ = forwarder_handle.await;

    if result.success {
        Ok(())
    } else {
        Err(result
            .error_message
            .unwrap_or_else(|| "node_build failed".to_string()))
    }
}

async fn start_node_directly(
    ctx: &ProcessLaunchContext,
    node_start_goal: NodeStartGoal,
    runtime_config: RuntimeConfig,
    log_path: PathBuf,
    log_file: Arc<StdMutex<File>>,
) -> (
    std::result::Result<NodeStartResult, String>,
    Option<PathBuf>,
) {
    let (feedback_tx, _forwarder_handle) = spawn_feedback_forwarder(
        &ctx.feedback_publisher,
        LaunchFeedbackStep::StartingNode,
        &ctx.log_file,
    );

    let action_context = NodeStartActionContext {
        node_stack: Arc::clone(&ctx.node_stack),
        messenger: ctx.messenger.clone(),
        core_node_name: ctx.bound_core_node.clone(),
        caller_instance_id: ctx.core_instance_id.clone(),
        node_startup_timeout: ctx.timeouts.node_startup,
        node_start_health_timeout: ctx.timeouts.node_start_health,
        peppy_dirs: ctx.peppy_dirs.clone(),
    };

    let log_file_for_timeout = log_file.clone();
    let max_timeout = Duration::from_secs(ctx.max_timeout_secs);

    let result = match tokio::time::timeout(
        max_timeout,
        run_node_start(
            node_start_goal,
            runtime_config,
            action_context,
            feedback_tx,
            log_file,
            ctx.core_instance_id.clone(),
        ),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            write_error_to_log(&log_file_for_timeout, "max timeout exceeded");
            NodeStartResult::failure("timeout: max timeout exceeded")
        }
    };

    // Don't await _forwarder_handle — the node process is still running and
    // output readers keep the internal channel alive.

    let node_log_path = Some(log_path);
    if result.success {
        (Ok(result), node_log_path)
    } else {
        let err = result
            .error_message
            .clone()
            .unwrap_or_else(|| "node_start failed".to_string());
        (Err(err), node_log_path)
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

        let (source, variant) =
            match node_source_from_deployment_source(&deployment, nodes_directory) {
                Ok(result) => result,
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

        let config = match resolve_node_config(source.clone(), &ctx.peppy_dirs).await {
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
            variant,
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
                &item.config.manifest,
                &item.config.interfaces,
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
        let root_handle = ctx.node_stack.root();
        let (root_cfg, root_path) = {
            let guard = root_handle.read();
            (
                guard.config().clone(),
                guard
                    .artifact_path()
                    .unwrap_or_else(|| guard.config_path())
                    .to_path_buf(),
            )
        };
        let backup = NodeStack::new(root_cfg, None, root_path);
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
    add_log_paths: &mut Vec<NodeAddLogEntry>,
) -> std::result::Result<(), LaunchResult> {
    publish_stdout(
        ctx,
        "Adding nodes to the stack...",
        LaunchFeedbackStep::LauncherStep,
    )
    .await;

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

        // Pass max_timeout_secs to the goal for daemon-side busy reporting
        let goal_timeout_secs = ctx.max_timeout_secs;
        let node_add_goal = match &item.source {
            NodeSource::Fs(path) => {
                NodeAddGoal::new(path.clone(), STACK_LAUNCH_GIT_HASH, goal_timeout_secs)
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
                goal_timeout_secs,
            ),
            NodeSource::Http { url, sha256 } => NodeAddGoal::new_http(
                url.clone(),
                sha256.clone(),
                STACK_LAUNCH_GIT_HASH,
                goal_timeout_secs,
            ),
        }
        .with_env_vars(ctx.env_vars.clone());

        let node_add_goal = match item.variant {
            Some(ref variant) => node_add_goal.with_variant_source(variant.clone()),
            None => node_add_goal,
        };

        let (result, log_path) = add_node_directly(ctx, node_add_goal).await;

        let failed = result.as_ref().map(|r| !r.success).unwrap_or(true);
        if let Some(path) = log_path {
            add_log_paths.push(NodeAddLogEntry {
                node_label: key.label(),
                log_path: path,
                failed,
            });
        }

        match result {
            Ok(result) => {
                if !result.success {
                    let inner = result
                        .error_message
                        .unwrap_or_else(|| "node_add failed".to_string());
                    let reason = format!("failed to add node {}: {}", key.label(), inner);
                    return Err(restore_stack(ctx, backup_stack, reason).await);
                }
                let node_name = result.node_name.clone().unwrap_or_else(|| key.name.clone());
                let node_tag = result.node_tag.clone().unwrap_or_else(|| key.tag.clone());

                // Stack launch chains directly from add into build, since the
                // launcher's contract is "the stack is up and running" — an
                // `Added` entity isn't actually buildable from the user's
                // perspective until `node build` has run.
                if let Err(err) =
                    build_node_directly(ctx, node_name, node_tag, ctx.env_vars.clone()).await
                {
                    let reason = format!("failed to build node {}: {}", key.label(), err);
                    return Err(restore_stack(ctx, backup_stack, reason).await);
                }
            }
            Err(err) => {
                let reason = format!("failed to add node {}: {}", key.label(), err);
                return Err(restore_stack(ctx, backup_stack, reason).await);
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
    start_log_paths: &mut Vec<NodeStartLogEntry>,
) -> std::result::Result<(), LaunchResult> {
    publish_stdout(ctx, "Starting nodes...", LaunchFeedbackStep::LauncherStep).await;

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

            let node_instance = config::runtime::NodeInstanceConfig {
                instance_id: instance.instance_id.clone(),
                arguments: instance.arguments.clone(),
            };
            let runtime_config = match RuntimeConfig::new(
                messaging_host.as_str(),
                messaging_port,
                node_instance,
                item.node_name.as_str(),
                ctx.bound_core_node.as_str(),
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

            // Pass max_timeout_secs to the goal for daemon-side busy reporting
            let node_start_goal = NodeStartGoal::new(
                &runtime_config_json5,
                item.node_name.as_str(),
                item.node_tag.as_str(),
                ctx.max_timeout_secs,
            )
            .with_env_vars(ctx.env_vars.clone());

            // Create log file for this node start
            let log_dir = ctx.peppy_dirs.logs_dir_start();
            let log_filename = format!("{}.log", instance_id);
            let (log_file, log_path) = match create_action_log_file(&log_dir, &log_filename) {
                Ok(r) => r,
                Err(e) => {
                    return Err(restore_stack(ctx, backup_stack, e).await);
                }
            };

            let (result, log_path) =
                start_node_directly(ctx, node_start_goal, runtime_config, log_path, log_file).await;

            let failed = result.as_ref().map(|r| !r.success).unwrap_or(true);
            if let Some(path) = log_path {
                start_log_paths.push(NodeStartLogEntry {
                    instance_id: instance_id.to_string(),
                    node_label: key.label(),
                    log_path: path,
                    failed,
                });
            }

            match result {
                Ok(result) => {
                    if !result.success {
                        let inner = result
                            .error_message
                            .unwrap_or_else(|| "node_start failed".to_string());
                        let reason = format!(
                            "failed to start node {} instance {}: {}",
                            key.label(),
                            instance_id,
                            inner
                        );
                        return Err(restore_stack(ctx, backup_stack, reason).await);
                    }
                }
                Err(err) => {
                    let reason = format!(
                        "failed to start node {} instance {}: {}",
                        key.label(),
                        instance_id,
                        err
                    );
                    return Err(restore_stack(ctx, backup_stack, reason).await);
                }
            }
        }
    }

    Ok(())
}

async fn handle_goal_request(
    context: ServiceRequestContext,
    feedback_publisher: TopicPublisher,
    state: Arc<Mutex<ActionState<LaunchResult>>>,
    action_context: LaunchActionContext,
) -> PeppyResult<Payload> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    // Check if already running and mark as running if not
    {
        let mut state_guard = state.lock().await;
        if matches!(*state_guard, ActionState::Running) {
            let response = LaunchGoalResponse::rejected("action already in progress");
            return response
                .encode()
                .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
                    identifier: "launch_goal".to_string(),
                    reason: format!("Failed to encode response: {}", e),
                });
        }
        *state_guard = ActionState::Running;
    }

    let goal = match LaunchGoal::decode(payload.as_ref()) {
        Ok(g) => g,
        Err(e) => {
            let mut state_guard = state.lock().await;
            *state_guard = ActionState::Rejected;
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
    let log_dir = action_context.peppy_dirs.logs_dir_launch();
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        let error_msg = format!("Failed to create logs directory: {}", e);
        debug!("Failed to create logs directory {:?}: {}", log_dir, e);
        let mut state_guard = state.lock().await;
        *state_guard = ActionState::Rejected;
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
            *state_guard = ActionState::Rejected;
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
        let LaunchActionContext {
            messenger,
            bound_core_node,
            core_instance_id,
            node_stack,
            peppy_dirs,
            timeouts,
        } = action_context;
        let env_vars = goal.env_vars.clone();
        let max_timeout_secs = goal.max_timeout_secs;
        let ctx = ProcessLaunchContext {
            messenger,
            bound_core_node,
            core_instance_id,
            node_stack,
            peppy_dirs,
            feedback_publisher,
            log_file,
            log_path: log_path_clone.clone(),
            env_vars,
            timeouts,
            max_timeout_secs,
        };
        let result = process_launch(goal, ctx).await;
        let mut state_guard = state_clone.lock().await;
        *state_guard = ActionState::Completed { result };
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
    let root_config = ctx.node_stack.root().read().config().clone();
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

    let mut add_log_paths: Vec<NodeAddLogEntry> = Vec::new();
    let mut start_log_paths: Vec<NodeStartLogEntry> = Vec::new();

    // Step 5: Add nodes in dependency order
    let add_result = add_nodes_to_stack(
        &ctx,
        &ordered,
        &planned_by_key,
        &backup_stack,
        &mut add_log_paths,
    )
    .await;

    // Step 6: Start instances in dependency order (only if add succeeded)
    let start_result = if add_result.is_ok() {
        Some(
            start_node_instances(
                &ctx,
                &ordered,
                &planned_by_key,
                &backup_stack,
                &mut start_log_paths,
            )
            .await,
        )
    } else {
        None
    };

    if let Err(mut launch_result) = add_result {
        launch_result.node_add_logs = add_log_paths;
        return launch_result;
    }
    if let Some(Err(mut launch_result)) = start_result {
        launch_result.node_add_logs = add_log_paths;
        launch_result.node_start_logs = start_log_paths;
        return launch_result;
    }

    publish_stdout(&ctx, "Launch complete", LaunchFeedbackStep::LauncherStep).await;
    LaunchResult::success(&ctx.log_path).with_node_logs(add_log_paths, start_log_paths)
}
