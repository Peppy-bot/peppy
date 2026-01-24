use crate::Result;
use crate::encoding::{NodeAddFeedback, NodeAddGoal, NodeAddGoalResponse, NodeAddResult};
use crate::names;
use bytes::Bytes;
use chrono::Local;
use config::consts::{
    NODE_CONFIG_FILE, PEPPY_OUTPUT_DIR, PEPPYGEN_OUTPUT_PATH, logs_dir_add, peppy_data_dir,
};
use config::node::{NodeConfig, NodeConfigParser};
use node_stack::NodeStack;
use peppylib::messaging::{ActionCreation, ServiceRequestContext, TopicPublisher};
use peppylib::{ActionMessenger, MessengerHandle, PeppyResult};
use rand::Rng;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tracing::debug;

pub async fn listen_for_node_add(
    messenger: &MessengerHandle,
    master_node_name: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
) -> Result<JoinHandle<Result<()>>> {
    let action = ActionMessenger::expose(
        messenger,
        master_node_name,
        instance_id,
        node_name,
        names::NODE_ADD_ACTION,
    )
    .await?;

    let handle = tokio::spawn(async move { run_node_add_action_loop(action, node_stack).await });

    Ok(handle)
}

fn generate_random_id() -> String {
    let mut rng = rand::rng();
    let bytes: [u8; 3] = rng.random();
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;

    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dest_path = dest.join(entry.file_name());

        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dest_path)?;
        } else {
            std::fs::copy(&src_path, &dest_path)?;
        }
    }
    Ok(())
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

fn spawn_output_reader<R: Read + Send + 'static>(
    reader: R,
    feedback_tx: mpsc::UnboundedSender<FeedbackLine>,
    stream: FeedbackStream,
    log_file: Arc<StdMutex<File>>,
) -> JoinHandle<()> {
    let stream_prefix = match stream {
        FeedbackStream::Stdout => "stdout",
        FeedbackStream::Stderr => "stderr",
    };

    tokio::task::spawn_blocking(move || {
        let reader = BufReader::new(reader);
        for line in reader.lines().map_while(|r| r.ok()) {
            // Always write to log file
            if let Ok(mut file) = log_file.lock() {
                let timestamp = Local::now().format("%Y-%m-%dT%H:%M:%S%.3f");
                let _ = writeln!(file, "[{}] [{}] {}", timestamp, stream_prefix, line);
            }

            let _ = feedback_tx.send(FeedbackLine {
                stream,
                line: line.to_string(),
            });
        }
    })
}

