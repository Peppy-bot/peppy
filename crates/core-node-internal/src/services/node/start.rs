use super::super::action_loop::{ActionResult, ActionState, GoalHandler, run_action_loop};
use super::{
    FeedbackLine, FeedbackStream, create_action_log_file, extract_tar_zst, push_stderr_line,
    write_error_to_log,
};
use crate::Result;
use crate::encoding::{NodeStartFeedback, NodeStartGoal, NodeStartGoalResponse, NodeStartResult};
use crate::names;
use chrono::Local;
use config::consts::{PeppyDirs, RUNTIME_CONFIG_VAR_NAME};
use config::node::{Name, PeppygenLanguage};
use config::runtime::RuntimeConfig;
use config::{AnyType, NodeArguments};
use futures::FutureExt;
use node_stack::{NodeEntity, NodeStack};
use peppylib::encoding::health::NodeHealthRequest;
use peppylib::encoding::ready::NodeReadyRequest;
use peppylib::messaging::{
    NODE_HEALTH_SERVICE, NODE_READY_SERVICE, ServiceRequestContext, TopicPublisher,
};
use peppylib::types::Payload;
use peppylib::{ActionMessenger, MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use std::collections::VecDeque;
use std::fs::File;
use std::io::Write;
use std::panic::AssertUnwindSafe;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, Notify, mpsc};
use tokio::task::JoinHandle;
use tracing::debug;

const STARTUP_OUTPUT_MAX_WAIT: Duration = Duration::from_millis(100);
const STARTUP_OUTPUT_QUIET_WINDOW: Duration = Duration::from_millis(10);
const CONTAINER_STARTUP_OUTPUT_MAX_WAIT: Duration = Duration::from_secs(2);
const CONTAINER_STARTUP_OUTPUT_QUIET_WINDOW: Duration = Duration::from_millis(100);
const FEEDBACK_FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

static RUNTIME_CONFIG_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
pub struct NodeStartServiceConfig {
    pub node_startup_timeout: Duration,
    pub node_start_health_timeout: Duration,
    pub peppy_dirs: PeppyDirs,
}

#[derive(Clone)]
struct NodeStartActionContext {
    node_stack: Arc<NodeStack>,
    messenger: MessengerHandle,
    core_node_name: String,
    caller_instance_id: String,
    node_startup_timeout: Duration,
    node_start_health_timeout: Duration,
    peppy_dirs: PeppyDirs,
}

struct ProcessNodeStartContext {
    action: NodeStartActionContext,
    feedback_publisher: TopicPublisher,
    log_file: Arc<StdMutex<File>>,
    sender_instance_id: String,
}

pub async fn listen_for_node_start(
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    config: NodeStartServiceConfig,
) -> Result<JoinHandle<Result<()>>> {
    let action = ActionMessenger::expose(
        messenger,
        core_node_name,
        instance_id,
        node_name,
        names::NODE_START_ACTION,
    )
    .await?;

    let handler = NodeStartGoalHandler {
        context: NodeStartActionContext {
            node_stack,
            messenger: messenger.clone(),
            core_node_name: core_node_name.to_string(),
            caller_instance_id: instance_id.to_string(),
            node_startup_timeout: config.node_startup_timeout,
            node_start_health_timeout: config.node_start_health_timeout,
            peppy_dirs: config.peppy_dirs,
        },
        running_since: Arc::new(Mutex::new(None)),
    };

    let handle = tokio::spawn(async move { run_action_loop(action, handler).await });

    Ok(handle)
}

impl ActionResult for NodeStartResult {
    fn identifier() -> &'static str {
        "node_start_result"
    }

    fn encode_result(&self) -> crate::Result<Payload> {
        self.encode()
    }
}

#[derive(Clone)]
struct NodeStartGoalHandler {
    context: NodeStartActionContext,
    running_since: Arc<Mutex<Option<(Instant, u64)>>>,
}

impl GoalHandler for NodeStartGoalHandler {
    type Result = NodeStartResult;

    async fn handle_goal(
        &self,
        context: ServiceRequestContext,
        feedback_publisher: TopicPublisher,
        state: Arc<Mutex<ActionState<NodeStartResult>>>,
    ) -> PeppyResult<Payload> {
        handle_goal_request(
            context,
            feedback_publisher,
            state,
            self.context.clone(),
            Arc::clone(&self.running_since),
        )
        .await
    }
}

