use crate::Result;
use crate::encoding::{NodeStartFeedback, NodeStartGoal, NodeStartResult};
use crate::names;
use bytes::Bytes;
use config::consts::RUNTIME_CONFIG_VAR_NAME;
use config::node::Name;
use config::runtime::RuntimeConfig;
use node_stack::{NodeEntity, NodeStack};
use peppylib::encoding::health::NodeHealthRequest;
use peppylib::encoding::ready::NodeReadyRequest;
use peppylib::messaging::{
    ActionCreation, NODE_HEALTH_SERVICE, NODE_READY_SERVICE, ServiceRequestContext, TopicPublisher,
};
use peppylib::{ActionMessenger, MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::debug;

const STDERR_BUFFER_LINES: usize = 20;

/// State for tracking the current node start action.
enum NodeStartActionState {
    /// No action is currently running.
    Idle,
    /// An action is currently running.
    Running,
    /// The action completed and the result is ready to be sent.
    Completed { result: NodeStartResult },
    /// The result has been sent to the requester.
    ResultSent { result: NodeStartResult },
}

impl Default for NodeStartActionState {
    fn default() -> Self {
        Self::Idle
    }
}

struct FeedbackPublishGuard {
    publish_enabled: Arc<AtomicBool>,
}

impl FeedbackPublishGuard {
    fn new(publish_enabled: Arc<AtomicBool>) -> Self {
        Self { publish_enabled }
    }
}

impl Drop for FeedbackPublishGuard {
    fn drop(&mut self) {
        self.publish_enabled.store(false, Ordering::Relaxed);
    }
}

fn push_stderr_line(buffer: &Arc<StdMutex<VecDeque<String>>>, line: &str) {
    let mut guard = buffer.lock().expect("stderr buffer lock poisoned");
    if guard.len() == STDERR_BUFFER_LINES {
        guard.pop_front();
    }
    guard.push_back(line.to_string());
}

fn spawn_output_reader<R: Read + Send + 'static>(
    reader: R,
    feedback_publisher: TopicPublisher,
    publish_enabled: Arc<AtomicBool>,
    is_stderr: bool,
    stderr_buffer: Option<Arc<StdMutex<VecDeque<String>>>>,
) {
    tokio::task::spawn_blocking(move || {
        let reader = BufReader::new(reader);
        let rt = tokio::runtime::Handle::current();
        for line in reader.lines().flatten() {
            if is_stderr {
                if let Some(buffer) = &stderr_buffer {
                    push_stderr_line(buffer, &line);
                }
            }
            if publish_enabled.load(Ordering::Relaxed) {
                let feedback = if is_stderr {
                    NodeStartFeedback::stderr(&line)
                } else {
                    NodeStartFeedback::stdout(&line)
                };
                if let Ok(payload) = feedback.encode() {
                    let _ = rt.block_on(feedback_publisher.publish(payload));
                }
            }
        }
    });
}