/// Runs the add_cmd for a node and streams output via the feedback publisher.
/// Returns Ok(()) if add_cmd is None or executes successfully.
async fn run_add_cmd_with_streaming(
    add_cmd: Option<&Vec<String>>,
    working_dir: &Path,
    feedback_publisher: &TopicPublisher,
    log_file: Arc<StdMutex<File>>,
) -> std::result::Result<(), String> {
    let Some(cmd) = add_cmd else {
        return Ok(());
    };

    let Some((program, args)) = cmd.split_first() else {
        return Err("add_cmd is empty".to_string());
    };

    debug!(
        "Running add_cmd: {} {:?} in dir {:?}",
        program, args, working_dir
    );

    let mut child = Command::new(program)
        .args(args)
        .current_dir(working_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to execute add_cmd: {}", e))?;

    let (feedback_tx, mut feedback_rx) = mpsc::unbounded_channel::<FeedbackLine>();
    let feedback_publisher = feedback_publisher.clone();
    let publisher_handle = tokio::spawn(async move {
        while let Some(line) = feedback_rx.recv().await {
            let feedback = match line.stream {
                FeedbackStream::Stdout => NodeAddFeedback::stdout(&line.line),
                FeedbackStream::Stderr => NodeAddFeedback::stderr(&line.line),
            };
            if let Ok(payload) = feedback.encode() {
                let _ = feedback_publisher.publish(payload).await;
            }
        }
    });

    let mut reader_handles = Vec::new();

    // Stream stdout
    if let Some(stdout) = child.stdout.take() {
        reader_handles.push(spawn_output_reader(
            stdout,
            feedback_tx.clone(),
            FeedbackStream::Stdout,
            Arc::clone(&log_file),
        ));
    }

    // Stream stderr
    if let Some(stderr) = child.stderr.take() {
        reader_handles.push(spawn_output_reader(
            stderr,
            feedback_tx.clone(),
            FeedbackStream::Stderr,
            Arc::clone(&log_file),
        ));
    }

    // Wait for the process to complete
    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .map_err(|e| format!("failed to wait for add_cmd: {}", e))?
        .map_err(|e| format!("failed to wait for add_cmd: {}", e))?;

    for handle in reader_handles {
        let _ = handle.await;
    }
    drop(feedback_tx);
    let _ = publisher_handle.await;

    if !status.success() {
        return Err(format!("add_cmd failed with status {}", status));
    }

    debug!("add_cmd completed successfully");
    Ok(())
}

/// Copies a node folder to the peppy nodes storage directory.
///
/// The destination path follows the format: `<storage_dir>/<node_name>_<tag>_<uuid>`
///
/// Returns the path to the copied folder.
fn copy_node_to_storage(from_dir: &Path, node_name: &str, node_tag: &str) -> Result<PathBuf> {
    let storage_dir = peppy_data_dir().join("nodes");
    let random_id = generate_random_id();
    let folder_name = format!("{}_{}_{}", node_name, node_tag, random_id);
    let dest_path = storage_dir.join(&folder_name);

    debug!(
        "Copying node folder from {} to {}",
        from_dir.display(),
        dest_path.display()
    );

    copy_dir_recursive(from_dir, &dest_path)?;

    Ok(dest_path)
}

fn is_node_snapshot_path(path: &Path, node_name: &str, node_tag: &str) -> bool {
    let storage_dir = peppy_data_dir().join("nodes");
    if !path.starts_with(&storage_dir) {
        return false;
    }
    let Some(folder_name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    folder_name.starts_with(&format!("{node_name}_{node_tag}_"))
}

/// State for tracking the current node add action.
#[derive(Default)]
enum NodeAddActionState {
    /// No action is currently running.
    #[default]
    Idle,
    /// The goal was rejected (no result polling expected).
    Rejected,
    /// An action is currently running.
    Running,
    /// The action completed and the result is ready to be sent.
    Completed { result: NodeAddResult },
    /// The result has been sent to the requester.
    ResultSent { result: NodeAddResult },
}

async fn run_node_add_action_loop(
    mut action: ActionCreation,
    node_stack: Arc<NodeStack>,
) -> Result<()> {
    let state = Arc::new(Mutex::new(NodeAddActionState::default()));

    loop {
        // Wait for a goal request
        let goal_result = action
            .goal_service
            .handle_next_request({
                let feedback_publisher = &action.feedback_publisher;
                let node_stack = Arc::clone(&node_stack);
                let state = Arc::clone(&state);
                move |context| {
                    let feedback_publisher = feedback_publisher.clone();
                    let node_stack = Arc::clone(&node_stack);
                    let state = Arc::clone(&state);

                    async move {
                        handle_goal_request(context, feedback_publisher, node_stack, state).await
                    }
                }
            })
            .await;

        match goal_result {
            Ok(true) => {
                // Check if the goal was rejected (no result polling expected)
                {
                    let mut state_guard = state.lock().await;
                    if matches!(*state_guard, NodeAddActionState::Rejected) {
                        // Goal was rejected, reset to Idle and wait for next goal
                        *state_guard = NodeAddActionState::Idle;
                        continue;
                    }
                }

                // Goal accepted, now wait for result, cancel, or new goal requests.
                // We also listen for new goals here to handle the case where a client
                // abandons an action without polling for the result.
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
                                    if matches!(*state_guard, NodeAddActionState::ResultSent { .. }) {
                                        *state_guard = NodeAddActionState::default();
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
                        // Handle new goals while waiting for result/cancel.
                        // This allows a new goal to be accepted if the previous action
                        // completed but the client never polled for the result.
                        // If an action is still running, the goal will be rejected
                        // with "action already in progress".
                        goal_result = action.goal_service.handle_next_request({
                            let feedback_publisher = &action.feedback_publisher;
                            let node_stack = Arc::clone(&node_stack);
                            let state = Arc::clone(&state);
                            move |context| {
                                let feedback_publisher = feedback_publisher.clone();
                                let node_stack = Arc::clone(&node_stack);
                                let state = Arc::clone(&state);
                                async move {
                                    handle_goal_request(context, feedback_publisher, node_stack, state).await
                                }
                            }
                        }) => {
                            match goal_result {
                                Ok(true) => {
                                    // Goal was handled. Check if it was rejected.
                                    let mut state_guard = state.lock().await;
                                    if matches!(*state_guard, NodeAddActionState::Rejected) {
                                        // Goal was rejected (for reasons other than "in progress"),
                                        // reset to Idle and continue waiting.
                                        *state_guard = NodeAddActionState::Idle;
                                    }
                                    // If state is Running: either old action is still running
                                    // (new goal rejected for "in progress"), or new goal was
                                    // accepted and a new action started. Either way, continue
                                    // waiting in the inner loop.
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
    state: Arc<Mutex<NodeAddActionState>>,
) -> PeppyResult<Bytes> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    // Check if already running and mark as running if not
    {
        let mut state_guard = state.lock().await;
        if matches!(*state_guard, NodeAddActionState::Running) {
            let response = NodeAddGoalResponse::rejected("action already in progress");
            return response
                .encode()
                .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
                    identifier: "node_add_goal".to_string(),
                    reason: format!("Failed to encode response: {}", e),
                });
        }
        *state_guard = NodeAddActionState::Running;
    }

    let goal = match NodeAddGoal::decode(&payload.as_bytes()) {
        Ok(g) => g,
        Err(e) => {
            let mut state_guard = state.lock().await;
            *state_guard = NodeAddActionState::Rejected;
            let response = NodeAddGoalResponse::rejected(format!("invalid payload: {}", e));
            return response
                .encode()
                .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
                    identifier: "node_add_goal".to_string(),
                    reason: format!("Failed to encode response: {}", e),
                });
        }
    };

    debug!(
        "Received `node_add` goal from {sender_instance_id}, from_dir={}",
        goal.from_dir.display()
    );

    // Verify that the node directory is in sync with the currently running daemon.
    let git_hash_path = goal.from_dir.join(PEPPY_OUTPUT_DIR).join("git.hash");
    let stored_git_hash = match std::fs::read_to_string(&git_hash_path) {
        Ok(hash) => hash,
        Err(e) => {
            let error_msg = format!(
                "Missing required git hash file at {}: {}. Run `peppy node sync` before `peppy node add`.",
                git_hash_path.display(),
                e
            );
            let mut state_guard = state.lock().await;
            *state_guard = NodeAddActionState::Rejected;
            let response = NodeAddGoalResponse::rejected(&error_msg);
            return response
                .encode()
                .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
                    identifier: "node_add_goal".to_string(),
                    reason: format!("Failed to encode response: {}", e),
                });
        }
    };

    let expected_git_hash = goal.git_hash.trim();
    let stored_git_hash = stored_git_hash.trim();

    if stored_git_hash.is_empty() {
        let error_msg = format!(
            "Invalid git hash file at {}: file is empty. Run `peppy node sync` before `peppy node add`.",
            git_hash_path.display(),
        );
        let mut state_guard = state.lock().await;
        *state_guard = NodeAddActionState::Rejected;
        let response = NodeAddGoalResponse::rejected(&error_msg);
        return response
            .encode()
            .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
                identifier: "node_add_goal".to_string(),
                reason: format!("Failed to encode response: {}", e),
            });
    }

    if stored_git_hash != expected_git_hash {
        let error_msg = format!(
            "git hash mismatch for node directory {} (expected '{}', found '{}' in {}). Run `peppy node sync` before retrying.",
            goal.from_dir.display(),
            expected_git_hash,
            stored_git_hash,
            git_hash_path.display(),
        );
        let mut state_guard = state.lock().await;
        *state_guard = NodeAddActionState::Rejected;
        let response = NodeAddGoalResponse::rejected(&error_msg);
        return response
            .encode()
            .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
                identifier: "node_add_goal".to_string(),
                reason: format!("Failed to encode response: {}", e),
            });
    }

    // Parse node config to get node_name and tag for log file naming
    let config_path = goal.from_dir.join(NODE_CONFIG_FILE);
    let node_config = match NodeConfigParser::from_path(&config_path) {
        Ok(config) => config,
        Err(e) => {
            let error_msg = format!(
                "Failed to parse node config at {}: {}",
                config_path.display(),
                e
            );
            let mut state_guard = state.lock().await;
            *state_guard = NodeAddActionState::Rejected;
            let response = NodeAddGoalResponse::rejected(&error_msg);
            return response
                .encode()
                .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
                    identifier: "node_add_goal".to_string(),
                    reason: format!("Failed to encode response: {}", e),
                });
        }
    };

    let node_name = node_config.manifest.name.as_str().to_owned();
    let node_tag = node_config.manifest.tag.clone();

    // Create log file with timestamp-based filename
    let log_dir = logs_dir_add();
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        let error_msg = format!("Failed to create logs directory: {}", e);
        debug!("Failed to create logs directory {:?}: {}", log_dir, e);
        let mut state_guard = state.lock().await;
        *state_guard = NodeAddActionState::Rejected;
        let response = NodeAddGoalResponse::rejected(&error_msg);
        return response
            .encode()
            .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
                identifier: "node_add_goal".to_string(),
                reason: format!("Failed to encode response: {}", e),
            });
    }

    let timestamp = Local::now().format("%Y%m%d_%H%M%S_%3f");
    let log_filename = format!("{}_{}_{}.log", node_name, node_tag, timestamp);
    let log_path = log_dir.join(&log_filename);
    let log_file = match File::create(&log_path) {
        Ok(file) => Arc::new(StdMutex::new(file)),
        Err(e) => {
            let error_msg = format!("Failed to create log file: {}", e);
            debug!("Failed to create log file {:?}: {}", log_path, e);
            let mut state_guard = state.lock().await;
            *state_guard = NodeAddActionState::Rejected;
            let response = NodeAddGoalResponse::rejected(&error_msg);
            return response
                .encode()
                .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
                    identifier: "node_add_goal".to_string(),
                    reason: format!("Failed to encode response: {}", e),
                });
        }
    };

    debug!("Created log file for node add: {}", log_path.display());

    // Process the add operation in a separate task to not block goal response
    let state_clone = Arc::clone(&state);
    let feedback_publisher_clone = feedback_publisher.clone();
    let log_path_clone = log_path.clone();
    tokio::spawn(async move {
        let result = process_node_add(
            goal,
            node_config,
            node_stack,
            feedback_publisher_clone,
            log_file,
            log_path_clone,
        )
        .await;
        let mut state_guard = state_clone.lock().await;
        *state_guard = NodeAddActionState::Completed { result };
    });

    let response = NodeAddGoalResponse::accepted(&log_path);
    response
        .encode()
        .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
            identifier: "node_add_goal".to_string(),
            reason: format!("Failed to encode response: {}", e),
        })
}

