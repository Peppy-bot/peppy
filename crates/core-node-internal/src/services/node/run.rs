use super::super::action_loop::{ActionResult, ActionState, GoalHandler, run_action_loop};
use super::gate::ConcurrencyGate;
use super::{FeedbackLine, FeedbackStream, create_action_log_file, write_error_to_log};
use crate::Result;
use crate::names;
use config::consts::PeppyDirs;
use config::node::Name;
use config::runtime::RuntimeConfig;
use config::{AnyType, resolve_parameter_path};
use core_node_api::encoding::{NodeRunFeedback, NodeRunGoal, NodeRunGoalResponse, NodeRunResult};
use futures::FutureExt;
use node_stack::{self, NodeStack};
use parking_lot::Mutex as StdMutex;
use peppylib::encoding::health::NodeHealthRequest;
use peppylib::encoding::ready::NodeReadyRequest;
use peppylib::messaging::{
    NODE_HEALTH_SERVICE, NODE_READY_SERVICE, ServiceRequestContext, TopicPublisher,
};
use peppylib::types::Payload;
use peppylib::{ActionMessenger, MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use std::collections::BTreeMap;
use std::fs::File;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::process::Child;
use tokio::sync::{Mutex, Notify, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::debug;

const STARTUP_OUTPUT_MAX_WAIT: Duration = Duration::from_millis(100);
const STARTUP_OUTPUT_QUIET_WINDOW: Duration = Duration::from_millis(10);
const CONTAINER_STARTUP_OUTPUT_MAX_WAIT: Duration = Duration::from_secs(2);
const CONTAINER_STARTUP_OUTPUT_QUIET_WINDOW: Duration = Duration::from_millis(100);
const FEEDBACK_FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct NodeRunServiceConfig {
    pub node_startup_timeout: Duration,
    pub node_start_health_timeout: Duration,
    pub peppy_dirs: PeppyDirs,
    pub health_monitor_interval: Duration,
    pub health_monitor_timeout: Duration,
    pub health_monitor_max_failures: u32,
}

#[derive(Clone)]
pub(crate) struct NodeRunActionContext {
    pub(crate) node_stack: Arc<NodeStack>,
    pub(crate) messenger: MessengerHandle,
    pub(crate) core_node_name: String,
    pub(crate) caller_instance_id: String,
    pub(crate) node_startup_timeout: Duration,
    pub(crate) node_start_health_timeout: Duration,
    pub(crate) peppy_dirs: PeppyDirs,
    pub(crate) health_monitor_interval: Duration,
    pub(crate) health_monitor_timeout: Duration,
    pub(crate) health_monitor_max_failures: u32,
}

struct ProcessNodeRunContext {
    action: NodeRunActionContext,
    feedback_tx: mpsc::UnboundedSender<FeedbackLine>,
    log_file: Arc<StdMutex<File>>,
    sender_instance_id: String,
}

pub async fn listen_for_node_run(
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    config: NodeRunServiceConfig,
) -> Result<JoinHandle<Result<()>>> {
    let action = ActionMessenger::expose(
        messenger,
        core_node_name,
        instance_id,
        node_name,
        names::NODE_RUN_ACTION,
    )
    .await?;

    let handler = NodeRunGoalHandler {
        context: NodeRunActionContext {
            node_stack,
            messenger: messenger.clone(),
            core_node_name: core_node_name.to_string(),
            caller_instance_id: instance_id.to_string(),
            node_startup_timeout: config.node_startup_timeout,
            node_start_health_timeout: config.node_start_health_timeout,
            peppy_dirs: config.peppy_dirs,
            health_monitor_interval: config.health_monitor_interval,
            health_monitor_timeout: config.health_monitor_timeout,
            health_monitor_max_failures: config.health_monitor_max_failures,
        },
        gate: ConcurrencyGate::new(),
    };

    let handle = tokio::spawn(async move { run_action_loop(action, handler).await });

    Ok(handle)
}

impl ActionResult for NodeRunResult {
    fn identifier() -> &'static str {
        "node_run_result"
    }

    fn encode_result(&self) -> crate::Result<Payload> {
        self.encode().map_err(Into::into)
    }
}

#[derive(Clone)]
struct NodeRunGoalHandler {
    context: NodeRunActionContext,
    gate: ConcurrencyGate,
}

impl GoalHandler for NodeRunGoalHandler {
    type Result = NodeRunResult;

    async fn handle_goal(
        &self,
        context: ServiceRequestContext,
        feedback_publisher: TopicPublisher,
        state: Arc<Mutex<ActionState<NodeRunResult>>>,
    ) -> PeppyResult<Payload> {
        handle_goal_request(
            context,
            feedback_publisher,
            state,
            self.context.clone(),
            self.gate.clone(),
        )
        .await
    }
}

/// Validates that all required parameters from the schema are present in the provided arguments.
/// Returns a list of all missing parameter paths (e.g., ["device.physical", "video.frame_rate"]).
fn validate_parameters(
    schema: &std::collections::BTreeMap<String, AnyType>,
    arguments: &std::collections::BTreeMap<String, AnyType>,
    prefix: &str,
) -> Vec<String> {
    let mut missing = Vec::new();

    for (key, schema_value) in schema {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{}.{}", prefix, key)
        };

        match arguments.get(key) {
            None => {
                // Parameter is missing - collect all nested paths if it's an object schema
                collect_all_required_paths(schema_value, &path, &mut missing);
            }
            Some(arg_value) => {
                // Parameter exists - check nested objects recursively
                if let AnyType::Object(schema_fields) = schema_value {
                    if let AnyType::Object(arg_fields) = arg_value {
                        missing.extend(validate_parameters(schema_fields, arg_fields, &path));
                    } else {
                        // Schema expects an object but argument is not an object
                        collect_all_required_paths(schema_value, &path, &mut missing);
                    }
                }
            }
        }
    }

    missing
}

