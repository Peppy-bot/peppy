use crate::Result;
use crate::encoding::{NodeStartFeedback, NodeStartGoal, NodeStartGoalResponse, NodeStartResult};
use crate::names;
use chrono::Local;
use config::consts::{RUNTIME_CONFIG_VAR_NAME, logs_dir_start, runtime_config_dir};
use config::node::{Name, PeppygenLanguage};
use config::runtime::RuntimeConfig;
use config::{AnyType, NodeArguments};
use node_stack::{NodeEntity, NodeStack};
use peppylib::encoding::health::NodeHealthRequest;
use peppylib::encoding::ready::NodeReadyRequest;
use peppylib::messaging::{
    ActionCreation, NODE_HEALTH_SERVICE, NODE_READY_SERVICE, ServiceRequestContext, TopicPublisher,
};
use peppylib::types::Payload;
use peppylib::{ActionMessenger, MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use std::collections::VecDeque;
use std::fs::File;
use std::io::Write;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, Notify, mpsc};
use tokio::task::JoinHandle;
use tracing::debug;

const STDERR_BUFFER_LINES: usize = 20;
const STARTUP_OUTPUT_MAX_WAIT: Duration = Duration::from_millis(100);
const STARTUP_OUTPUT_QUIET_WINDOW: Duration = Duration::from_millis(10);

static RUNTIME_CONFIG_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

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

/// State for tracking the current node start action.
#[derive(Default)]
enum NodeStartActionState {
    /// No action is currently running.
    #[default]
    Idle,
    /// The goal was rejected (invalid payload, missing parameters, etc.)
    /// and no actual work was started. The server should reset to Idle
    /// and be ready to accept the next goal.
    Rejected,
    /// An action is currently running.
    Running {
        started_at: Instant,
        timeout_secs: u64,
    },
    /// The action completed and the result is ready to be sent.
    Completed { result: NodeStartResult },
    /// The result has been sent to the requester.
    ResultSent { result: NodeStartResult },
}

#[derive(Clone, Copy)]
enum FeedbackStream {
    Stdout,
    Stderr,
}

struct FeedbackLine {
    stream: FeedbackStream,
    line: String,
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

    async fn flush(&self) {
        let target = self.read_count.load(Ordering::Relaxed);
        loop {
            if self.published_count.load(Ordering::Relaxed) >= target {
                break;
            }
            let notified = self.notify.notified();
            if self.published_count.load(Ordering::Relaxed) >= target {
                break;
            }
            notified.await;
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

fn push_stderr_line(buffer: &Arc<StdMutex<VecDeque<String>>>, line: &str) {
    let mut guard = buffer.lock().expect("stderr buffer lock poisoned");
    if guard.len() == STDERR_BUFFER_LINES {
        guard.pop_front();
    }
    guard.push_back(line.to_string());
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

            if !publish_enabled.load(Ordering::Relaxed) {
                continue;
            }

            if matches!(stream, FeedbackStream::Stderr)
                && let Some(buffer) = &stderr_buffer
            {
                push_stderr_line(buffer, &line);
            }

            if feedback_tx.send(FeedbackLine { stream, line }).is_ok() {
                feedback_sync.increment_read();
            }
        }
    })
}

pub async fn listen_for_node_start(
    messenger: &MessengerHandle,
    daemon_node_node: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    node_startup_timeout: Duration,
    node_start_health_timeout: Duration,
) -> Result<JoinHandle<Result<()>>> {
    let action = ActionMessenger::expose(
        messenger,
        daemon_node_node,
        instance_id,
        node_name,
        names::NODE_START_ACTION,
    )
    .await?;

    let messenger = messenger.clone();
    let daemon_node_name = daemon_node_node.to_string();
    let caller_instance_id = instance_id.to_string();

    let handle = tokio::spawn(async move {
        run_node_start_action_loop(
            action,
            node_stack,
            messenger,
            daemon_node_name,
            caller_instance_id,
            node_startup_timeout,
            node_start_health_timeout,
        )
        .await
    });

    Ok(handle)
}