async fn process_node_add(
    goal: NodeAddGoal,
    node_config: NodeConfig,
    node_stack: Arc<NodeStack>,
    feedback_publisher: TopicPublisher,
    log_file: Arc<StdMutex<File>>,
    log_path: PathBuf,
) -> NodeAddResult {
    let node_name = node_config.manifest.name.as_str().to_owned();
    let node_tag = node_config.manifest.tag.clone();

    let previous_snapshot_path = node_stack
        .find(&node_name, &node_tag)
        .map(|entity| entity.root_path().to_path_buf());

    // Copy the node folder to the peppy storage directory.
    let copied_path = match copy_node_to_storage(&goal.from_dir, &node_name, &node_tag) {
        Ok(path) => path,
        Err(e) => {
            return NodeAddResult::failure(&log_path, format!("Failed to copy node folder: {}", e));
        }
    };

    // Verify that the node config fingerprint matches the one in the generated folder
    let config_path = copied_path.join(NODE_CONFIG_FILE);
    if let Err(e) =
        config::fingerprint::verify_codegen_fingerprint(&config_path, PEPPYGEN_OUTPUT_PATH)
    {
        // Clean up the copied folder on failure
        let _ = std::fs::remove_dir_all(&copied_path);
        return NodeAddResult::failure(
            &log_path,
            format!("Fingerprint verification failed: {}", e),
        );
    }

    // Run add_cmd on the copied folder with streaming output
    if let Err(e) = run_add_cmd_with_streaming(
        node_config.manifest.add_cmd.as_ref(),
        &copied_path,
        &feedback_publisher,
        log_file,
    )
    .await
    {
        // Clean up the copied folder on failure
        let _ = std::fs::remove_dir_all(&copied_path);
        return NodeAddResult::failure(&log_path, format!("add_cmd failed: {}", e));
    }

    // Add the node config to the stack
    if let Err(e) = node_stack.push_config(node_config, false, &copied_path) {
        // Clean up the copied folder on failure
        let _ = std::fs::remove_dir_all(&copied_path);
        return NodeAddResult::failure(&log_path, format!("Failed to add node config: {}", e));
    }

    if let Some(previous_snapshot_path) = previous_snapshot_path
        && previous_snapshot_path != copied_path
        && is_node_snapshot_path(&previous_snapshot_path, &node_name, &node_tag)
    {
        let _ = std::fs::remove_dir_all(&previous_snapshot_path);
    }

    debug!(
        "Added node {}:{} at {}",
        node_name,
        node_tag,
        copied_path.display()
    );

    NodeAddResult::success(copied_path, &log_path)
}