/// Validates that all required parameters from the schema are present in the provided arguments.
/// Returns a list of all missing parameter paths (e.g., ["device.physical", "video.frame_rate"]).
fn validate_parameters(
    schema: &NodeArguments,
    arguments: &NodeArguments,
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
}

impl FeedbackSync {
    fn new() -> Self {
        Self {
            read_count: Arc::new(AtomicU64::new(0)),
            published_count: Arc::new(AtomicU64::new(0)),
            notify: Arc::new(Notify::new()),
            read_notify: Arc::new(Notify::new()),
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

    async fn wait_for_read_quiescence(&self, max_wait: Duration, quiet_window: Duration) {
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
                    // treat that as the end of the initial burst.
                    if saw_read {
                        break;
                    }
                }
            }
        }
    }
}

fn spawn_output_reader<R>(
    reader: R,
    feedback_tx: mpsc::UnboundedSender<FeedbackLine>,
    publish_enabled: Arc<AtomicBool>,
    feedback_sync: FeedbackSync,
    stream: FeedbackStream,
    stderr_buffer: Option<Arc<StdMutex<VecDeque<String>>>>,
    log_file: Arc<StdMutex<File>>,
) -> JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let stream_prefix = match stream {
        FeedbackStream::Stdout => "stdout",
        FeedbackStream::Stderr => "stderr",
    };

    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();

        loop {
            let line = match lines.next_line().await {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(_) => break,
            };

            // Always write to log file, regardless of publish_enabled state
            if let Ok(mut file) = log_file.lock() {
                let timestamp = Local::now().format("%Y-%m-%dT%H:%M:%S%.3f");
                let _ = writeln!(file, "[{}] [{}] {}", timestamp, stream_prefix, line);
            }

            // Always capture stderr for error diagnostics, regardless of publish state
            if matches!(stream, FeedbackStream::Stderr)
                && let Some(buffer) = &stderr_buffer
            {
                push_stderr_line(buffer, &line);
            }

            if !publish_enabled.load(Ordering::Relaxed) {
                continue;
            }

            if feedback_tx.send(FeedbackLine { stream, line }).is_ok() {
                feedback_sync.increment_read();
            }
        }
    })
}