/// Recursively collects all required parameter paths from a schema value.
/// Used when a parameter is completely missing to report all its nested fields.
fn collect_all_required_paths(schema_value: &AnyType, path: &str, missing: &mut Vec<String>) {
    match schema_value {
        AnyType::Object(fields) => {
            // For object schemas, recursively collect all nested paths
            for (key, nested_value) in fields {
                let nested_path = format!("{}.{}", path, key);
                collect_all_required_paths(nested_value, &nested_path, missing);
            }
        }
        _ => {
            // Leaf value (type specification like "string", "u16", etc.)
            missing.push(path.to_string());
        }
    }
}

#[derive(Clone)]
struct FeedbackSync {
    read_count: Arc<AtomicU64>,
    published_count: Arc<AtomicU64>,
    notify: Arc<Notify>,
    read_notify: Arc<Notify>,
    stdout_seen: Arc<AtomicBool>,
    stdout_notify: Arc<Notify>,
}

impl FeedbackSync {
    fn new() -> Self {
        Self {
            read_count: Arc::new(AtomicU64::new(0)),
            published_count: Arc::new(AtomicU64::new(0)),
            notify: Arc::new(Notify::new()),
            read_notify: Arc::new(Notify::new()),
            stdout_seen: Arc::new(AtomicBool::new(false)),
            stdout_notify: Arc::new(Notify::new()),
        }
    }

    fn signal_stdout(&self) {
        if !self.stdout_seen.swap(true, Ordering::Relaxed) {
            self.stdout_notify.notify_waiters();
        }
    }

    fn increment_read(&self) {
        self.read_count.fetch_add(1, Ordering::Relaxed);
        self.read_notify.notify_waiters();
    }

    fn increment_published(&self) {
        self.published_count.fetch_add(1, Ordering::Relaxed);
        self.notify.notify_waiters();
    }

    /// Waits until all read lines have been published, or until `timeout` elapses.
    /// Returns `true` if all lines were flushed, `false` on timeout.
    async fn flush_with_timeout(&self, timeout: Duration) -> bool {
        let target = self.read_count.load(Ordering::Relaxed);
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.published_count.load(Ordering::Relaxed) >= target {
                return true;
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return false;
            }
            let remaining = deadline - now;
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.published_count.load(Ordering::Relaxed) >= target {
                return true;
            }
            match tokio::time::timeout(remaining, notified).await {
                Ok(_) => {}
                Err(_) => return false,
            }
        }
    }

    /// Flush pending feedback, logging a debug warning on timeout.
    async fn flush_or_warn(&self, instance_id: &str) {
        if !self.flush_with_timeout(FEEDBACK_FLUSH_TIMEOUT).await {
            debug!(
                "feedback flush timed out for node instance '{}'",
                instance_id
            );
        }
    }

    async fn wait_for_read_quiescence(
        &self,
        max_wait: Duration,
        quiet_window: Duration,
        require_stdout: bool,
    ) {
        let start = Instant::now();
        let mut last_read = self.read_count.load(Ordering::Relaxed);
        let mut saw_read = last_read > 0;

        loop {
            let elapsed = Instant::now().duration_since(start);
            if elapsed >= max_wait {
                break;
            }

            let remaining = max_wait - elapsed;
            let wait = quiet_window.min(remaining);

            match tokio::time::timeout(wait, self.read_notify.notified()).await {
                Ok(_) => {
                    let current = self.read_count.load(Ordering::Relaxed);
                    if current != last_read {
                        last_read = current;
                        saw_read = true;
                    }
                }
                Err(_) => {
                    // No reads during `wait` (quiet). If we've already seen at least one read,
                    // treat that as the end of the initial burst — but only if we don't
                    // require stdout or stdout has already been seen. For containers,
                    // stderr-only quiet periods (e.g. during SIF-to-sandbox conversion)
                    // should not be treated as quiescence.
                    if saw_read && (!require_stdout || self.stdout_seen.load(Ordering::Relaxed)) {
                        break;
                    }
                }
            }
        }
    }
}