async fn handle_cancel_request(
    _context: ServiceRequestContext,
    state: Arc<Mutex<NodeAddActionState>>,
) -> PeppyResult<Bytes> {
    // For now, we don't support cancellation of the add operation
    // Just acknowledge the request
    let state_guard = state.lock().await;
    if matches!(*state_guard, NodeAddActionState::Running) {
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
    state: Arc<Mutex<NodeAddActionState>>,
) -> PeppyResult<Bytes> {
    let mut state_guard = state.lock().await;

    match std::mem::replace(&mut *state_guard, NodeAddActionState::Idle) {
        NodeAddActionState::Running => {
            // Still running, restore state and return pending status
            *state_guard = NodeAddActionState::Running;
            Ok(Bytes::from_static(
                b"result pending: operation still in progress",
            ))
        }
        NodeAddActionState::Completed { result } => {
            let payload =
                result
                    .encode()
                    .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
                        identifier: "node_add_result".to_string(),
                        reason: format!("Failed to encode result: {}", e),
                    })?;
            *state_guard = NodeAddActionState::ResultSent { result };
            Ok(payload)
        }
        NodeAddActionState::ResultSent { result } => {
            // Result was already sent, restore state and return it again
            let payload =
                result
                    .encode()
                    .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
                        identifier: "node_add_result".to_string(),
                        reason: format!("Failed to encode result: {}", e),
                    })?;
            *state_guard = NodeAddActionState::ResultSent { result };
            Ok(payload)
        }
        NodeAddActionState::Idle | NodeAddActionState::Rejected => {
            Ok(Bytes::from_static(b"result pending: no result available"))
        }
    }
}