async fn run_node_start_action_loop(
    mut action: ActionCreation,
    node_stack: Arc<NodeStack>,
    messenger: MessengerHandle,
    daemon_node_name: String,
    caller_instance_id: String,
    node_startup_timeout: Duration,
    node_start_health_timeout: Duration,
) -> Result<()> {
    let state = Arc::new(Mutex::new(NodeStartActionState::default()));

    loop {
        let goal_result = action
            .goal_service
            .handle_next_request({
                let feedback_publisher = &action.feedback_publisher;
                let node_stack = Arc::clone(&node_stack);
                let messenger = messenger.clone();
                let daemon_node_name = daemon_node_name.clone();
                let caller_instance_id = caller_instance_id.clone();
                let state = Arc::clone(&state);
                move |context| {
                    let feedback_publisher = feedback_publisher.clone();
                    let node_stack = Arc::clone(&node_stack);
                    let messenger = messenger.clone();
                    let daemon_node_name = daemon_node_name.clone();
                    let caller_instance_id = caller_instance_id.clone();
                    let state = Arc::clone(&state);
                    async move {
                        handle_goal_request(
                            context,
                            feedback_publisher,
                            node_stack,
                            messenger,
                            daemon_node_name,
                            caller_instance_id,
                            node_startup_timeout,
                            node_start_health_timeout,
                            state,
                        )
                        .await
                    }
                }
            })
            .await;

        match goal_result {
            Ok(true) => {
                // If the goal was rejected (invalid payload, etc.), reset to Idle
                // and continue waiting for the next goal without entering the inner loop.
                {
                    let mut state_guard = state.lock().await;
                    if matches!(*state_guard, NodeStartActionState::Rejected) {
                        *state_guard = NodeStartActionState::Idle;
                        continue;
                    }
                }

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
                                    let mut state_guard = state.lock().await;
                                    if matches!(*state_guard, NodeStartActionState::ResultSent { .. }) {
                                        *state_guard = NodeStartActionState::default();
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
                        // Listen for new goals in case the client abandoned the current action
                        // (e.g., accepted but never polled for result). This prevents one
                        // abandoned action from blocking all subsequent requests.
                        goal_result = action.goal_service.handle_next_request({
                            let feedback_publisher = &action.feedback_publisher;
                            let node_stack = Arc::clone(&node_stack);
                            let messenger = messenger.clone();
                            let daemon_node_name = daemon_node_name.clone();
                            let caller_instance_id = caller_instance_id.clone();
                            let state = Arc::clone(&state);
                            move |context| {
                                let feedback_publisher = feedback_publisher.clone();
                                let node_stack = Arc::clone(&node_stack);
                                let messenger = messenger.clone();
                                let daemon_node_name = daemon_node_name.clone();
                                let caller_instance_id = caller_instance_id.clone();
                                let state = Arc::clone(&state);
                                async move {
                                    handle_goal_request(
                                        context,
                                        feedback_publisher,
                                        node_stack,
                                        messenger,
                                        daemon_node_name,
                                        caller_instance_id,
                                        node_startup_timeout,
                                        node_start_health_timeout,
                                        state,
                                    )
                                    .await
                                }
                            }
                        }) => {
                            match goal_result {
                                Ok(true) => {
                                    // A new goal was received. If it was rejected, reset to Idle.
                                    let mut state_guard = state.lock().await;
                                    if matches!(*state_guard, NodeStartActionState::Rejected) {
                                        *state_guard = NodeStartActionState::Idle;
                                    }
                                    // Continue in inner loop - the new goal will be processed
                                    // and its result will be available for polling.
                                }
                                Ok(false) => return Ok(()),
                                Err(e) => {
                                    debug!("Goal service error in inner loop: {}", e);
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

#[allow(clippy::too_many_arguments)]
async fn handle_goal_request(
    context: ServiceRequestContext,
    feedback_publisher: TopicPublisher,
    node_stack: Arc<NodeStack>,
    messenger: MessengerHandle,
    daemon_node_name: String,
    caller_instance_id: String,
    node_startup_timeout: Duration,
    node_start_health_timeout: Duration,
    state: Arc<Mutex<NodeStartActionState>>,
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
        if let NodeStartActionState::Running {
            started_at,
            timeout_secs,
        } = *state_guard
        {
            let remaining = Duration::from_secs(timeout_secs)
                .saturating_sub(started_at.elapsed())
                .as_secs();
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
        *state_guard = NodeStartActionState::Running {
            started_at: Instant::now(),
            timeout_secs: goal.timeout_secs,
        };
    }

    // Parse runtime config to get instance_id for log file naming
    let runtime_config: RuntimeConfig = match serde_json5::from_str(&goal.runtime_config_json5) {
        Ok(config) => config,
        Err(e) => {
            let error_msg = format!("Failed to parse PEPPY_RUNTIME_CONFIG: {}", e);
            let mut state_guard = state.lock().await;
            *state_guard = NodeStartActionState::Rejected;
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
    let log_dir = logs_dir_start();
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        let error_msg = format!("Failed to create logs directory: {}", e);
        debug!("Failed to create logs directory {:?}: {}", log_dir, e);
        let mut state_guard = state.lock().await;
        *state_guard = NodeStartActionState::Rejected;
        let response = NodeStartGoalResponse::rejected(&error_msg);
        return response
            .encode()
            .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
                identifier: "node_start_goal".to_string(),
                reason: format!("Failed to encode response: {}", e),
            });
    }

    let log_path = log_dir.join(format!("{}.log", instance_id_str));
    let log_file = match File::create(&log_path) {
        Ok(file) => Arc::new(StdMutex::new(file)),
        Err(e) => {
            let error_msg = format!("Failed to create log file: {}", e);
            debug!("Failed to create log file {:?}: {}", log_path, e);
            let mut state_guard = state.lock().await;
            *state_guard = NodeStartActionState::Rejected;
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

    let state_clone = Arc::clone(&state);
    tokio::spawn(async move {
        let result = process_node_start(
            goal,
            runtime_config,
            sender_instance_id,
            node_stack,
            messenger,
            daemon_node_name,
            caller_instance_id,
            node_startup_timeout,
            node_start_health_timeout,
            feedback_publisher,
            log_file,
        )
        .await;
        let mut state_guard = state_clone.lock().await;
        *state_guard = NodeStartActionState::Completed { result };
    });

    let response = NodeStartGoalResponse::accepted(&log_path);
    response
        .encode()
        .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
            identifier: "node_start_goal".to_string(),
            reason: format!("Failed to encode response: {}", e),
        })
}

#[allow(clippy::too_many_arguments)]
async fn process_node_start(
    goal: NodeStartGoal,
    runtime_config: RuntimeConfig,
    sender_instance_id: String,
    node_stack: Arc<NodeStack>,
    messenger: MessengerHandle,
    daemon_node_name: String,
    caller_instance_id: String,
    node_startup_timeout: Duration,
    node_start_health_timeout: Duration,
    feedback_publisher: TopicPublisher,
    log_file: Arc<StdMutex<File>>,
) -> NodeStartResult {
    let NodeStartGoal {
        runtime_config_json5,
        node_name,
        tag,
        env_vars,
        ..
    } = goal;
    let env_vars = match super::validate_goal_env_vars(&env_vars) {
        Ok(vars) => vars,
        Err(e) => {
            return NodeStartResult::failure(e.to_string());
        }
    };

    let instance_id_str = runtime_config.node_instance.instance_id.as_str();
    let instance_id = match Name::new(instance_id_str) {
        Ok(name) => name,
        Err(e) => {
            return NodeStartResult::failure(format!("Invalid instance_id: {}", e));
        }
    };

    debug!(
        "Processing `node_start` from {sender_instance_id}, node={}:{}, instance_id={}",
        node_name, tag, instance_id_str
    );

    let entity = match node_stack.find(&node_name, &tag) {
        Some(entity) => entity,
        None => {
            return NodeStartResult::failure(format!(
                "Node '{}:{}' not found in node stack",
                node_name, tag
            ));
        }
    };

    let mut env_vars = env_vars;
    let sccache_injected = super::inject_rust_build_env(
        &mut env_vars,
        entity.config().manifest.language,
        &node_name,
        &tag,
    );
    if sccache_injected {
        if let Ok(payload) =
            NodeStartFeedback::stdout("Using sccache for Rust compilation").encode()
        {
            let _ = feedback_publisher.publish(payload).await;
        }
    }

    // Validate that all required parameters are provided before starting the node
    let missing_params = validate_parameters(
        &entity.config().parameters,
        &runtime_config.node_instance.arguments,
        "",
    );
    if !missing_params.is_empty() {
        return NodeStartResult::failure(format!(
            "Missing required parameters: {}",
            missing_params.join(", ")
        ));
    }

    let mut child = match start_node(&entity, &runtime_config_json5, &env_vars, &log_file) {
        Ok(child) => child,
        Err(e) => {
            debug!("Failed to start node instance '{}': {}", instance_id_str, e);
            return NodeStartResult::failure(format!("Failed to start node: {}", e));
        }
    };

    let publish_enabled = Arc::new(AtomicBool::new(true));
    let feedback_sync = FeedbackSync::new();
    let stderr_buffer = Arc::new(StdMutex::new(VecDeque::new()));

    let (feedback_tx, mut feedback_rx) = mpsc::unbounded_channel::<FeedbackLine>();
    let feedback_publisher = feedback_publisher.clone();
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
            Arc::clone(&log_file),
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
            Arc::clone(&log_file),
        ));
    }

    debug!(
        "Successfully spawned node instance '{}', waiting for ready signal for {}s...",
        instance_id_str,
        node_startup_timeout.as_secs()
    );

    let ready_result = wait_for_ready_signal(
        &messenger,
        &daemon_node_name,
        &caller_instance_id,
        runtime_config.node_name.as_str(),
        runtime_config.bound_daemon_node.as_str(),
        instance_id_str,
        node_startup_timeout,
        &mut child,
    )
    .await;

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
        )
        .await;
        feedback_sync.flush().await;
        publish_enabled.store(false, Ordering::Relaxed);
        return result;
    }

    debug!(
        "Node instance '{}' is ready, performing health check...",
        instance_id_str
    );

    let health_result = perform_health_check(
        &messenger,
        &daemon_node_name,
        &caller_instance_id,
        runtime_config.node_name.as_str(),
        runtime_config.bound_daemon_node.as_str(),
        instance_id_str,
        node_start_health_timeout,
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
            if let Err(e) = node_stack.add_instance(&node_name, &tag, Some(&instance_id), Some(pid))
            {
                if let Err(kill_err) = child.kill().await {
                    debug!(
                        "Failed to kill process for node instance '{}': {}",
                        instance_id_str, kill_err
                    );
                }
                let result =
                    NodeStartResult::failure(format!("Failed to register instance: {}", e));
                feedback_sync.flush().await;
                publish_enabled.store(false, Ordering::Relaxed);
                return result;
            }
            feedback_sync
                .wait_for_read_quiescence(STARTUP_OUTPUT_MAX_WAIT, STARTUP_OUTPUT_QUIET_WINDOW)
                .await;
            let result = NodeStartResult::success(pid);
            feedback_sync.flush().await;
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
            )
            .await;
            feedback_sync.flush().await;
            publish_enabled.store(false, Ordering::Relaxed);
            result
        }
    }
}