impl node_stack::OutputReaderHooks for FeedbackSync {
    fn on_first_stdout_line(&self) {
        self.signal_stdout();
    }

    fn on_line_read(&self) {
        self.increment_read();
    }
}

/// Runs the node run pipeline: calls [`process_node_run`] and catches panics.
///
/// The caller is responsible for creating the log file and feedback channel.
///
/// This is the shared implementation used by both the action-server path
/// ([`handle_goal_request`]) and the direct-call path from `stack_launch`.
///
/// `cancel_token` lets an outer orchestrator (currently `stack_launch`'s
/// idle/max-timeout watchdog) request an in-flight abort. When cancelled after
/// the child process has been spawned but before the `Starting → Started`
/// commit, the pipeline explicitly calls `abort_started` to SIGKILL the child
/// and tear down its `Starting` entry; if cancelled before the spawn, it
/// returns without starting anything. Callers that don't need cancellation
/// pass `CancellationToken::new()` and never trigger it.
pub(crate) async fn run_node_run(
    goal: NodeRunGoal,
    runtime_config: RuntimeConfig,
    action_context: NodeRunActionContext,
    feedback_tx: mpsc::UnboundedSender<FeedbackLine>,
    log_file: Arc<StdMutex<File>>,
    sender_instance_id: String,
    cancel_token: CancellationToken,
) -> NodeRunResult {
    let log_file_for_panic = log_file.clone();

    let process_context = ProcessNodeRunContext {
        action: action_context,
        feedback_tx,
        log_file,
        sender_instance_id,
    };
    match AssertUnwindSafe(process_node_run(
        goal,
        runtime_config,
        process_context,
        cancel_token,
    ))
    .catch_unwind()
    .await
    {
        Ok(result) => result,
        Err(panic_payload) => {
            let msg = format!(
                "node_run task panicked: {}",
                super::panic_message(&*panic_payload)
            );
            tracing::error!("{}", msg);
            write_error_to_log(&log_file_for_panic, &msg);
            NodeRunResult::failure(msg)
        }
    }
}

async fn handle_goal_request(
    context: ServiceRequestContext,
    feedback_publisher: TopicPublisher,
    state: Arc<Mutex<ActionState<NodeRunResult>>>,
    action_context: NodeRunActionContext,
    gate: ConcurrencyGate,
) -> PeppyResult<Payload> {
    let sender_instance_id = context.message().instance_id().to_string();
    let payload = context.message().payload();

    let goal = match NodeRunGoal::decode(payload.as_ref()) {
        Ok(goal) => goal,
        Err(e) => {
            return encode_rejected_start_goal(format!("invalid payload: {}", e));
        }
    };

    {
        let mut state_guard = state.lock().await;
        if let super::gate::Admission::AlreadyRunning { remaining_secs } =
            gate.try_admit(&mut state_guard, goal.timeout_secs, false)
        {
            return encode_rejected_start_goal(format!(
                "action already in progress (times out in {remaining_secs}s)"
            ));
        }
    }

    // Parse runtime config to get instance_id for log file naming
    let runtime_config: RuntimeConfig = match serde_json5::from_str(&goal.runtime_config_json5) {
        Ok(config) => config,
        Err(e) => {
            let error_msg = format!("Failed to parse PEPPY_RUNTIME_CONFIG: {}", e);
            let mut state_guard = state.lock().await;
            *state_guard = ActionState::Rejected;
            return encode_rejected_start_goal(error_msg);
        }
    };

    let instance_id_str = runtime_config.node_instance.instance_id.as_str();

    debug!(
        "Received `node_run` goal from {sender_instance_id}, node={}:{}, instance_id={}, runtime_config_len={}",
        goal.node_name,
        goal.tag,
        instance_id_str,
        goal.runtime_config_json5.len()
    );

    // Create log file for stdout/stderr
    let log_dir = action_context.peppy_dirs.logs_dir_run();
    let log_filename = format!("{}.log", instance_id_str);
    let (log_file, log_path) = match create_action_log_file(&log_dir, &log_filename) {
        Ok(result) => result,
        Err(error_msg) => {
            debug!("{}", error_msg);
            let mut state_guard = state.lock().await;
            *state_guard = ActionState::Rejected;
            return encode_rejected_start_goal(error_msg);
        }
    };

    debug!("Created log file for node run: {}", log_path.display());

    // Panics are caught via catch_unwind so the state always transitions to
    // Completed — without this, a panic silently aborts the task and leaves
    // the state stuck on Running, causing clients to time out.
    let state_clone = Arc::clone(&state);
    tokio::spawn(async move {
        let (feedback_tx, feedback_rx) = mpsc::unbounded_channel::<FeedbackLine>();
        let _consumer_handle =
            super::spawn_feedback_forwarder(feedback_rx, feedback_publisher.clone(), |line| {
                NodeRunFeedback::from_stream(line.stream, &line.line).encode()
            });

        // Action-server path has no outer cancellation source; the internal
        // per-step timeouts inside `run_node_run` remain the only way out.
        let result = run_node_run(
            goal,
            runtime_config,
            action_context,
            feedback_tx,
            log_file,
            sender_instance_id,
            CancellationToken::new(),
        )
        .await;

        let mut state_guard = state_clone.lock().await;
        *state_guard = ActionState::Completed { result };
    });

    super::encode_response_or_err(
        "node_run_goal",
        NodeRunGoalResponse::accepted(&log_path).encode(),
    )
}