async fn handle_goal_request(
    context: ServiceRequestContext,
    feedback_publisher: TopicPublisher,
    state: Arc<Mutex<ActionState<NodeStartResult>>>,
    action_context: NodeStartActionContext,
    running_since: Arc<Mutex<Option<(Instant, u64)>>>,
) -> PeppyResult<Payload> {
    let sender_instance_id = context.message().instance_id().to_string();
    let payload = context.message().payload();

    let goal = match NodeStartGoal::decode(payload.as_ref()) {
        Ok(goal) => goal,
        Err(e) => {
            let response = NodeStartGoalResponse::rejected(format!("invalid payload: {}", e));
            return response
                .encode()
                .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
                    identifier: "node_start_goal".to_string(),
                    reason: format!("Failed to encode response: {}", e),
                });
        }
    };

    {
        let mut state_guard = state.lock().await;
        if matches!(*state_guard, ActionState::Running) {
            let running_guard = running_since.lock().await;
            let remaining = running_guard
                .map(|(started_at, timeout_secs)| {
                    Duration::from_secs(timeout_secs)
                        .saturating_sub(started_at.elapsed())
                        .as_secs()
                })
                .unwrap_or(0);
            let response = NodeStartGoalResponse::rejected(format!(
                "action already in progress (times out in {remaining}s)"
            ));
            return response
                .encode()
                .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
                    identifier: "node_start_goal".to_string(),
                    reason: format!("Failed to encode response: {}", e),
                });
        }
        *state_guard = ActionState::Running;
        *running_since.lock().await = Some((Instant::now(), goal.timeout_secs));
    }

    // Parse runtime config to get instance_id for log file naming
    let runtime_config: RuntimeConfig = match serde_json5::from_str(&goal.runtime_config_json5) {
        Ok(config) => config,
        Err(e) => {
            let error_msg = format!("Failed to parse PEPPY_RUNTIME_CONFIG: {}", e);
            let mut state_guard = state.lock().await;
            *state_guard = ActionState::Rejected;
            let response = NodeStartGoalResponse::rejected(&error_msg);
            return response
                .encode()
                .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
                    identifier: "node_start_goal".to_string(),
                    reason: format!("Failed to encode response: {}", e),
                });
        }
    };

    let instance_id_str = runtime_config.node_instance.instance_id.as_str();

    debug!(
        "Received `node_start` goal from {sender_instance_id}, node={}:{}, instance_id={}, runtime_config_len={}",
        goal.node_name,
        goal.tag,
        instance_id_str,
        goal.runtime_config_json5.len()
    );

    // Create log file for stdout/stderr
    let log_dir = action_context.peppy_dirs.logs_dir_start();
    let log_filename = format!("{}.log", instance_id_str);
    let (log_file, log_path) = match create_action_log_file(&log_dir, &log_filename) {
        Ok(result) => result,
        Err(error_msg) => {
            debug!("{}", error_msg);
            let mut state_guard = state.lock().await;
            *state_guard = ActionState::Rejected;
            let response = NodeStartGoalResponse::rejected(&error_msg);
            return response
                .encode()
                .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
                    identifier: "node_start_goal".to_string(),
                    reason: format!("Failed to encode response: {}", e),
                });
        }
    };

    debug!("Created log file for node start: {}", log_path.display());

    // Panics are caught via catch_unwind so the state always transitions to
    // Completed — without this, a panic silently aborts the task and leaves
    // the state stuck on Running, causing clients to time out.
    let state_clone = Arc::clone(&state);
    tokio::spawn(async move {
        let log_file_for_panic = log_file.clone();
        let process_context = ProcessNodeStartContext {
            action: action_context,
            feedback_publisher,
            log_file,
            sender_instance_id,
        };
        let result =
            match AssertUnwindSafe(process_node_start(goal, runtime_config, process_context))
                .catch_unwind()
                .await
            {
                Ok(result) => result,
                Err(panic_payload) => {
                    let msg = format!(
                        "node_start task panicked: {}",
                        super::panic_message(&*panic_payload)
                    );
                    tracing::error!("{}", msg);
                    write_error_to_log(&log_file_for_panic, &msg);
                    NodeStartResult::failure(msg)
                }
            };
        let mut state_guard = state_clone.lock().await;
        *state_guard = ActionState::Completed { result };
    });

    let response = NodeStartGoalResponse::accepted(&log_path);
    response
        .encode()
        .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
            identifier: "node_start_goal".to_string(),
            reason: format!("Failed to encode response: {}", e),
        })
}