async fn handle_cancel_request(
    _context: ServiceRequestContext,
    state: Arc<Mutex<NodeStartActionState>>,
) -> PeppyResult<Payload> {
    let state_guard = state.lock().await;
    if matches!(*state_guard, NodeStartActionState::Running { .. }) {
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
    state: Arc<Mutex<NodeStartActionState>>,
) -> PeppyResult<Payload> {
    let mut state_guard = state.lock().await;

    match std::mem::replace(&mut *state_guard, NodeStartActionState::Idle) {
        NodeStartActionState::Running {
            started_at,
            timeout_secs,
        } => {
            *state_guard = NodeStartActionState::Running {
                started_at,
                timeout_secs,
            };
            Ok(Payload::from_static(
                b"result pending: operation still in progress",
            ))
        }
        NodeStartActionState::Completed { result } => {
            let payload =
                result
                    .encode()
                    .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
                        identifier: "node_start_result".to_string(),
                        reason: format!("Failed to encode result: {}", e),
                    })?;
            *state_guard = NodeStartActionState::ResultSent { result };
            Ok(payload)
        }
        NodeStartActionState::ResultSent { result } => {
            let payload =
                result
                    .encode()
                    .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
                        identifier: "node_start_result".to_string(),
                        reason: format!("Failed to encode result: {}", e),
                    })?;
            *state_guard = NodeStartActionState::ResultSent { result };
            Ok(payload)
        }
        NodeStartActionState::Idle | NodeStartActionState::Rejected => {
            // Rejected state is normally reset to Idle before result polling,
            // but handle it the same way for robustness.
            Ok(Payload::from_static(b"result pending: no result available"))
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
        guard.iter().cloned().collect::<Vec<_>>().join("\n")
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
        format!("{}. Node stderr: {}", error, stderr_output)
    };

    NodeStartResult::failure(error_msg)
}