fn encode_rejected_start_goal(reason: impl Into<String>) -> PeppyResult<Payload> {
    super::encode_response_or_err(
        "node_run_goal",
        NodeRunGoalResponse::rejected(reason).encode(),
    )
}

async fn process_node_run(
    goal: NodeRunGoal,
    runtime_config: RuntimeConfig,
    ctx: ProcessNodeRunContext,
    cancel_token: CancellationToken,
) -> NodeRunResult {
    let sender_instance_id = ctx.sender_instance_id.as_str();
    let NodeRunGoal {
        runtime_config_json5,
        node_name,
        tag,
        env_vars,
        ..
    } = goal;
    let mut env_vars = match super::validate_goal_env_vars(&env_vars) {
        Ok(vars) => vars,
        Err(e) => {
            let msg = e.to_string();
            write_error_to_log(&ctx.log_file, &msg);
            return NodeRunResult::failure(msg);
        }
    };

    let instance_id_str = runtime_config.node_instance.instance_id.as_str();
    let instance_id = match Name::new(instance_id_str) {
        Ok(name) => name,
        Err(e) => {
            let msg = format!("Invalid instance_id: {}", e);
            write_error_to_log(&ctx.log_file, &msg);
            return NodeRunResult::failure(msg);
        }
    };

    debug!(
        "Processing `node_run` from {sender_instance_id}, node={}:{}, instance_id={}",
        node_name, tag, instance_id_str
    );

    let entity_handle = match ctx.action.node_stack.find(&node_name, &tag) {
        Some(entity) => entity,
        None => {
            let msg = format!("Node '{}:{}' not found in node stack", node_name, tag);
            write_error_to_log(&ctx.log_file, &msg);
            return NodeRunResult::failure(msg);
        }
    };

    let node_config = {
        let guard = entity_handle.read();
        if guard.artifact_path().is_none() {
            let msg = format!(
                "Node '{}:{}' has not been built yet (still in Added stage)",
                node_name, tag
            );
            drop(guard);
            write_error_to_log(&ctx.log_file, &msg);
            return NodeRunResult::failure(msg);
        }
        guard.config().clone()
    };

    let sccache_injected =
        super::inject_rust_build_env(&mut env_vars, node_config.execution.language);
    if sccache_injected {
        let _ = ctx.feedback_tx.send(FeedbackLine {
            stream: FeedbackStream::Stdout,
            line: "Using sccache for Rust compilation".to_string(),
        });
    }
    super::inject_node_runtime_env(
        &mut env_vars,
        node_config.manifest.name.as_str(),
        node_config.manifest.tag.as_str(),
    );

    // Validate that all required parameters are provided before starting the node
    let missing_params = validate_parameters(
        &node_config.execution.parameters,
        &runtime_config.node_instance.arguments,
        "",
    );
    if !missing_params.is_empty() {
        let msg = format!("Missing required parameters: {}", missing_params.join(", "));
        write_error_to_log(&ctx.log_file, &msg);
        return NodeRunResult::failure(msg);
    }

    let is_container = node_config.execution.container.is_some();

    let container_config = node_config.execution.container.as_ref();
    let raw_mount_paths = container_config
        .and_then(|c| c.mount_paths.as_deref())
        .unwrap_or_default();
    let resolved_mount_paths = match resolve_mount_path_parameters(
        raw_mount_paths,
        &runtime_config.node_instance.arguments,
    ) {
        Ok(paths) => paths,
        Err(msg) => {
            write_error_to_log(&ctx.log_file, &msg);
            return NodeRunResult::failure(msg);
        }
    };

    // Container nodes need their runtime config rewritten with the apptainer
    // host_gateway so that requests inside the container can reach the daemon.
    let runtime_config_json5 = if is_container {
        let apptainer = match tokio::task::spawn_blocking(containers::Apptainer::new).await {
            Ok(Ok(a)) => a,
            Ok(Err(e)) => {
                let msg = format!("Failed to initialize Apptainer: {}", e);
                write_error_to_log(&ctx.log_file, &msg);
                return NodeRunResult::failure(msg);
            }
            Err(e) => {
                let msg = format!("Apptainer initialization task failed: {}", e);
                write_error_to_log(&ctx.log_file, &msg);
                return NodeRunResult::failure(msg);
            }
        };
        match apptainer.host_gateway() {
            Some(gateway) => {
                let mut cfg = runtime_config.clone();
                cfg.messaging_host = gateway.to_string();
                match serde_json5::to_string(&cfg) {
                    Ok(json) => json,
                    Err(e) => {
                        let msg = format!("Failed to serialize runtime config: {}", e);
                        write_error_to_log(&ctx.log_file, &msg);
                        return NodeRunResult::failure(msg);
                    }
                }
            }
            None => runtime_config_json5,
        }
    } else {
        runtime_config_json5
    };

    // Set up the FeedbackSync + two-channel forwarder. The internal channel
    // feeds the entity's output readers; the forwarder copies lines onto the
    // daemon's external feedback topic and increments published_count so
    // FeedbackSync can detect quiescence.
    let publish_enabled = Arc::new(AtomicBool::new(true));
    let feedback_sync = FeedbackSync::new();

    let (internal_feedback_tx, mut internal_feedback_rx) =
        mpsc::unbounded_channel::<FeedbackLine>();
    let external_feedback_tx = ctx.feedback_tx.clone();
    let feedback_sync_publisher = feedback_sync.clone();
    tokio::spawn(async move {
        while let Some(line) = internal_feedback_rx.recv().await {
            let _ = external_feedback_tx.send(FeedbackLine {
                stream: line.stream,
                line: line.line,
            });
            feedback_sync_publisher.increment_published();
        }
    });

    let start_ctx = node_stack::StartContext {
        instance_id: &instance_id,
        runtime_config_json5: &runtime_config_json5,
        env_vars: &env_vars,
        mount_paths_resolved: &resolved_mount_paths,
        peppy_dirs: &ctx.action.peppy_dirs,
        output_sinks: node_stack::OutputSinks {
            feedback_tx: internal_feedback_tx.clone(),
            log_file: Arc::clone(&ctx.log_file),
            publish_enabled: Arc::clone(&publish_enabled),
            hooks: Arc::new(feedback_sync.clone()),
        },
    };
    // Reject early if an outer orchestrator already cancelled us — avoids
    // spawning a child process we're only going to tear down on the next line.
    if cancel_token.is_cancelled() {
        let msg = "cancelled before node process spawn".to_string();
        write_error_to_log(&ctx.log_file, &msg);
        feedback_sync.flush_or_warn(instance_id_str).await;
        publish_enabled.store(false, Ordering::Release);
        return NodeRunResult::failure(msg);
    }

    let (mut child, started_ctx) =
        match node_stack::NodeEntity::prepare_and_spawn(&entity_handle, start_ctx).await {
            Ok(t) => t,
            Err(e) => {
                let msg = e.to_string();
                write_error_to_log(&ctx.log_file, &msg);
                feedback_sync.flush_or_warn(instance_id_str).await;
                publish_enabled.store(false, Ordering::Release);
                return NodeRunResult::failure(msg);
            }
        };

    let signal_target = NodeSignalTarget {
        messenger: &ctx.action.messenger,
        core_node_name: &ctx.action.core_node_name,
        caller_instance_id: &ctx.action.caller_instance_id,
        target_node_name: runtime_config.node_name.as_str(),
        target_core_node: runtime_config.bound_core_node.as_str(),
        target_instance_id: instance_id_str,
    };

    debug!(
        "Successfully spawned node instance '{}', waiting for ready signal for {}s...",
        instance_id_str,
        ctx.action.node_startup_timeout.as_secs()
    );

    // Race the ready-signal wait against external cancellation. If the outer
    // idle/max-timeout watchdog cancels while the node is quietly starting,
    // we must SIGKILL the child and unregister the `Starting` instance —
    // otherwise the OS process outlives the launch failure.
    let ready_outcome = tokio::select! {
        biased;
        _ = cancel_token.cancelled() => StartupOutcome::Cancelled,
        res = wait_for_ready_signal(&signal_target, ctx.action.node_startup_timeout, &mut child) => {
            match res {
                Ok(()) => StartupOutcome::Ok,
                Err(e) => StartupOutcome::Failed(e),
            }
        }
    };

    if let Some(reason) = startup_abort_reason(&ready_outcome) {
        debug!(
            "Aborting node instance '{}' during ready wait: {}",
            instance_id_str, reason
        );
        let msg = node_stack::NodeEntity::abort_started(
            &entity_handle,
            child,
            started_ctx,
            reason.to_string(),
            &instance_id,
        )
        .await;
        feedback_sync.flush_or_warn(instance_id_str).await;
        publish_enabled.store(false, Ordering::Release);
        return NodeRunResult::failure(msg);
    }

    debug!(
        "Node instance '{}' is ready, performing health check...",
        instance_id_str
    );

    let health_outcome = tokio::select! {
        biased;
        _ = cancel_token.cancelled() => StartupOutcome::Cancelled,
        res = perform_health_check(&signal_target, ctx.action.node_start_health_timeout, &mut child) => {
            match res {
                Ok(()) => StartupOutcome::Ok,
                Err(e) => StartupOutcome::Failed(e),
            }
        }
    };

    match health_outcome {
        StartupOutcome::Ok => {
            debug!(
                "Health check passed for node instance '{}'",
                instance_id_str
            );
            // Last chance to bail out cleanly — once `commit_started` succeeds
            // the child is owned by the stack and a late cancel would have to
            // go through the normal stop path instead of abort_started.
            if cancel_token.is_cancelled() {
                let reason = "cancelled before commit".to_string();
                debug!(
                    "Aborting node instance '{}' after health check: {}",
                    instance_id_str, reason
                );
                let msg = node_stack::NodeEntity::abort_started(
                    &entity_handle,
                    child,
                    started_ctx,
                    reason,
                    &instance_id,
                )
                .await;
                feedback_sync.flush_or_warn(instance_id_str).await;
                publish_enabled.store(false, Ordering::Release);
                return NodeRunResult::failure(msg);
            }
            let pid = child.id().unwrap_or(0);
            let commit_result = node_stack::NodeEntity::commit_started(
                &entity_handle,
                child,
                started_ctx,
                instance_id.clone(),
            )
            .await;
            match commit_result {
                Ok(_) => {
                    spawn_health_monitor(HealthMonitorParams {
                        messenger: ctx.action.messenger.clone(),
                        core_node_name: ctx.action.core_node_name.clone(),
                        caller_instance_id: ctx.action.caller_instance_id.clone(),
                        target_node_name: runtime_config.node_name.as_str().to_owned(),
                        target_core_node: runtime_config.bound_core_node.as_str().to_owned(),
                        target_instance_id: instance_id.clone(),
                        node_tag: tag.clone(),
                        node_stack: Arc::clone(&ctx.action.node_stack),
                        peppy_dirs: ctx.action.peppy_dirs.clone(),
                        interval: ctx.action.health_monitor_interval,
                        timeout: ctx.action.health_monitor_timeout,
                        max_failures: ctx.action.health_monitor_max_failures,
                    });

                    let (max_wait, quiet_window) = if is_container {
                        (
                            CONTAINER_STARTUP_OUTPUT_MAX_WAIT,
                            CONTAINER_STARTUP_OUTPUT_QUIET_WINDOW,
                        )
                    } else {
                        (STARTUP_OUTPUT_MAX_WAIT, STARTUP_OUTPUT_QUIET_WINDOW)
                    };
                    feedback_sync
                        .wait_for_read_quiescence(max_wait, quiet_window, is_container)
                        .await;
                    let result = NodeRunResult::success(pid);
                    feedback_sync.flush_or_warn(instance_id_str).await;
                    publish_enabled.store(false, Ordering::Release);
                    result
                }
                Err(e) => {
                    let msg = format!("Failed to register instance: {}", e);
                    write_error_to_log(&ctx.log_file, &msg);
                    feedback_sync.flush_or_warn(instance_id_str).await;
                    publish_enabled.store(false, Ordering::Release);
                    NodeRunResult::failure(msg)
                }
            }
        }
        StartupOutcome::Cancelled | StartupOutcome::Failed(_) => {
            let reason = match health_outcome {
                StartupOutcome::Cancelled => "cancelled during health check".to_string(),
                StartupOutcome::Failed(e) => e,
                StartupOutcome::Ok => unreachable!(),
            };
            debug!(
                "Aborting node instance '{}' during health check: {}",
                instance_id_str, reason
            );
            let msg = node_stack::NodeEntity::abort_started(
                &entity_handle,
                child,
                started_ctx,
                reason,
                &instance_id,
            )
            .await;
            feedback_sync.flush_or_warn(instance_id_str).await;
            publish_enabled.store(false, Ordering::Release);
            NodeRunResult::failure(msg)
        }
    }
}