async fn process_node_start(
    goal: NodeStartGoal,
    runtime_config: RuntimeConfig,
    ctx: ProcessNodeStartContext,
) -> NodeStartResult {
    let sender_instance_id = ctx.sender_instance_id.as_str();
    let NodeStartGoal {
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
            return NodeStartResult::failure(msg);
        }
    };

    let instance_id_str = runtime_config.node_instance.instance_id.as_str();
    let instance_id = match Name::new(instance_id_str) {
        Ok(name) => name,
        Err(e) => {
            let msg = format!("Invalid instance_id: {}", e);
            write_error_to_log(&ctx.log_file, &msg);
            return NodeStartResult::failure(msg);
        }
    };

    debug!(
        "Processing `node_start` from {sender_instance_id}, node={}:{}, instance_id={}",
        node_name, tag, instance_id_str
    );

    let entity = match ctx.action.node_stack.find(&node_name, &tag) {
        Some(entity) => entity,
        None => {
            let msg = format!("Node '{}:{}' not found in node stack", node_name, tag);
            write_error_to_log(&ctx.log_file, &msg);
            return NodeStartResult::failure(msg);
        }
    };

    let sccache_injected =
        super::inject_rust_build_env(&mut env_vars, entity.config().manifest.language);
    if sccache_injected
        && let Ok(payload) =
            NodeStartFeedback::stdout("Using sccache for Rust compilation").encode()
    {
        let _ = ctx.feedback_publisher.publish(payload).await;
    }
    super::inject_node_runtime_env(
        &mut env_vars,
        entity.config().manifest.name.as_str(),
        entity.config().manifest.tag.as_str(),
    );

    // Validate that all required parameters are provided before starting the node
    let missing_params = validate_parameters(
        &entity.config().parameters,
        &runtime_config.node_instance.arguments,
        "",
    );
    if !missing_params.is_empty() {
        let msg = format!("Missing required parameters: {}", missing_params.join(", "));
        write_error_to_log(&ctx.log_file, &msg);
        return NodeStartResult::failure(msg);
    }

    let is_container = entity.config().container.is_some();

    // Prepare instance directory:
    // - Container nodes: create empty dir (SIF image is self-contained)
    // - Process nodes: extract .tar.zst archive into the directory
    let instance_dir = if is_container {
        match create_instance_dir(instance_id_str, &ctx.action.peppy_dirs) {
            Ok(dir) => dir,
            Err(e) => {
                let msg = format!("Failed to create instance directory: {}", e);
                debug!("{}", msg);
                write_error_to_log(&ctx.log_file, &msg);
                return NodeStartResult::failure(msg);
            }
        }
    } else {
        match extract_node_archive(entity.root_path(), instance_id_str, &ctx.action.peppy_dirs) {
            Ok(dir) => dir,
            Err(e) => {
                let msg = format!("Failed to extract node archive: {}", e);
                debug!("{}", msg);
                write_error_to_log(&ctx.log_file, &msg);
                return NodeStartResult::failure(msg);
            }
        }
    };

    // Spawn the node process:
    // - Container nodes: apptainer run <sif>
    // - Process nodes: execute start_cmd
    let mount_paths = entity
        .config()
        .container
        .as_ref()
        .and_then(|c| c.mount_paths.as_deref())
        .unwrap_or_default();

    let mut child = if is_container {
        let mut apptainer = match tokio::task::spawn_blocking(containers::Apptainer::new).await {
            Ok(Ok(a)) => a,
            Ok(Err(e)) => {
                let msg = format!("Failed to initialize Apptainer: {}", e);
                write_error_to_log(&ctx.log_file, &msg);
                return NodeStartResult::failure(msg);
            }
            Err(e) => {
                let msg = format!("Apptainer initialization task failed: {}", e);
                write_error_to_log(&ctx.log_file, &msg);
                return NodeStartResult::failure(msg);
            }
        };

        // Set the correct messaging host for the container environment.
        // On macOS (Lima), 127.0.0.1 inside the VM is the VM's localhost,
        // not the macOS host. Use Lima's host gateway hostname instead.
        let runtime_config_json5 = match apptainer.host_gateway() {
            Some(gateway) => {
                let mut cfg = runtime_config.clone();
                cfg.messaging_host = gateway.to_string();
                match serde_json5::to_string(&cfg) {
                    Ok(json) => json,
                    Err(e) => {
                        let msg = format!("Failed to serialize runtime config: {}", e);
                        write_error_to_log(&ctx.log_file, &msg);
                        return NodeStartResult::failure(msg);
                    }
                }
            }
            None => runtime_config_json5,
        };

        match start_container_node(
            &mut apptainer,
            entity.root_path(),
            &instance_dir,
            &runtime_config_json5,
            &env_vars,
            mount_paths,
            &ctx.log_file,
            &ctx.action.peppy_dirs,
        ) {
            Ok(child) => child,
            Err(e) => {
                let msg = format!("Failed to start container node: {}", e);
                debug!("{}", msg);
                write_error_to_log(&ctx.log_file, &msg);
                return NodeStartResult::failure(msg);
            }
        }
    } else {
        match start_node(
            &entity,
            &instance_dir,
            &runtime_config_json5,
            &env_vars,
            &ctx.log_file,
            &ctx.action.peppy_dirs,
        ) {
            Ok(child) => child,
            Err(e) => {
                let msg = format!("Failed to start node: {}", e);
                debug!("Failed to start node instance '{}': {}", instance_id_str, e);
                write_error_to_log(&ctx.log_file, &msg);
                return NodeStartResult::failure(msg);
            }
        }
    };

    let publish_enabled = Arc::new(AtomicBool::new(true));
    let feedback_sync = FeedbackSync::new();
    let stderr_buffer = Arc::new(StdMutex::new(VecDeque::new()));

    let (feedback_tx, mut feedback_rx) = mpsc::unbounded_channel::<FeedbackLine>();
    let feedback_publisher = ctx.feedback_publisher.clone();
    let feedback_sync_publisher = feedback_sync.clone();
    tokio::spawn(async move {
        while let Some(line) = feedback_rx.recv().await {
            let feedback = match line.stream {
                FeedbackStream::Stdout => NodeStartFeedback::stdout(&line.line),
                FeedbackStream::Stderr => NodeStartFeedback::stderr(&line.line),
            };
            if let Ok(payload) = feedback.encode() {
                let _ = feedback_publisher.publish(payload).await;
            }
            feedback_sync_publisher.increment_published();
        }
    });

    let mut output_reader_handles = Vec::new();

    if let Some(stdout) = child.stdout.take() {
        output_reader_handles.push(spawn_output_reader(
            stdout,
            feedback_tx.clone(),
            Arc::clone(&publish_enabled),
            feedback_sync.clone(),
            FeedbackStream::Stdout,
            None,
            Arc::clone(&ctx.log_file),
        ));
    }

    if let Some(stderr) = child.stderr.take() {
        output_reader_handles.push(spawn_output_reader(
            stderr,
            feedback_tx,
            Arc::clone(&publish_enabled),
            feedback_sync.clone(),
            FeedbackStream::Stderr,
            Some(Arc::clone(&stderr_buffer)),
            Arc::clone(&ctx.log_file),
        ));
    }

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

    let ready_result =
        wait_for_ready_signal(&signal_target, ctx.action.node_startup_timeout, &mut child).await;

    if let Err(e) = ready_result {
        debug!(
            "Ready signal failed for node instance '{}': {}, killing process",
            instance_id_str, e
        );
        let result = kill_and_report_error(
            child,
            instance_id_str,
            &e,
            stderr_buffer,
            output_reader_handles,
            Arc::clone(&ctx.log_file),
        )
        .await;
        feedback_sync.flush_or_warn(instance_id_str).await;
        publish_enabled.store(false, Ordering::Relaxed);
        return result;
    }

    debug!(
        "Node instance '{}' is ready, performing health check...",
        instance_id_str
    );

    let health_result = perform_health_check(
        &signal_target,
        ctx.action.node_start_health_timeout,
        &mut child,
    )
    .await;

    match health_result {
        Ok(()) => {
            debug!(
                "Health check passed for node instance '{}'",
                instance_id_str
            );
            let pid = child.id().unwrap_or(0);
            if let Err(e) =
                ctx.action
                    .node_stack
                    .add_instance(&node_name, &tag, Some(&instance_id), Some(pid))
            {
                if let Err(kill_err) = child.kill().await {
                    debug!(
                        "Failed to kill process for node instance '{}': {}",
                        instance_id_str, kill_err
                    );
                }
                let msg = format!("Failed to register instance: {}", e);
                write_error_to_log(&ctx.log_file, &msg);
                let result = NodeStartResult::failure(msg);
                feedback_sync.flush_or_warn(instance_id_str).await;
                publish_enabled.store(false, Ordering::Relaxed);
                return result;
            }
            let (max_wait, quiet_window) = if is_container {
                (
                    CONTAINER_STARTUP_OUTPUT_MAX_WAIT,
                    CONTAINER_STARTUP_OUTPUT_QUIET_WINDOW,
                )
            } else {
                (STARTUP_OUTPUT_MAX_WAIT, STARTUP_OUTPUT_QUIET_WINDOW)
            };
            feedback_sync
                .wait_for_read_quiescence(max_wait, quiet_window)
                .await;
            let result = NodeStartResult::success(pid);
            feedback_sync.flush_or_warn(instance_id_str).await;
            publish_enabled.store(false, Ordering::Relaxed);
            result
        }
        Err(e) => {
            debug!(
                "Health check failed for node instance '{}': {}, killing process",
                instance_id_str, e
            );
            let result = kill_and_report_error(
                child,
                instance_id_str,
                &e,
                stderr_buffer,
                output_reader_handles,
                Arc::clone(&ctx.log_file),
            )
            .await;
            feedback_sync.flush_or_warn(instance_id_str).await;
            publish_enabled.store(false, Ordering::Relaxed);
            result
        }
    }
}

