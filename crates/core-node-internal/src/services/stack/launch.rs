use crate::Result;
use crate::names;
use crate::services::action_loop::{ActionResult, ActionState, GoalHandler, run_action_loop};
use crate::services::node::{
    FeedbackLine, FeedbackStream, NodeAddActionContext, NodeBuildActionContext,
    NodeRunActionContext, create_action_log_file, log_label_from_source, resolve_node_config,
    run_node_add, run_node_build_for_entity, run_node_run, write_error_to_log,
};
use chrono::Local;
use config::consts::{DEFAULT_MESSAGING_HOST, DEFAULT_MESSAGING_PORT, PeppyDirs};
use config::launcher::{Deployment, DeploymentSource, PeppyLauncherParser, VariantSource};
use config::runtime::RuntimeConfig;
use core_node_api::encoding::{
    LaunchFeedback, LaunchFeedbackStep, LaunchGoal, LaunchGoalResponse, LaunchResult,
    LauncherOrigin, NodeAddGoal, NodeAddLogEntry, NodeAddResult, NodeBuildLogEntry, NodeRunGoal,
    NodeRunLogEntry, NodeRunResult, NodeSource,
};
use node_stack::{NameTagKey, NodeStack};
use parking_lot::Mutex as StdMutex;
use peppylib::messaging::{ActionFeedbackPublisher, ServiceRequestContext};
use peppylib::types::Payload;
use peppylib::{ActionMessenger, MessengerHandle, PeppyResult};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Notify, mpsc};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::debug;

/// Upper bound on how long a run-phase future may spend in its cancellation
/// cleanup (SIGKILL the child + unregister the `Starting` instance + clear
/// temp files). Keeps a misbehaving cleanup from stalling the launch failure.
const RUN_PHASE_CANCEL_CLEANUP_BUDGET: Duration = Duration::from_secs(30);

/// Watches for an idle period: returns when no `notify_one()` arrives for `idle_timeout`.
/// Each call to `notify_one()` on `notify` resets the clock.
async fn watch_idle(notify: Arc<Notify>, idle_timeout: Duration) {
    loop {
        match tokio::time::timeout(idle_timeout, notify.notified()).await {
            Ok(()) => continue,
            Err(_) => return,
        }
    }
}

/// Outcome of a per-phase operation wrapped with idle + (optional) launch-deadline enforcement.
enum PhaseOutcome<T> {
    Completed(T),
    IdleTimeout,
    MaxTimeout,
}

/// Per-phase idle timeouts, sourced from the `LaunchGoal` payload. Each phase's clock resets
/// only on genuine subprocess/git/http activity (see `spawn_feedback_forwarder`).
#[derive(Clone, Copy)]
struct IdleTimeouts {
    add: Duration,
    build: Duration,
    run: Duration,
}

#[derive(Clone, Copy)]
pub struct StackLaunchTimeouts {
    pub node_startup: Duration,
    pub node_start_health: Duration,
    pub health_monitor_interval: Duration,
    pub health_monitor_timeout: Duration,
    pub health_monitor_max_failures: u32,
}

/// Daemon-wide defaults the stack launcher applies to every spawned
/// instance. Pairs with launcher overrides (`FrameworkOverrides`) and the
/// per-instance resolved values (`ResolvedFramework`); this struct is the
/// "what the daemon would pick when the instance omits the override" half.
pub struct StackLaunchDefaults {
    pub timeouts: StackLaunchTimeouts,
    pub use_sim_time: bool,
}