/// Runs a node using its manifest's start_cmd and passes the PEPPY_RUNTIME_CONFIG as an env var.
/// Returns the spawned child process handle on success.
pub fn start_node(
    entity: &NodeEntity,
    runtime_config_json5: &str,
    env_vars: &[(String, String)],
    log_file: &Arc<StdMutex<File>>,
) -> std::io::Result<Child> {
    let manifest = &entity.config().manifest;

    let Some((program, args)) = manifest.start_cmd.split_first() else {
        return Err(std::io::Error::other("start_cmd is empty"));
    };

    debug!(
        "Running node '{}:{}' with command: {} {:?} in dir {:?}",
        manifest.name.as_str(),
        manifest.tag,
        program,
        args,
        entity.root_path()
    );

    // Log the command being executed to the log file before attempting to spawn
    {
        let full_cmd = manifest.start_cmd.join(" ");
        if let Ok(mut file) = log_file.lock() {
            let timestamp = Local::now().format("%Y-%m-%dT%H:%M:%S%.3f");
            let _ = writeln!(
                file,
                "[{}] Executing start_cmd: {} (working_dir: {})",
                timestamp,
                full_cmd,
                entity.root_path().display()
            );
            let _ = file.flush();
        }
    }

    // Write runtime config to a unique file per spawned process.
    // Using a shared path can cause cross-test and cross-instance races where a node reads the
    // wrong config (instance_id/port), leading to hangs waiting for ready/health responses.
    let runtime_dir = runtime_config_dir();
    std::fs::create_dir_all(&runtime_dir)?;
    let counter = RUNTIME_CONFIG_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let runtime_config_path = runtime_dir.join(format!("runtime_config_{pid}_{counter}.json5"));
    std::fs::write(&runtime_config_path, runtime_config_json5)?;

    let mut command = Command::new(program);
    command.current_dir(entity.root_path());
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env_vars {
        command.env(key, value);
    }
    command.env(RUNTIME_CONFIG_VAR_NAME, &runtime_config_path);

    // Force unbuffered stdout/stderr for Python nodes. Without this, Python
    // defaults to full buffering when stdout is a pipe, delaying log capture.
    if manifest.language == PeppygenLanguage::Python {
        command.env("PYTHONUNBUFFERED", "1");
    }

    command.spawn()
}