/// Helper function to kill a child process and report an error with stderr tail capture.
async fn kill_and_report_error(
    mut child: Child,
    instance_id_str: &str,
    error: &str,
    stderr_buffer: Arc<StdMutex<VecDeque<String>>>,
    output_reader_handles: Vec<JoinHandle<()>>,
    log_file: Arc<StdMutex<File>>,
) -> NodeStartResult {
    if let Err(kill_err) = child.kill().await {
        debug!(
            "Failed to kill process for node instance '{}': {}",
            instance_id_str, kill_err
        );
    }

    let _ = child.wait().await;

    // Drain any remaining output that was already in-flight so error reporting is stable.
    // We intentionally ignore join errors so we don't mask the actual node start failure.
    for handle in output_reader_handles {
        let _ = handle.await;
    }

    let stderr_output = {
        let guard = stderr_buffer.lock().expect("stderr buffer lock poisoned");
        let buffer_lines: Vec<String> = guard.iter().cloned().collect();
        if !buffer_lines.is_empty() {
            buffer_lines.join("\n")
        } else {
            // Fall back to the log file for stderr lines — the log write is unconditional
            // and may have captured output that the stderr_buffer missed due to timing
            // (e.g. the async reader hadn't processed the line before we read the buffer).
            extract_stderr_from_log(&log_file)
        }
    };

    if !stderr_output.is_empty() {
        debug!(
            "Node instance '{}' stderr (tail): {}",
            instance_id_str, stderr_output
        );
    }

    let error_msg = if stderr_output.is_empty() {
        error.to_string()
    } else {
        format!(
            "{}\n\n--- stderr (last lines) ---\n{}",
            error, stderr_output
        )
    };

    NodeStartResult::failure(error_msg)
}