/// Outcome of a startup step (ready-signal wait or health check) racing
/// against external cancellation.
enum StartupOutcome {
    Ok,
    Cancelled,
    Failed(String),
}

fn startup_abort_reason(outcome: &StartupOutcome) -> Option<&str> {
    match outcome {
        StartupOutcome::Ok => None,
        StartupOutcome::Cancelled => Some("cancelled during ready-signal wait"),
        StartupOutcome::Failed(msg) => Some(msg.as_str()),
    }
}

/// Replaces `${parameters:...}` tokens in mount paths with actual argument values.
///
/// Each `${parameters:<dot.path>}` is resolved against the runtime `arguments`.
/// Only `AnyType::String` values are accepted — other types produce an error.
///
/// After resolution, the source (host) portion of each mount path is validated
/// against the blocked system directories list.
fn resolve_mount_path_parameters(
    mount_paths: &[String],
    arguments: &BTreeMap<String, AnyType>,
) -> std::result::Result<Vec<String>, String> {
    let mut resolved = Vec::with_capacity(mount_paths.len());
    for mount in mount_paths {
        let mut result = String::with_capacity(mount.len());
        let mut remaining: &str = mount;

        while let Some(start) = remaining.find("${parameters:") {
            result.push_str(&remaining[..start]);
            let after_prefix = &remaining[start + "${parameters:".len()..];
            let end = after_prefix
                .find('}')
                .ok_or_else(|| format!("Unclosed parameter reference in mount path: {mount}"))?;
            let dot_path = &after_prefix[..end];

            match resolve_parameter_path(arguments, dot_path) {
                Some(AnyType::String(value)) => {
                    result.push_str(value);
                }
                Some(other) => {
                    return Err(format!(
                        "Parameter `{dot_path}` in mount path must be a string, got {}",
                        other.type_name()
                    ));
                }
                None => {
                    return Err(format!(
                        "Parameter `{dot_path}` referenced in mount path not found in arguments"
                    ));
                }
            }
            remaining = &after_prefix[end + 1..];
        }
        result.push_str(remaining);

        // Validate the resolved source path against blocked system directories.
        let src = result.split(':').next().unwrap_or(&result);
        if config::node::is_blocked_mount_source(src) {
            return Err(format!(
                "Resolved mount path `{result}` uses a blocked system directory `{src}` as source — use a subdirectory instead"
            ));
        }

        resolved.push(result);
    }
    Ok(resolved)
}

