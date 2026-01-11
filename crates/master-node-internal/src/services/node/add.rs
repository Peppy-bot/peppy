use crate::Result;
use crate::encoding::{NodeAddFeedback, NodeAddGoal, NodeAddResult};
use crate::names;
use bytes::Bytes;
use config::consts::{AppEnv, NODE_CONFIG_FILE, app_env};
use config::node::NodeConfigParser;
use node_stack::NodeStack;
use peppylib::messaging::{ActionCreation, ServiceRequestContext, TopicPublisher};
use peppylib::{ActionMessenger, MessengerHandle, PeppyResult};
use rand::Rng;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::debug;

/// Returns the base directory for storing copied node folders.
/// In production: ~/.peppy/nodes
/// In development: /tmp/.peppy/nodes
fn nodes_storage_dir() -> PathBuf {
    match app_env() {
        AppEnv::Prod => {
            let home = std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(PathBuf::from);
            home.unwrap_or_else(std::env::temp_dir)
                .join(".peppy")
                .join("nodes")
        }
        AppEnv::Dev => std::env::temp_dir().join(".peppy").join("nodes"),
    }
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

/// Runs the add_cmd for a node and streams output via the feedback publisher.
/// Returns Ok(()) if add_cmd is None or executes successfully.
async fn run_add_cmd_with_streaming(
    add_cmd: Option<&Vec<String>>,
    working_dir: &Path,
    feedback_publisher: &TopicPublisher,
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

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // Stream stdout
    if let Some(stdout) = stdout {
        let feedback_publisher = feedback_publisher.clone();
        tokio::task::spawn_blocking(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                if let Ok(line) = line {
                    let feedback = NodeAddFeedback::stdout(&line);
                    if let Ok(payload) = feedback.encode() {
                        // Use a blocking approach since we're in spawn_blocking
                        let rt = tokio::runtime::Handle::current();
                        let _ = rt.block_on(feedback_publisher.publish(payload));
                    }
                }
            }
        });
    }

    // Stream stderr
    if let Some(stderr) = stderr {
        let feedback_publisher = feedback_publisher.clone();
        tokio::task::spawn_blocking(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                if let Ok(line) = line {
                    let feedback = NodeAddFeedback::stderr(&line);
                    if let Ok(payload) = feedback.encode() {
                        let rt = tokio::runtime::Handle::current();
                        let _ = rt.block_on(feedback_publisher.publish(payload));
                    }
                }
            }
        });
    }

    // Wait for the process to complete
    let status = tokio::task::spawn_blocking(move || child.wait())
        .await
        .map_err(|e| format!("failed to wait for add_cmd: {}", e))?
        .map_err(|e| format!("failed to wait for add_cmd: {}", e))?;

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
    let storage_dir = nodes_storage_dir();
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

/// State for tracking the current node add action.
enum NodeAddActionState {
    /// No action is currently running.
    Idle,
    /// An action is currently running.
    Running,
    /// The action completed and the result is ready to be sent.
    Completed { result: NodeAddResult },
    /// The result has been sent to the requester.
    ResultSent { result: NodeAddResult },
}

impl Default for NodeAddActionState {
    fn default() -> Self {
        Self::Idle
    }
}

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
                // Goal accepted, now wait for result or cancel requests
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
            return Ok(Bytes::from_static(
                b"goal rejected: action already in progress",
            ));
        }
        *state_guard = NodeAddActionState::Running;
    }

    let goal = match NodeAddGoal::decode(&payload.as_bytes()) {
        Ok(g) => g,
        Err(e) => {
            let result = NodeAddResult::failure(format!("Failed to decode goal: {}", e));
            let mut state_guard = state.lock().await;
            *state_guard = NodeAddActionState::Completed { result };
            return Ok(Bytes::from_static(b"goal rejected: invalid payload"));
        }
    };

    debug!(
        "Received `node_add` goal from {sender_instance_id}, from_dir={}",
        goal.from_dir.display()
    );

    // Process the add operation in a separate task to not block goal response
    let state_clone = Arc::clone(&state);
    let feedback_publisher_clone = feedback_publisher.clone();
    tokio::spawn(async move {
        let result = process_node_add(goal, node_stack, feedback_publisher_clone).await;
        let mut state_guard = state_clone.lock().await;
        *state_guard = NodeAddActionState::Completed { result };
    });

    Ok(Bytes::from_static(b"goal accepted"))
}

async fn process_node_add(
    goal: NodeAddGoal,
    node_stack: Arc<NodeStack>,
    feedback_publisher: TopicPublisher,
) -> NodeAddResult {
    // Parse the node configuration from the request directory.
    let config_path = goal.from_dir.join(NODE_CONFIG_FILE);
    let node_config = match NodeConfigParser::from_path(&config_path) {
        Ok(config) => config,
        Err(e) => {
            return NodeAddResult::failure(format!(
                "Failed to parse node config at {}: {}",
                config_path.display(),
                e
            ));
        }
    };

    let node_name = node_config.manifest.name.as_str().to_owned();
    let node_tag = node_config.manifest.tag.clone();

    // Copy the node folder to the peppy storage directory.
    let copied_path = match copy_node_to_storage(&goal.from_dir, &node_name, &node_tag) {
        Ok(path) => path,
        Err(e) => {
            return NodeAddResult::failure(format!("Failed to copy node folder: {}", e));
        }
    };

    // TODO: Run generate on the copied node
    // TODO: Make sure the received node_config fingerprint matches the one that is in the generated folder

    // Run add_cmd on the copied folder with streaming output
    if let Err(e) = run_add_cmd_with_streaming(
        node_config.manifest.add_cmd.as_ref(),
        &copied_path,
        &feedback_publisher,
    )
    .await
    {
        // Clean up the copied folder on failure
        let _ = std::fs::remove_dir_all(&copied_path);
        return NodeAddResult::failure(format!("add_cmd failed: {}", e));
    }

    // Add the node config to the stack
    if let Err(e) = node_stack.push_config(node_config, false, &copied_path) {
        // Clean up the copied folder on failure
        let _ = std::fs::remove_dir_all(&copied_path);
        return NodeAddResult::failure(format!("Failed to add node config: {}", e));
    }

    debug!(
        "Added node {}:{} at {}",
        node_name,
        node_tag,
        copied_path.display()
    );

    NodeAddResult::success(copied_path)
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
        NodeAddActionState::Idle => Ok(Bytes::from_static(b"result pending: no result available")),
    }
}