/// Extracts stderr lines from the log file.
///
/// The log file captures all output unconditionally (before any async processing),
/// so it serves as a reliable fallback when the stderr_buffer is empty due to
/// async scheduling timing (e.g., the reader task hadn't processed the line before
/// the buffer was read).
fn extract_stderr_from_log(log_file: &Arc<StdMutex<File>>) -> String {
    use std::io::{Read, Seek};

    let content = match log_file.lock() {
        Ok(mut f) => {
            if f.seek(std::io::SeekFrom::Start(0)).is_err() {
                return String::new();
            }
            let mut buf = String::new();
            if f.read_to_string(&mut buf).is_err() {
                return String::new();
            }
            buf
        }
        Err(_) => return String::new(),
    };

    content
        .lines()
        .filter(|l| l.contains("[stderr]"))
        .filter_map(|l| l.split_once("[stderr] ").map(|(_, rest)| rest))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Creates (or recreates) a clean instance directory under the instances dir.
/// Returns the path to the newly created directory.
fn create_instance_dir(
    instance_id: &str,
    peppy_dirs: &PeppyDirs,
) -> std::result::Result<std::path::PathBuf, String> {
    let instances_dir = peppy_dirs.instances_dir();
    std::fs::create_dir_all(&instances_dir).map_err(|e| {
        format!(
            "Failed to create instances directory {}: {}",
            instances_dir.display(),
            e
        )
    })?;

    let instance_dir = instances_dir.join(instance_id);

    // Clean up any leftover instance directory from a previous failed attempt,
    // since the instance ID is deterministic and may be retried.
    if instance_dir.exists() {
        std::fs::remove_dir_all(&instance_dir).map_err(|e| {
            format!(
                "Failed to clean up existing instance directory {}: {}",
                instance_dir.display(),
                e
            )
        })?;
    }

    std::fs::create_dir(&instance_dir).map_err(|e| {
        format!(
            "Failed to create instance directory {}: {}",
            instance_dir.display(),
            e
        )
    })?;

    Ok(instance_dir)
}

/// Extracts a `.tar.zst` node archive to a new instance directory.
/// Returns the path to the extracted instance directory.
fn extract_node_archive(
    archive_path: &std::path::Path,
    instance_id: &str,
    peppy_dirs: &PeppyDirs,
) -> std::result::Result<std::path::PathBuf, String> {
    let instance_dir = create_instance_dir(instance_id, peppy_dirs)?;
    extract_tar_zst(archive_path, &instance_dir)?;
    Ok(instance_dir)
}

/// Runs a node using its build's start_cmd and passes the PEPPY_RUNTIME_CONFIG as an env var.
/// Returns the spawned child process handle on success.
pub fn start_node(
    entity: &NodeEntity,
    working_dir: &std::path::Path,
    runtime_config_json5: &str,
    env_vars: &[(String, String)],
    log_file: &Arc<StdMutex<File>>,
    peppy_dirs: &PeppyDirs,
) -> std::io::Result<Child> {
    let config = entity.config();
    let manifest = &config.manifest;
    let build = config.process.as_ref().ok_or_else(|| {
        std::io::Error::other(
            "node has no process config (container nodes cannot be started this way)",
        )
    })?;

    let Some((program, args)) = build.start_cmd.split_first() else {
        return Err(std::io::Error::other("start_cmd is empty"));
    };

    debug!(
        "Running node '{}:{}' with command: {} {:?} in dir {:?}",
        manifest.name.as_str(),
        manifest.tag,
        program,
        args,
        working_dir
    );

    // Log the command being executed to the log file before attempting to spawn
    {
        let full_cmd = build.start_cmd.join(" ");
        if let Ok(mut file) = log_file.lock() {
            let timestamp = Local::now().format("%Y-%m-%dT%H:%M:%S%.3f");
            let _ = writeln!(
                file,
                "[{}] Executing start_cmd: {} (working_dir: {})",
                timestamp,
                full_cmd,
                working_dir.display()
            );
            let _ = file.flush();
        }
    }

    // Write runtime config to a unique file per spawned process.
    // Using a shared path can cause cross-test and cross-instance races where a node reads the
    // wrong config (instance_id/port), leading to hangs waiting for ready/health responses.
    let runtime_dir = peppy_dirs.runtime_config_dir();
    std::fs::create_dir_all(&runtime_dir)?;
    let counter = RUNTIME_CONFIG_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let runtime_config_path = runtime_dir.join(format!("runtime_config_{pid}_{counter}.json5"));
    std::fs::write(&runtime_config_path, runtime_config_json5)?;

    let mut command = Command::new(program);
    command.current_dir(working_dir);
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env_vars {
        command.env(key, value);
    }
    // Set PWD to match the actual working directory so tools that read this
    // variable (e.g. capnproto's KJ) see a consistent value. The caller's
    // PWD is stripped by caller_env_overrides() since it refers to the
    // caller's directory, not the node's instance dir.
    command.env("PWD", working_dir);
    command.env(RUNTIME_CONFIG_VAR_NAME, &runtime_config_path);

    // Force unbuffered stdout/stderr for Python nodes. Without this, Python
    // defaults to full buffering when stdout is a pipe, delaying log capture.
    if manifest.language == PeppygenLanguage::Python {
        command.env("PYTHONUNBUFFERED", "1");
    }

    command.spawn()
}

/// Describes a bind mount for a container node.
struct ContainerBind {
    src: String,
    dest: Option<String>,
    opts: Option<String>,
}

/// Collect all bind mounts needed for a container node.
///
/// Always includes the runtime config file as the first entry so it is
/// accessible inside the container regardless of Apptainer's `$HOME` auto-bind
/// behavior (which may not cover `~/.peppy/` when running inside a Lima VM).
fn collect_container_binds(
    runtime_config_path: &std::path::Path,
    mount_paths: &[String],
) -> Vec<ContainerBind> {
    let mut binds = Vec::with_capacity(1 + mount_paths.len());

    // Runtime config must always be bound into the container.
    binds.push(ContainerBind {
        src: runtime_config_path.to_string_lossy().into_owned(),
        dest: None,
        opts: None,
    });

    // User-specified mount paths (format: "host:container[:opts]")
    for m in mount_paths {
        let parts: Vec<&str> = m.splitn(3, ':').collect();
        binds.push(match parts.len() {
            1 => ContainerBind {
                src: parts[0].into(),
                dest: None,
                opts: None,
            },
            2 => ContainerBind {
                src: parts[0].into(),
                dest: Some(parts[1].into()),
                opts: None,
            },
            _ => ContainerBind {
                src: parts[0].into(),
                dest: Some(parts[1].into()),
                opts: Some(parts[2].into()),
            },
        });
    }

    binds
}

/// Starts a container node using the Apptainer runtime.
///
/// Builds an `apptainer run <sif_path>` command with environment variables
/// passed into the container via `--env` flags and optional bind mounts from
/// `mount_paths`. Returns a tokio [`Child`] with piped stdout/stderr for
/// async output capture.
#[allow(clippy::too_many_arguments)]
fn start_container_node(
    apptainer: &mut containers::Apptainer,
    sif_path: &std::path::Path,
    working_dir: &std::path::Path,
    runtime_config_json5: &str,
    env_vars: &[(String, String)],
    mount_paths: &[String],
    log_file: &Arc<StdMutex<File>>,
    peppy_dirs: &PeppyDirs,
) -> std::io::Result<Child> {
    // Write runtime config to a unique file (same pattern as start_node).
    let runtime_dir = peppy_dirs.runtime_config_dir();
    std::fs::create_dir_all(&runtime_dir)?;
    let counter = RUNTIME_CONFIG_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let runtime_config_path = runtime_dir.join(format!("runtime_config_{pid}_{counter}.json5"));
    std::fs::write(&runtime_config_path, runtime_config_json5)?;

    let sif_str = sif_path
        .to_str()
        .ok_or_else(|| std::io::Error::other("SIF path is not valid UTF-8"))?;

    // Collect all bind mounts (runtime config + user-specified mount_paths).
    let binds = collect_container_binds(&runtime_config_path, mount_paths);

    // Ensure host-side source directories exist for user-specified bind mounts.
    // Skip binds[0] (runtime config file) — its parent dir is already created above.
    for bind in &binds[1..] {
        let src = std::path::Path::new(&bind.src);
        if !src.exists() {
            std::fs::create_dir_all(src)?;
        }
    }

    // Ensure host paths outside $HOME are accessible in the Lima VM.
    // Skip binds[0] (runtime config) — it's always under $HOME.
    if binds.len() > 1 {
        let src_paths: Vec<&str> = binds[1..].iter().map(|b| b.src.as_str()).collect();
        apptainer
            .ensure_host_mounts(&src_paths)
            .map_err(|e| std::io::Error::other(format!("Failed to ensure host mounts: {}", e)))?;
    }

    // Build apptainer run command. Environment variables are passed into the
    // container via --env flags (not host-side process env) so they are
    // visible inside the container.
    let mut apptainer_cmd = apptainer.run(sif_str);
    for (key, value) in env_vars {
        // Apptainer manages HOME itself; passing it via --env triggers a warning.
        if key.eq_ignore_ascii_case("HOME") {
            continue;
        }
        apptainer_cmd = apptainer_cmd.env(key, value);
    }
    apptainer_cmd = apptainer_cmd.env(
        RUNTIME_CONFIG_VAR_NAME,
        runtime_config_path.to_str().unwrap_or_default(),
    );

    // Add all bind mounts (runtime config + user-specified).
    for bind in &binds {
        apptainer_cmd = apptainer_cmd.bind(&bind.src, bind.dest.as_deref(), bind.opts.as_deref());
    }

    // Log the command being executed
    {
        if let Ok(mut file) = log_file.lock() {
            let timestamp = Local::now().format("%Y-%m-%dT%H:%M:%S%.3f");
            let _ = writeln!(
                file,
                "[{}] Executing apptainer run: {} (working_dir: {}, bind_mounts: [{}])",
                timestamp,
                sif_path.display(),
                working_dir.display(),
                mount_paths.join(", ")
            );
            let _ = file.flush();
        }
    }

    // Get the fully-built std::process::Command from the Apptainer facade,
    // then convert to tokio::process::Command for async stdio piping.
    let std_cmd = apptainer_cmd
        .into_std_command()
        .map_err(|e| std::io::Error::other(format!("Failed to build apptainer command: {}", e)))?;

    let mut command = Command::from(std_cmd);
    command
        .current_dir(working_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    command.spawn()
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_collect_container_binds_always_includes_runtime_config() {
        let rc = PathBuf::from("/home/user/.peppy/runtime/runtime_config_99_0.json5");
        let binds = collect_container_binds(&rc, &[]);

        assert_eq!(binds.len(), 1);
        assert_eq!(
            binds[0].src,
            "/home/user/.peppy/runtime/runtime_config_99_0.json5"
        );
        assert!(binds[0].dest.is_none());
        assert!(binds[0].opts.is_none());
    }

    #[test]
    fn test_collect_container_binds_includes_user_mounts() {
        let rc = PathBuf::from("/home/user/.peppy/runtime/rc.json5");
        let user_mounts = vec![
            "/data/input:/container/input:ro".to_string(),
            "/dev/ttyUSB0".to_string(),
        ];

        let binds = collect_container_binds(&rc, &user_mounts);

        assert_eq!(binds.len(), 3);
        // First entry is always the runtime config
        assert_eq!(binds[0].src, "/home/user/.peppy/runtime/rc.json5");
        assert!(binds[0].dest.is_none());
        assert!(binds[0].opts.is_none());
        // User mounts follow
        assert_eq!(binds[1].src, "/data/input");
        assert_eq!(binds[1].dest.as_deref(), Some("/container/input"));
        assert_eq!(binds[1].opts.as_deref(), Some("ro"));
        assert_eq!(binds[2].src, "/dev/ttyUSB0");
        assert!(binds[2].dest.is_none());
        assert!(binds[2].opts.is_none());
    }
}