struct NodeSignalTarget<'a> {
    messenger: &'a MessengerHandle,
    core_node_name: &'a str,
    caller_instance_id: &'a str,
    target_node_name: &'a str,
    target_core_node: &'a str,
    target_instance_id: &'a str,
}

/// Performs a health check on a newly started node instance.
/// Polls the node's health service with a timeout and returns Ok if the node responds.
/// Also monitors the child process to detect early exits.
async fn perform_health_check(
    target: &NodeSignalTarget<'_>,
    timeout: Duration,
    child: &mut Child,
) -> std::result::Result<(), String> {
    let request_payload = NodeHealthRequest::new()
        .encode()
        .map_err(|e| format!("failed to encode node health request: {e}"))?;
    let deadline = Instant::now() + timeout;
    let mut last_err: Option<PeppyError> = None;

    // Poll in short intervals to avoid a startup race where the node subscribes to
    // `node_health` after the first request has already been published.
    loop {
        // Check if the child process has exited
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(format!(
                    "node process exited before becoming healthy (status={status})"
                ));
            }
            Ok(None) => {}
            Err(err) => return Err(format!("failed to query node process status: {err}")),
        }

        let now = Instant::now();
        if now >= deadline {
            let err = last_err.unwrap_or_else(|| PeppyError::ServiceTimeout {
                instance_id: Some(target.target_instance_id.to_string()),
                service_name: NODE_HEALTH_SERVICE.to_string(),
            });
            return Err(format!("health check timed out: {err}"));
        }

        let remaining = deadline - now;
        let attempt_timeout = remaining.min(Duration::from_millis(500));

        match ServiceMessenger::poll(
            target.messenger,
            target.core_node_name,
            target.caller_instance_id,
            target.target_node_name,
            NODE_HEALTH_SERVICE,
            Some(target.target_core_node),
            Some(target.target_instance_id),
            request_payload.clone(),
            attempt_timeout,
        )
        .await
        {
            Ok(_) => return Ok(()),
            Err(err) => {
                last_err = Some(err);
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

/// Waits for a node to signal it's ready (runner::run() has started).
/// Polls the node's ready service with a timeout and returns Ok when the node responds.
/// Also monitors the child process to detect early exits (e.g., compilation failures).
///
/// This is used during startup to wait for compilation to complete before
/// starting the health check timer.
async fn wait_for_ready_signal(
    target: &NodeSignalTarget<'_>,
    timeout: Duration,
    child: &mut Child,
) -> std::result::Result<(), String> {
    let request_payload = NodeReadyRequest::new()
        .encode()
        .map_err(|e| format!("failed to encode node ready request: {e}"))?;
    let deadline = Instant::now() + timeout;
    let mut last_err: Option<PeppyError> = None;

    // Poll in short intervals to detect when the node becomes ready
    loop {
        // Check if the child process has exited (e.g., compilation failed)
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(format!(
                    "node process exited during startup (status={status})"
                ));
            }
            Ok(None) => {}
            Err(err) => return Err(format!("failed to query node process status: {err}")),
        }

        let now = Instant::now();
        if now >= deadline {
            let err = last_err.unwrap_or_else(|| PeppyError::ServiceTimeout {
                instance_id: Some(target.target_instance_id.to_string()),
                service_name: NODE_READY_SERVICE.to_string(),
            });
            return Err(format!(
                "startup timed out waiting for node to be ready (node may still be compiling): {err}"
            ));
        }

        let remaining = deadline - now;
        let attempt_timeout = remaining.min(Duration::from_millis(500));

        match ServiceMessenger::poll(
            target.messenger,
            target.core_node_name,
            target.caller_instance_id,
            target.target_node_name,
            NODE_READY_SERVICE,
            Some(target.target_core_node),
            Some(target.target_instance_id),
            request_payload.clone(),
            attempt_timeout,
        )
        .await
        {
            Ok(_) => return Ok(()),
            Err(err) => {
                last_err = Some(err);
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

struct HealthMonitorParams {
    messenger: MessengerHandle,
    core_node_name: String,
    caller_instance_id: String,
    target_node_name: String,
    target_core_node: String,
    target_instance_id: Name,
    node_tag: String,
    node_stack: Arc<NodeStack>,
    peppy_dirs: PeppyDirs,
    interval: Duration,
    timeout: Duration,
    max_failures: u32,
}

/// Spawns a background task that periodically polls the node's health service.
/// If `max_failures` consecutive health checks fail, the instance is removed
/// from the node stack and the event is logged to the stack log.
///
/// The task exits when:
/// - The instance is no longer found in the stack (removed externally).
/// - The health checks fail consecutively `max_failures` times.
fn spawn_health_monitor(p: HealthMonitorParams) {
    tokio::spawn(async move {
        let instance_id_str = p.target_instance_id.as_str().to_owned();
        let request_payload = match NodeHealthRequest::new().encode() {
            Ok(payload) => payload,
            Err(e) => {
                tracing::warn!(
                    "Health monitor for '{}' failed to encode request: {}",
                    instance_id_str,
                    e
                );
                return;
            }
        };

        let mut consecutive_failures: u32 = 0;

        loop {
            tokio::time::sleep(p.interval).await;

            // If the instance was removed externally (e.g. user ran `node stop`),
            // our job is done.
            if p.node_stack
                .find_by_instance_id(&p.target_instance_id)
                .is_none()
            {
                debug!(
                    "Health monitor: instance '{}' no longer in stack, exiting",
                    instance_id_str
                );
                return;
            }

            match ServiceMessenger::poll(
                &p.messenger,
                &p.core_node_name,
                &p.caller_instance_id,
                &p.target_node_name,
                NODE_HEALTH_SERVICE,
                Some(&p.target_core_node),
                Some(&instance_id_str),
                request_payload.clone(),
                p.timeout,
            )
            .await
            {
                Ok(_) => {
                    consecutive_failures = 0;
                }
                Err(err) => {
                    consecutive_failures += 1;
                    debug!(
                        "Health monitor: instance '{}' health check failed ({}/{}): {}",
                        instance_id_str, consecutive_failures, p.max_failures, err
                    );

                    if consecutive_failures >= p.max_failures {
                        tracing::warn!(
                            "Health monitor: instance '{}' failed {} consecutive health checks, removing from stack",
                            instance_id_str,
                            p.max_failures
                        );

                        if let Some(entity_handle) = p
                            .node_stack
                            .find_entity_by_instance_id(&p.target_instance_id)
                        {
                            entity_handle.write().stop_instance(&p.target_instance_id);
                        }

                        super::append_stack_log(
                            &p.peppy_dirs,
                            &format!(
                                "Removed instance '{}' of node '{}:{}': \
                                 failed {} consecutive health checks",
                                instance_id_str, p.target_node_name, p.node_tag, p.max_failures,
                            ),
                        );

                        return;
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_mount_path_parameters_simple() {
        let mount_paths = vec!["${parameters:device_path}:/dev/video0:rw".to_string()];
        let mut arguments = BTreeMap::new();
        arguments.insert(
            "device_path".to_string(),
            AnyType::String("/dev/video0".to_string()),
        );

        let resolved = resolve_mount_path_parameters(&mount_paths, &arguments).unwrap();
        assert_eq!(resolved, vec!["/dev/video0:/dev/video0:rw"]);
    }

    #[test]
    fn test_resolve_mount_path_parameters_nested() {
        let mount_paths = vec!["${parameters:video.device_path}:/dev/video0:rw".to_string()];
        let mut video = BTreeMap::new();
        video.insert(
            "device_path".to_string(),
            AnyType::String("/dev/video1".to_string()),
        );
        let mut arguments = BTreeMap::new();
        arguments.insert("video".to_string(), AnyType::Object(video));

        let resolved = resolve_mount_path_parameters(&mount_paths, &arguments).unwrap();
        assert_eq!(resolved, vec!["/dev/video1:/dev/video0:rw"]);
    }

    #[test]
    fn test_resolve_mount_path_parameters_passthrough() {
        let mount_paths = vec!["/data/models:/opt/models:ro".to_string()];
        let arguments = BTreeMap::new();

        let resolved = resolve_mount_path_parameters(&mount_paths, &arguments).unwrap();
        assert_eq!(resolved, vec!["/data/models:/opt/models:ro"]);
    }

    #[test]
    fn test_resolve_mount_path_parameters_rejects_blocked_path() {
        let mount_paths = vec!["${parameters:path}:/container:rw".to_string()];
        let mut arguments = BTreeMap::new();
        arguments.insert("path".to_string(), AnyType::String("/tmp".to_string()));

        let result = resolve_mount_path_parameters(&mount_paths, &arguments);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("blocked system directory"));
    }
}