pub async fn listen_for_stack_launch(
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    peppy_dirs: PeppyDirs,
    defaults: StackLaunchDefaults,
) -> Result<JoinHandle<Result<()>>> {
    let action = ActionMessenger::expose(
        messenger,
        core_node_name,
        instance_id,
        config::runtime::DEFAULT_VARIANT,
        node_name,
        names::STACK_LAUNCH_ACTION,
    )
    .await?;

    let StackLaunchDefaults {
        timeouts,
        use_sim_time: daemon_use_sim_time,
    } = defaults;
    let handler = LaunchGoalHandler {
        context: LaunchActionContext {
            node_stack,
            messenger: messenger.clone(),
            bound_core_node: core_node_name.to_string(),
            core_instance_id: instance_id.to_string(),
            peppy_dirs,
            timeouts,
            daemon_use_sim_time,
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
        self.encode().map_err(Into::into)
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
        user_payload: bytes::Bytes,
        feedback_publisher: ActionFeedbackPublisher,
        state: Arc<Mutex<ActionState<LaunchResult>>>,
    ) -> PeppyResult<Payload> {
        handle_goal_request(
            context,
            user_payload,
            feedback_publisher,
            state,
            self.context.clone(),
        )
        .await
    }
}

struct ProcessLaunchContext {
    messenger: MessengerHandle,
    bound_core_node: String,
    core_instance_id: String,
    node_stack: Arc<NodeStack>,
    peppy_dirs: PeppyDirs,
    feedback_publisher: ActionFeedbackPublisher,
    log_file: Arc<StdMutex<File>>,
    log_path: PathBuf,
    env_vars: Vec<(String, String)>,
    timeouts: StackLaunchTimeouts,
    /// Whole-launch deadline. `None` means the user did not opt into a max — only idle timeouts
    /// are enforced.
    launch_deadline: Option<Instant>,
    idle_timeouts: IdleTimeouts,
    /// Daemon-wide default for `framework.use_sim_time` applied to instances
    /// that omit the per-instance override.
    daemon_use_sim_time: bool,
}

#[derive(Clone)]
struct LaunchActionContext {
    node_stack: Arc<NodeStack>,
    messenger: MessengerHandle,
    bound_core_node: String,
    core_instance_id: String,
    peppy_dirs: PeppyDirs,
    timeouts: StackLaunchTimeouts,
    daemon_use_sim_time: bool,
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
        DeploymentSource::Repo(spec) => format!("repo:{}:{}", spec.name, spec.tag),
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
    peppy_dirs: &PeppyDirs,
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
        DeploymentSource::Repo(spec) => crate::services::repo::cache::resolve_repo_node_source(
            &spec.name, &spec.tag, None, peppy_dirs,
        )?,
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

/// Collapse a per-instance launcher override and the daemon-wide default
/// into a single resolved framework block. Centralizes the "per-instance
/// value > daemon default > wall" precedence so the spawned node receives
/// one concrete value and never has to re-implement the fallback.
fn resolve_framework(
    overrides: &config::launcher::FrameworkOverrides,
    daemon_default_use_sim_time: bool,
) -> config::runtime::ResolvedFramework {
    config::runtime::ResolvedFramework {
        use_sim_time: overrides
            .use_sim_time
            .unwrap_or(daemon_default_use_sim_time),
    }
}

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
/// Each line received also pings `activity_notify` (if provided), which the per-phase idle
/// watcher uses to reset its idle clock. The notify is the single seam where real subprocess /
/// git2 / http-downloader output (which all flow through this mpsc) gets observed; launcher
/// orchestration messages (`publish_stdout` / `publish_stderr`) bypass this channel and so do
/// NOT reset the idle clock — which is the right behavior, since they're operator narration,
/// not subprocess liveness.
///
/// Returns the sender end (to pass into the process context) and a join handle
/// for the consumer task. Drop the sender to signal completion, then await the
/// handle to drain remaining messages.
fn spawn_feedback_forwarder(
    feedback_publisher: &ActionFeedbackPublisher,
    step: LaunchFeedbackStep,
    log_file: &Arc<StdMutex<File>>,
    activity_notify: Option<Arc<Notify>>,
) -> (mpsc::UnboundedSender<FeedbackLine>, JoinHandle<()>) {
    let (feedback_tx, mut feedback_rx) = mpsc::unbounded_channel::<FeedbackLine>();
    let publisher = feedback_publisher.clone();
    let log_file = Arc::clone(log_file);
    let handle = tokio::spawn(async move {
        while let Some(line) = feedback_rx.recv().await {
            if let Some(notify) = &activity_notify {
                notify.notify_one();
            }

            node_stack::build_io::write_feedback_log_line(&log_file, line.stream, &line.line);

            let launch_feedback = match line.stream {
                FeedbackStream::Stdout => LaunchFeedback::stdout(&line.line, step),
                FeedbackStream::Stderr => LaunchFeedback::stderr(&line.line, step),
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

/// Wraps a phase future with idle-timeout enforcement and an optional whole-launch deadline.
///
/// The idle watcher always runs (idle protection is always on); the deadline only wraps when
/// `launch_deadline` is `Some`. Returns:
/// - `Completed(T)` if the phase finished within both bounds
/// - `IdleTimeout` if `idle_timeout` elapsed without subprocess activity
/// - `MaxTimeout` if the launch deadline fired
///
/// Cancellation semantics differ per phase:
/// - **add** relies on git2's progress callback returning the cancellation status when the
///   future is dropped, and on the http downloader's drop-safe streaming reader.
/// - **build** relies on `stream_child_output`'s `KillGuard` (in
///   `node-stack-internal/src/build_io.rs`), which SIGKILLs the child process group on drop.
/// - **run** cannot rely on drop alone: `prepare_and_spawn` returns a raw
///   `tokio::process::Child` held on the phase future's stack with no `kill_on_drop`, so
///   dropping it leaves the OS process and its `Starting` stack entry behind. Callers that
///   need run-phase cancellation pass `cancel_and_drain = Some(token)`; on timeout the
///   runner signals the token and awaits the phase future's cooperative cleanup (bounded by
///   `RUN_PHASE_CANCEL_CLEANUP_BUDGET`) instead of dropping it.
async fn run_phase_with_timeouts<F, T>(
    phase: F,
    activity_notify: Arc<Notify>,
    idle_timeout: Duration,
    launch_deadline: Option<Instant>,
    cancel_and_drain: Option<CancellationToken>,
) -> PhaseOutcome<T>
where
    F: std::future::Future<Output = T>,
{
    match cancel_and_drain {
        None => {
            run_phase_drop_on_timeout(phase, activity_notify, idle_timeout, launch_deadline).await
        }
        Some(token) => {
            run_phase_cancel_on_timeout(
                phase,
                activity_notify,
                idle_timeout,
                launch_deadline,
                token,
            )
            .await
        }
    }
}

/// Timeout behavior for phases whose futures are cancellation-safe via `Drop`
/// (currently: add, build).
async fn run_phase_drop_on_timeout<F, T>(
    phase: F,
    activity_notify: Arc<Notify>,
    idle_timeout: Duration,
    launch_deadline: Option<Instant>,
) -> PhaseOutcome<T>
where
    F: std::future::Future<Output = T>,
{
    let inner = async {
        tokio::select! {
            biased;
            _ = watch_idle(activity_notify, idle_timeout) => None,
            result = phase => Some(result),
        }
    };

    match launch_deadline {
        Some(deadline) => match tokio::time::timeout_at(deadline, inner).await {
            Ok(Some(value)) => PhaseOutcome::Completed(value),
            Ok(None) => PhaseOutcome::IdleTimeout,
            Err(_) => PhaseOutcome::MaxTimeout,
        },
        None => match inner.await {
            Some(value) => PhaseOutcome::Completed(value),
            None => PhaseOutcome::IdleTimeout,
        },
    }
}

/// Timeout behavior for phases that own resources (e.g. a spawned child
/// process) not reaped by `Drop`. On timeout, signals `cancel_token` and
/// awaits the phase future for up to `RUN_PHASE_CANCEL_CLEANUP_BUDGET` so it
/// can run its own teardown (SIGKILL the child, unregister the `Starting`
/// instance, remove temp files) before we return the timeout outcome.
async fn run_phase_cancel_on_timeout<F, T>(
    phase: F,
    activity_notify: Arc<Notify>,
    idle_timeout: Duration,
    launch_deadline: Option<Instant>,
    cancel_token: CancellationToken,
) -> PhaseOutcome<T>
where
    F: std::future::Future<Output = T>,
{
    tokio::pin!(phase);

    // `sleep_until(past-instant)` resolves immediately, so we model "no
    // deadline" as a far-future sleep and let idle/phase race win.
    let deadline_sleep = async {
        match launch_deadline {
            Some(deadline) => tokio::time::sleep_until(deadline).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(deadline_sleep);

    let timeout_kind = tokio::select! {
        biased;
        result = &mut phase => return PhaseOutcome::Completed(result),
        _ = watch_idle(activity_notify, idle_timeout) => PhaseOutcome::IdleTimeout,
        _ = &mut deadline_sleep => PhaseOutcome::MaxTimeout,
    };

    // Timeout fired. Ask the phase to tear itself down, then drive it to
    // completion so its cleanup (kill child, remove `Starting` entry, delete
    // instance dir) actually runs. If cleanup stalls past the budget we drop
    // the future as a last resort — still strictly better than today, since
    // the run phase would have been dropped immediately in that branch.
    cancel_token.cancel();
    let _ = tokio::time::timeout(RUN_PHASE_CANCEL_CLEANUP_BUDGET, phase.as_mut()).await;

    timeout_kind
}

/// Runs a phase future under idle + (optional) deadline bounds and, on timeout, writes the
/// reason to `log_file` and builds a caller-specified failure result. `build_failure`
/// receives the same string that was logged so phase-specific failure types (differing in
/// whether they carry a `log_path`) can embed it verbatim.
///
/// `cancel_and_drain` controls what happens to the phase future on timeout — see
/// [`run_phase_with_timeouts`] for the per-phase rationale.
#[allow(clippy::too_many_arguments)] // All args serve distinct, unrelated roles; grouping them adds noise.
async fn run_phase<F, T>(
    phase: F,
    activity_notify: Arc<Notify>,
    idle_timeout: Duration,
    launch_deadline: Option<Instant>,
    log_file: &Arc<StdMutex<File>>,
    step: LaunchFeedbackStep,
    build_failure: impl FnOnce(String) -> T,
    cancel_and_drain: Option<CancellationToken>,
) -> T
where
    F: std::future::Future<Output = T>,
{
    match run_phase_with_timeouts(
        phase,
        activity_notify,
        idle_timeout,
        launch_deadline,
        cancel_and_drain,
    )
    .await
    {
        PhaseOutcome::Completed(result) => result,
        PhaseOutcome::IdleTimeout => {
            let reason = format!(
                "timeout: {} idle timeout exceeded ({}s without output)",
                step.phase_label(),
                idle_timeout.as_secs()
            );
            write_error_to_log(log_file, &reason);
            build_failure(reason)
        }
        PhaseOutcome::MaxTimeout => {
            let reason = "timeout: max launch timeout exceeded".to_string();
            write_error_to_log(log_file, &reason);
            build_failure(reason)
        }
    }
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

    let activity_notify = Arc::new(Notify::new());
    let (feedback_tx, forwarder_handle) = spawn_feedback_forwarder(
        &ctx.feedback_publisher,
        LaunchFeedbackStep::AddingNode,
        &ctx.log_file,
        Some(Arc::clone(&activity_notify)),
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

    let result = run_phase(
        run_node_add(
            node_add_goal,
            action_context,
            feedback_tx,
            log_file,
            log_path,
            timestamp,
        ),
        activity_notify,
        ctx.idle_timeouts.add,
        ctx.launch_deadline,
        &log_file_for_timeout,
        LaunchFeedbackStep::AddingNode,
        |reason| NodeAddResult::failure(&log_path_for_timeout, reason),
        None,
    )
    .await;

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
    node_variant: String,
    env_vars: Vec<(String, String)>,
) -> (std::result::Result<(), String>, Option<PathBuf>) {
    let log_dir = ctx.peppy_dirs.logs_dir_build();
    let timestamp = Local::now().format("%Y%m%d_%H%M%S_%3f").to_string();
    let log_filename = format!(
        "{}_{}__{}_{}.log",
        node_name, node_tag, node_variant, timestamp
    );
    let (log_file, log_path) = match create_action_log_file(&log_dir, &log_filename) {
        Ok(pair) => pair,
        Err(e) => return (Err(e.to_string()), None),
    };

    let final_log_path = log_path.clone();

    let activity_notify = Arc::new(Notify::new());
    let (feedback_tx, forwarder_handle) = spawn_feedback_forwarder(
        &ctx.feedback_publisher,
        LaunchFeedbackStep::BuildingNode,
        &ctx.log_file,
        Some(Arc::clone(&activity_notify)),
    );

    let action_context = NodeBuildActionContext {
        node_stack: Arc::clone(&ctx.node_stack),
        peppy_dirs: ctx.peppy_dirs.clone(),
    };

    let log_file_for_timeout = log_file.clone();
    let log_path_for_timeout = log_path.clone();

    let result = run_phase(
        run_node_build_for_entity(
            node_name.clone(),
            node_tag.clone(),
            env_vars,
            action_context,
            feedback_tx,
            log_file,
            log_path,
        ),
        activity_notify,
        ctx.idle_timeouts.build,
        ctx.launch_deadline,
        &log_file_for_timeout,
        LaunchFeedbackStep::BuildingNode,
        |reason| core_node_api::encoding::NodeBuildResult::failure(&log_path_for_timeout, reason),
        None,
    )
    .await;

    let _ = forwarder_handle.await;

    if result.success {
        (Ok(()), Some(final_log_path))
    } else {
        (
            Err(result
                .error_message
                .unwrap_or_else(|| "node_build failed".to_string())),
            Some(final_log_path),
        )
    }
}

async fn start_node_directly(
    ctx: &ProcessLaunchContext,
    node_run_goal: NodeRunGoal,
    runtime_config: RuntimeConfig,
    log_path: PathBuf,
    log_file: Arc<StdMutex<File>>,
) -> (std::result::Result<NodeRunResult, String>, Option<PathBuf>) {
    let activity_notify = Arc::new(Notify::new());
    let (feedback_tx, _forwarder_handle) = spawn_feedback_forwarder(
        &ctx.feedback_publisher,
        LaunchFeedbackStep::RunningNode,
        &ctx.log_file,
        Some(Arc::clone(&activity_notify)),
    );

    let action_context = NodeRunActionContext {
        node_stack: Arc::clone(&ctx.node_stack),
        messenger: ctx.messenger.clone(),
        core_node_name: ctx.bound_core_node.clone(),
        caller_instance_id: ctx.core_instance_id.clone(),
        node_startup_timeout: ctx.timeouts.node_startup,
        node_start_health_timeout: ctx.timeouts.node_start_health,
        peppy_dirs: ctx.peppy_dirs.clone(),
        health_monitor_interval: ctx.timeouts.health_monitor_interval,
        health_monitor_timeout: ctx.timeouts.health_monitor_timeout,
        health_monitor_max_failures: ctx.timeouts.health_monitor_max_failures,
    };

    let log_file_for_timeout = log_file.clone();

    // Token triggered by `run_phase_with_timeouts` on idle/max timeout; observed
    // inside `run_node_run` to abort a half-spawned node instance (SIGKILL the
    // child + unregister its `Starting` entry) before we return the failure.
    let run_cancel_token = CancellationToken::new();

    let result = run_phase(
        run_node_run(
            node_run_goal,
            runtime_config,
            action_context,
            feedback_tx,
            log_file,
            ctx.core_instance_id.clone(),
            run_cancel_token.clone(),
        ),
        activity_notify,
        ctx.idle_timeouts.run,
        ctx.launch_deadline,
        &log_file_for_timeout,
        LaunchFeedbackStep::RunningNode,
        NodeRunResult::failure,
        Some(run_cancel_token),
    )
    .await;

    // Don't await _forwarder_handle — the node process is still running and
    // output readers keep the internal channel alive.

    let node_log_path = Some(log_path);
    if result.success {
        (Ok(result), node_log_path)
    } else {
        let err = result
            .error_message
            .clone()
            .unwrap_or_else(|| "node_run failed".to_string());
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

    let launch_file = match resolve_launcher_origin(ctx, &goal.launcher_origin).await {
        Ok(path) => path,
        Err(msg) => {
            publish_stderr(ctx, &msg, LaunchFeedbackStep::LauncherStep).await;
            return Err(LaunchResult::failure(&ctx.log_path, msg));
        }
    };

    if !launch_file.exists() {
        let msg = format!("launch file does not exist: {}", launch_file.display());
        publish_stderr(ctx, &msg, LaunchFeedbackStep::LauncherStep).await;
        return Err(LaunchResult::failure(&ctx.log_path, msg));
    }

    if !launch_file.is_file() {
        let msg = format!("launch file path must be a file: {}", launch_file.display());
        publish_stderr(ctx, &msg, LaunchFeedbackStep::LauncherStep).await;
        return Err(LaunchResult::failure(&ctx.log_path, msg));
    }

    let peppy_launcher = match PeppyLauncherParser::from_path(&launch_file) {
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
    let nodes_directory = launch_file
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    let deployments = peppy_launcher.deployments.clone();
    Ok((deployments, nodes_directory))
}

/// Translate a `LauncherOrigin` into a concrete on-disk path.
///
/// `Fs` is a no-op; `Repository` looks up the launcher in the cache and, for git-sourced
/// entries, materializes the checkout via `ensure_checkout`. Progress lines emitted by the
/// (blocking) checkout are buffered into a `Vec` and flushed to the launch feedback topic
/// after the resolver returns — quiet for cached/Fs entries, a few lines for fresh clones.
async fn resolve_launcher_origin(
    ctx: &ProcessLaunchContext,
    origin: &LauncherOrigin,
) -> std::result::Result<PathBuf, String> {
    match origin {
        LauncherOrigin::Fs(path) => Ok(path.clone()),
        LauncherOrigin::Repository { name } => {
            let peppy_dirs = ctx.peppy_dirs.clone();
            let name_for_blocking = name.clone();
            let collected = Arc::new(StdMutex::new(Vec::<String>::new()));
            let collected_for_cb = Arc::clone(&collected);

            let result = tokio::task::spawn_blocking(move || {
                crate::services::repo::cache::resolve_repo_launcher_path(
                    &name_for_blocking,
                    &peppy_dirs,
                    &|line| {
                        collected_for_cb.lock().push(line.to_owned());
                    },
                )
            })
            .await
            .map_err(|e| format!("launcher resolver join error: {e}"))?;

            let captured: Vec<String> = std::mem::take(&mut *collected.lock());
            for line in captured {
                publish_stdout(ctx, line, LaunchFeedbackStep::LauncherStep).await;
            }
            result
        }
    }
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
    let mut planned_keys: HashSet<NameTagKey> = HashSet::new();

    for deployment in deployments.into_iter() {
        if deployment.instances.is_empty() {
            planning_errors.push(format!(
                "deployment {} must have at least one instance",
                deployment_label(&deployment)
            ));
            continue;
        }

        let (source, variant) =
            match node_source_from_deployment_source(&deployment, nodes_directory, &ctx.peppy_dirs)
            {
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

        let key = NameTagKey::new(&node_name, &node_tag);
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
) -> std::result::Result<Vec<NameTagKey>, LaunchResult> {
    publish_stdout(
        ctx,
        "Validating dependencies",
        LaunchFeedbackStep::LauncherStep,
    )
    .await;

    let root_key = NameTagKey::new(
        root_config.manifest.name.as_str(),
        root_config.manifest.tag.as_str(),
    );

    let mut configs_by_key: HashMap<NameTagKey, config::node::NodeConfig> = HashMap::new();
    configs_by_key.insert(root_key.clone(), root_config.clone());
    for item in planned {
        configs_by_key.insert(
            NameTagKey::new(&item.node_name, &item.node_tag),
            item.config.clone(),
        );
    }

    let planned_keys: HashSet<NameTagKey> = planned
        .iter()
        .map(|p| NameTagKey::new(&p.node_name, &p.node_tag))
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
                |name, tag| configs_by_key.get(&NameTagKey::new(name, tag)).cloned(),
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
    let mut deps_for: HashMap<NameTagKey, HashSet<NameTagKey>> = HashMap::new();
    for item in planned {
        let dependant_key = NameTagKey::new(&item.node_name, &item.node_tag);
        let mut deps = HashSet::new();
        for spec in node_stack::collect_dependency_specs(&item.config) {
            let dep_key = NameTagKey::new(&spec.node_name, &spec.node_tag);
            if dep_key != root_key && planned_keys.contains(&dep_key) {
                deps.insert(dep_key);
            }
        }
        deps_for.insert(dependant_key, deps);
    }

    // Stable topological sort using original plan order as tie-breaker.
    let ordered = topological_sort(planned, &deps_for, &ctx.log_path).map_err(|e| *e)?;

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
    deps_for: &HashMap<NameTagKey, HashSet<NameTagKey>>,
    log_path: &PathBuf,
) -> std::result::Result<Vec<NameTagKey>, Box<LaunchResult>> {
    let mut in_degree: HashMap<NameTagKey, usize> = HashMap::new();
    let mut dependents: HashMap<NameTagKey, Vec<NameTagKey>> = HashMap::new();

    for key in planned
        .iter()
        .map(|p| NameTagKey::new(&p.node_name, &p.node_tag))
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

    let order_index: HashMap<NameTagKey, usize> = planned
        .iter()
        .enumerate()
        .map(|(idx, p)| (NameTagKey::new(&p.node_name, &p.node_tag), idx))
        .collect();

    let mut ready: Vec<NameTagKey> = in_degree
        .iter()
        .filter(|(_, deg)| **deg == 0)
        .map(|(k, _)| k.clone())
        .collect();
    ready.sort_by_key(|k| order_index.get(k).copied().unwrap_or(usize::MAX));

    let mut queue: VecDeque<NameTagKey> = ready.into();
    let mut ordered: Vec<NameTagKey> = Vec::new();

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
        let mut drained: Vec<NameTagKey> = queue.drain(..).collect();
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
        return Err(Box::new(LaunchResult::failure(log_path, msg)));
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
    ordered: &[NameTagKey],
    planned_by_key: &HashMap<NameTagKey, PlannedDeployment>,
    backup_stack: &NodeStack,
    add_log_paths: &mut Vec<NodeAddLogEntry>,
    build_log_paths: &mut Vec<NodeBuildLogEntry>,
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

        let node_add_goal =
            NodeAddGoal::for_internal_execution(item.source.clone(), STACK_LAUNCH_GIT_HASH)
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
                let node_variant = result
                    .variant
                    .clone()
                    .unwrap_or_else(|| node_stack::DEFAULT_VARIANT.to_owned());

                // Stack launch chains directly from add into build, since the
                // launcher's contract is "the stack is up and running": an
                // `Added` entity isn't actually buildable from the user's
                // perspective until `node build` has run.
                let (build_result, build_log_path) = build_node_directly(
                    ctx,
                    node_name,
                    node_tag,
                    node_variant,
                    ctx.env_vars.clone(),
                )
                .await;

                let build_failed = build_result.is_err();
                if let Some(path) = build_log_path {
                    build_log_paths.push(NodeBuildLogEntry {
                        node_label: key.label(),
                        log_path: path,
                        failed: build_failed,
                    });
                }

                if let Err(err) = build_result {
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
    ordered: &[NameTagKey],
    planned_by_key: &HashMap<NameTagKey, PlannedDeployment>,
    backup_stack: &NodeStack,
    run_log_paths: &mut Vec<NodeRunLogEntry>,
) -> std::result::Result<(), LaunchResult> {
    publish_stdout(ctx, "Running nodes...", LaunchFeedbackStep::LauncherStep).await;

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
                LaunchFeedbackStep::RunningNode,
            )
            .await;

            let node_instance = config::runtime::NodeInstanceConfig {
                instance_id: instance.instance_id.clone(),
                arguments: instance.arguments.clone(),
                framework: resolve_framework(&instance.framework, ctx.daemon_use_sim_time),
            };
            let item_variant = item
                .variant
                .as_ref()
                .map(crate::services::node::variant::variant_label)
                .unwrap_or_else(|| config::runtime::DEFAULT_VARIANT.to_owned());
            let runtime_config = match RuntimeConfig::new(
                messaging_host.as_str(),
                messaging_port,
                node_instance,
                item.node_name.as_str(),
                ctx.bound_core_node.as_str(),
                &item_variant,
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

            let node_run_goal = NodeRunGoal::for_internal_execution(
                &runtime_config_json5,
                item.node_name.as_str(),
                item.node_tag.as_str(),
            )
            .with_env_vars(ctx.env_vars.clone());

            // Create log file for this node start
            let log_dir = ctx.peppy_dirs.logs_dir_run();
            let log_filename = format!("{}.log", instance_id);
            let (log_file, log_path) = match create_action_log_file(&log_dir, &log_filename) {
                Ok(r) => r,
                Err(e) => {
                    return Err(restore_stack(ctx, backup_stack, e).await);
                }
            };

            let (result, log_path) =
                start_node_directly(ctx, node_run_goal, runtime_config, log_path, log_file).await;

            let failed = result.as_ref().map(|r| !r.success).unwrap_or(true);
            if let Some(path) = log_path {
                run_log_paths.push(NodeRunLogEntry {
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
                            .unwrap_or_else(|| "node_run failed".to_string());
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
    user_payload: bytes::Bytes,
    feedback_publisher: ActionFeedbackPublisher,
    state: Arc<Mutex<ActionState<LaunchResult>>>,
    action_context: LaunchActionContext,
) -> PeppyResult<Payload> {
    let sender_instance_id = context.message().instance_id();

    // Check if already running (but don't set Running yet — we need the goal's timeout first)
    {
        let state_guard = state.lock().await;
        if matches!(*state_guard, ActionState::Running { .. }) {
            let response = LaunchGoalResponse::rejected("action already in progress");
            return response
                .encode()
                .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
                    identifier: "launch_goal".to_string(),
                    reason: format!("Failed to encode response: {}", e),
                });
        }
    }

    // Decode the goal before marking as Running so we can capture the user-supplied timeouts
    let goal = match LaunchGoal::decode(&user_payload) {
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

    // Now mark as Running. `timeout_secs` is gate-reporting only; 0 indicates "no enforced
    // budget" (when --max-timeout-secs is unset).
    {
        let mut state_guard = state.lock().await;
        *state_guard = ActionState::Running {
            started_at: std::time::Instant::now(),
            timeout_secs: goal.max_timeout_secs.unwrap_or(0),
        };
    }

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
            daemon_use_sim_time,
        } = action_context;
        let env_vars = goal.env_vars.clone();
        // Compute the launch deadline once. `None` => no overall deadline (idle-only).
        let launch_deadline = goal
            .max_timeout_secs
            .map(|n| Instant::now() + Duration::from_secs(n));
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
            launch_deadline,
            idle_timeouts: IdleTimeouts {
                add: Duration::from_secs(goal.node_add_idle_timeout_secs),
                build: Duration::from_secs(goal.node_build_idle_timeout_secs),
                run: Duration::from_secs(goal.node_run_idle_timeout_secs),
            },
            daemon_use_sim_time,
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

    // Step 4: Snapshot and clear stack (the snapshot helps in case an `build_cmd` or `run_cmd` fails on one of the nodes)
    let backup_stack = match snapshot_and_clear_stack(&ctx).await {
        Ok(result) => result,
        Err(launch_result) => return launch_result,
    };

    // Build lookup map
    let planned_by_key: HashMap<NameTagKey, PlannedDeployment> = planned
        .into_iter()
        .map(|item| (NameTagKey::new(&item.node_name, &item.node_tag), item))
        .collect();

    let mut add_log_paths: Vec<NodeAddLogEntry> = Vec::new();
    let mut build_log_paths: Vec<NodeBuildLogEntry> = Vec::new();
    let mut run_log_paths: Vec<NodeRunLogEntry> = Vec::new();

    // Step 5: Add nodes in dependency order
    let add_result = add_nodes_to_stack(
        &ctx,
        &ordered,
        &planned_by_key,
        &backup_stack,
        &mut add_log_paths,
        &mut build_log_paths,
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
                &mut run_log_paths,
            )
            .await,
        )
    } else {
        None
    };

    if let Err(mut launch_result) = add_result {
        launch_result.node_add_logs = add_log_paths;
        launch_result.node_build_logs = build_log_paths;
        return launch_result;
    }
    if let Some(Err(mut launch_result)) = start_result {
        launch_result.node_add_logs = add_log_paths;
        launch_result.node_build_logs = build_log_paths;
        launch_result.node_run_logs = run_log_paths;
        return launch_result;
    }

    publish_stdout(&ctx, "Launch complete", LaunchFeedbackStep::LauncherStep).await;
    LaunchResult::success(&ctx.log_path).with_node_logs(
        add_log_paths,
        build_log_paths,
        run_log_paths,
    )
}

/// Regression tests for the run-phase cancel-and-drain contract.
///
/// The invariant under test: when a run-phase timeout fires,
/// `run_phase_cancel_on_timeout` must signal the cancel token *and* drive the
/// phase future to completion so its cleanup runs — not drop it. Using
/// `tokio::time::pause()` + manual advancement so these tests are
/// deterministic (no wall-clock dependency, no risk of CI flake).
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Builds a phase future that signals `cleanup_ran` iff it observes the
    /// cancel token — simulating `run_node_run`'s `abort_started` branch.
    /// If instead the outer runner drops this future, the flag stays false
    /// and the test fails, matching the real-world orphan bug.
    async fn cancellable_phase(
        cancel: CancellationToken,
        cleanup_ran: Arc<AtomicBool>,
    ) -> &'static str {
        tokio::select! {
            _ = cancel.cancelled() => {
                cleanup_ran.store(true, Ordering::SeqCst);
                "cleaned_up"
            }
            _ = std::future::pending::<()>() => unreachable!("phase should never complete on its own in these tests"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_on_timeout_awaits_cleanup_on_idle_timeout() {
        let notify = Arc::new(Notify::new());
        let token = CancellationToken::new();
        let cleanup_ran = Arc::new(AtomicBool::new(false));

        let outcome = run_phase_cancel_on_timeout(
            cancellable_phase(token.clone(), Arc::clone(&cleanup_ran)),
            Arc::clone(&notify),
            Duration::from_millis(100),
            None,
            token,
        )
        .await;

        assert!(
            matches!(outcome, PhaseOutcome::IdleTimeout),
            "idle timeout branch expected",
        );
        assert!(
            cleanup_ran.load(Ordering::SeqCst),
            "phase future must be awaited after cancel so cleanup runs; \
             dropping it would leave this flag false (the orphan-process bug)",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_on_timeout_awaits_cleanup_on_max_deadline() {
        let notify = Arc::new(Notify::new());
        let token = CancellationToken::new();
        let cleanup_ran = Arc::new(AtomicBool::new(false));

        let deadline = Instant::now() + Duration::from_millis(50);
        let outcome = run_phase_cancel_on_timeout(
            cancellable_phase(token.clone(), Arc::clone(&cleanup_ran)),
            Arc::clone(&notify),
            // Idle much larger than max so only the deadline branch can fire.
            Duration::from_secs(600),
            Some(deadline),
            token,
        )
        .await;

        assert!(
            matches!(outcome, PhaseOutcome::MaxTimeout),
            "max timeout branch expected",
        );
        assert!(
            cleanup_ran.load(Ordering::SeqCst),
            "phase future must be awaited after max-deadline cancel so cleanup runs",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_on_timeout_returns_value_when_phase_completes_first() {
        let notify = Arc::new(Notify::new());
        let token = CancellationToken::new();
        let cleanup_ran = Arc::new(AtomicBool::new(false));
        let cleanup_ran_for_phase = Arc::clone(&cleanup_ran);

        // Phase completes immediately with a value — no timeout should fire.
        let phase = async move {
            // Reset-like ping proves we keep the happy-path contract (no cancel signal).
            let _ = cleanup_ran_for_phase;
            "ok"
        };

        let outcome = run_phase_cancel_on_timeout(
            phase,
            Arc::clone(&notify),
            Duration::from_millis(100),
            Some(Instant::now() + Duration::from_millis(100)),
            token.clone(),
        )
        .await;

        match outcome {
            PhaseOutcome::Completed(v) => assert_eq!(v, "ok"),
            _ => panic!("phase should complete before any timeout fires"),
        }
        assert!(
            !token.is_cancelled(),
            "happy path must not cancel the token",
        );
    }

    /// Per-instance override beats the daemon default in either direction.
    /// `Some(true)` forces sim even when the daemon default is wall;
    /// `Some(false)` forces wall even when the daemon default is sim.
    #[test]
    fn resolve_framework_per_instance_wins() {
        let force_sim = config::launcher::FrameworkOverrides {
            use_sim_time: Some(true),
        };
        assert!(resolve_framework(&force_sim, false).use_sim_time);

        let force_wall = config::launcher::FrameworkOverrides {
            use_sim_time: Some(false),
        };
        assert!(!resolve_framework(&force_wall, true).use_sim_time);
    }

    /// When the instance omits the override, the daemon default decides.
    #[test]
    fn resolve_framework_falls_through_to_daemon_default() {
        let none = config::launcher::FrameworkOverrides::default();
        assert!(!resolve_framework(&none, false).use_sim_time);
        assert!(resolve_framework(&none, true).use_sim_time);
    }
}