pub async fn listen_for_node_start(
    messenger: &MessengerHandle,
    master_node_node: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    node_startup_timeout: Duration,
    node_start_health_timeout: Duration,
) -> Result<JoinHandle<Result<()>>> {
    let action = ActionMessenger::expose(
        messenger,
        master_node_node,
        instance_id,
        node_name,
        names::NODE_START_ACTION,
    )
    .await?;

    let messenger = messenger.clone();
    let master_node_name = master_node_node.to_string();
    let caller_instance_id = instance_id.to_string();

    let handle = tokio::spawn(async move {
        run_node_start_action_loop(
            action,
            node_stack,
            messenger,
            master_node_name,
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
    master_node_name: String,
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
                let master_node_name = master_node_name.clone();
                let caller_instance_id = caller_instance_id.clone();
                let state = Arc::clone(&state);
                move |context| {
                    let feedback_publisher = feedback_publisher.clone();
                    let node_stack = Arc::clone(&node_stack);
                    let messenger = messenger.clone();
                    let master_node_name = master_node_name.clone();
                    let caller_instance_id = caller_instance_id.clone();
                    let state = Arc::clone(&state);
                    async move {
                        handle_goal_request(
                            context,
                            feedback_publisher,
                            node_stack,
                            messenger,
                            master_node_name,
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
            Ok(true) => loop {
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
                }
            },
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
    master_node_name: String,
    caller_instance_id: String,
    node_startup_timeout: Duration,
    node_start_health_timeout: Duration,
    state: Arc<Mutex<NodeStartActionState>>,
) -> PeppyResult<Bytes> {
    let sender_instance_id = context.message().instance_id().to_string();
    let payload = context.message().payload();

    {
        let mut state_guard = state.lock().await;
        if matches!(*state_guard, NodeStartActionState::Running) {
            return Ok(Bytes::from_static(
                b"goal rejected: action already in progress",
            ));
        }
        *state_guard = NodeStartActionState::Running;
    }

    let goal = match NodeStartGoal::decode(&payload.as_bytes()) {
        Ok(goal) => goal,
        Err(e) => {
            let result = NodeStartResult::failure(format!("Failed to decode goal: {}", e));
            let mut state_guard = state.lock().await;
            *state_guard = NodeStartActionState::Completed { result };
            return Ok(Bytes::from_static(b"goal rejected: invalid payload"));
        }
    };

    debug!(
        "Received `node_start` goal from {sender_instance_id}, node={}:{}, runtime_config_len={}",
        goal.node_name,
        goal.tag,
        goal.runtime_config_json5.len()
    );

    let state_clone = Arc::clone(&state);
    tokio::spawn(async move {
        let result = process_node_start(
            goal,
            sender_instance_id,
            node_stack,
            messenger,
            master_node_name,
            caller_instance_id,
            node_startup_timeout,
            node_start_health_timeout,
            feedback_publisher,
        )
        .await;
        let mut state_guard = state_clone.lock().await;
        *state_guard = NodeStartActionState::Completed { result };
    });

    Ok(Bytes::from_static(b"goal accepted"))
}

#[allow(clippy::too_many_arguments)]
async fn process_node_start(
    goal: NodeStartGoal,
    sender_instance_id: String,
    node_stack: Arc<NodeStack>,
    messenger: MessengerHandle,
    master_node_name: String,
    caller_instance_id: String,
    node_startup_timeout: Duration,
    node_start_health_timeout: Duration,
    feedback_publisher: TopicPublisher,
) -> NodeStartResult {
    let NodeStartGoal {
        runtime_config_json5,
        node_name,
        tag,
    } = goal;

    let runtime_config: RuntimeConfig = match serde_json5::from_str(&runtime_config_json5) {
        Ok(config) => config,
        Err(e) => {
            return NodeStartResult::failure(format!(
                "Failed to parse PEPPY_RUNTIME_CONFIG: {}",
                e
            ));
        }
    };

    let instance_id_str = runtime_config.deployment_instance.instance_id.as_str();
    let instance_id = match Name::new(instance_id_str) {
        Ok(name) => name,
        Err(e) => {
            return NodeStartResult::failure(format!("Invalid instance_id: {}", e));
        }
    };

    debug!(
        "Received `node_start` goal from {sender_instance_id}, node={}:{}, instance_id={}",
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

    let mut child = match start_node(&entity, &runtime_config_json5) {
        Ok(child) => child,
        Err(e) => {
            debug!("Failed to start node instance '{}': {}", instance_id_str, e);
            return NodeStartResult::failure(format!("Failed to start node: {}", e));
        }
    };

    let publish_enabled = Arc::new(AtomicBool::new(true));
    let _publish_guard = FeedbackPublishGuard::new(Arc::clone(&publish_enabled));
    let stderr_buffer = Arc::new(StdMutex::new(VecDeque::new()));

    if let Some(stdout) = child.stdout.take() {
        spawn_output_reader(
            stdout,
            feedback_publisher.clone(),
            Arc::clone(&publish_enabled),
            false,
            None,
        );
    }

    if let Some(stderr) = child.stderr.take() {
        spawn_output_reader(
            stderr,
            feedback_publisher.clone(),
            Arc::clone(&publish_enabled),
            true,
            Some(Arc::clone(&stderr_buffer)),
        );
    }

    debug!(
        "Successfully spawned node instance '{}', waiting for ready signal for {}s...",
        instance_id_str,
        node_startup_timeout.as_secs()
    );

    let ready_result = wait_for_ready_signal(
        &messenger,
        &master_node_name,
        &caller_instance_id,
        runtime_config.node_name.as_str(),
        runtime_config.bound_master_node.as_str(),
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
        return kill_and_report_error(child, instance_id_str, &e, stderr_buffer).await;
    }

    debug!(
        "Node instance '{}' is ready, performing health check...",
        instance_id_str
    );

    let health_result = perform_health_check(
        &messenger,
        &master_node_name,
        &caller_instance_id,
        runtime_config.node_name.as_str(),
        runtime_config.bound_master_node.as_str(),
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
            if let Err(e) = node_stack.add_instance(&node_name, &tag, Some(&instance_id)) {
                if let Err(kill_err) = child.kill() {
                    debug!(
                        "Failed to kill process for node instance '{}': {}",
                        instance_id_str, kill_err
                    );
                }
                return NodeStartResult::failure(format!("Failed to register instance: {}", e));
            }
            NodeStartResult::success()
        }
        Err(e) => {
            debug!(
                "Health check failed for node instance '{}': {}, killing process",
                instance_id_str, e
            );
            kill_and_report_error(child, instance_id_str, &e, stderr_buffer).await
        }
    }
}

async fn handle_cancel_request(
    _context: ServiceRequestContext,
    state: Arc<Mutex<NodeStartActionState>>,
) -> PeppyResult<Bytes> {
    let state_guard = state.lock().await;
    if matches!(*state_guard, NodeStartActionState::Running) {
        Ok(Bytes::from_static(
            b"cancel acknowledged (operation cannot be interrupted)",
        ))
    } else {
        Ok(Bytes::from_static(
            b"cancel acknowledged (no operation in progress)",
        ))
    }
}

async fn handle_result_request(
    _context: ServiceRequestContext,
    state: Arc<Mutex<NodeStartActionState>>,
) -> PeppyResult<Bytes> {
    let mut state_guard = state.lock().await;

    match std::mem::replace(&mut *state_guard, NodeStartActionState::Idle) {
        NodeStartActionState::Running => {
            *state_guard = NodeStartActionState::Running;
            Ok(Bytes::from_static(
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
        NodeStartActionState::Idle => {
            Ok(Bytes::from_static(b"result pending: no result available"))
        }
    }
}

/// Helper function to kill a child process and report an error with stderr tail capture.
async fn kill_and_report_error(
    mut child: Child,
    instance_id_str: &str,
    error: &str,
    stderr_buffer: Arc<StdMutex<VecDeque<String>>>,
) -> NodeStartResult {
    if let Err(kill_err) = child.kill() {
        debug!(
            "Failed to kill process for node instance '{}': {}",
            instance_id_str, kill_err
        );
    }

    let _ = tokio::task::spawn_blocking(move || child.wait()).await;

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
pub fn start_node(entity: &NodeEntity, runtime_config_json5: &str) -> std::io::Result<Child> {
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

    // Write the runtime config to a file in the node's .peppy directory
    // PEPPY_RUNTIME_CONFIG expects a file path, not JSON content
    let runtime_config_path = entity
        .root_path()
        .join(".peppy")
        .join("runtime")
        .join("runtime_config.json");

    if let Some(parent) = runtime_config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&runtime_config_path, runtime_config_json5)?;

    let mut command = Command::new(program);
    command.current_dir(entity.root_path());
    command
        .args(args)
        .env(RUNTIME_CONFIG_VAR_NAME, &runtime_config_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    command.spawn()
}

/// Performs a health check on a newly started node instance.
/// Polls the node's health service with a timeout and returns Ok if the node responds.
/// Also monitors the child process to detect early exits.
#[allow(clippy::too_many_arguments)]
pub async fn perform_health_check(
    messenger: &MessengerHandle,
    master_node_name: &str,
    caller_instance_id: &str,
    target_node_name: &str,
    target_master_node: &str,
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
            master_node_name,
            caller_instance_id,
            target_node_name,
            NODE_HEALTH_SERVICE,
            Some(target_master_node),
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
    master_node_name: &str,
    caller_instance_id: &str,
    target_node_name: &str,
    target_master_node: &str,
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
            master_node_name,
            caller_instance_id,
            target_node_name,
            NODE_READY_SERVICE,
            Some(target_master_node),
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