/// Performs a health check on a newly started node instance.
/// Polls the node's health service with a timeout and returns Ok if the node responds.
/// Also monitors the child process to detect early exits.
#[allow(clippy::too_many_arguments)]
pub async fn perform_health_check(
    messenger: &MessengerHandle,
    daemon_node_name: &str,
    caller_instance_id: &str,
    target_node_name: &str,
    target_daemon_node: &str,
    target_instance_id: &str,
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
                instance_id: Some(target_instance_id.to_string()),
                service_name: NODE_HEALTH_SERVICE.to_string(),
            });
            return Err(format!("health check timed out: {err}"));
        }

        let remaining = deadline - now;
        let attempt_timeout = remaining.min(Duration::from_millis(500));

        match ServiceMessenger::poll(
            messenger,
            daemon_node_name,
            caller_instance_id,
            target_node_name,
            NODE_HEALTH_SERVICE,
            Some(target_daemon_node),
            Some(target_instance_id),
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
#[allow(clippy::too_many_arguments)]
pub async fn wait_for_ready_signal(
    messenger: &MessengerHandle,
    daemon_node_name: &str,
    caller_instance_id: &str,
    target_node_name: &str,
    target_daemon_node: &str,
    target_instance_id: &str,
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
                instance_id: Some(target_instance_id.to_string()),
                service_name: NODE_READY_SERVICE.to_string(),
            });
            return Err(format!(
                "startup timed out waiting for node to be ready (node may still be compiling): {err}"
            ));
        }

        let remaining = deadline - now;
        let attempt_timeout = remaining.min(Duration::from_millis(500));

        match ServiceMessenger::poll(
            messenger,
            daemon_node_name,
            caller_instance_id,
            target_node_name,
            NODE_READY_SERVICE,
            Some(target_daemon_node),
            Some(target_instance_id),
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
