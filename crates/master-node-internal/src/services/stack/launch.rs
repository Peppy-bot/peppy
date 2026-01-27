use crate::Result;
use crate::encoding::{LaunchGoal, LaunchGoalResponse, LaunchResult};
use crate::names;
use bytes::Bytes;
use chrono::Local;
use config::consts::logs_dir_launch;
use node_stack::NodeStack;
use peppylib::messaging::{ActionCreation, ServiceRequestContext, TopicPublisher};
use peppylib::{ActionMessenger, MessengerHandle, PeppyResult};
use std::fs::File;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::debug;

pub async fn listen_for_stack_launch(
    messenger: &MessengerHandle,
    master_node_name: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    _node_startup_timeout: Duration,
    _node_start_health_timeout: Duration,
) -> Result<JoinHandle<Result<()>>> {
    let action = ActionMessenger::expose(
        messenger,
        master_node_name,
        instance_id,
        node_name,
        names::STACK_LAUNCH_ACTION,
    )
    .await?;

    let handle = tokio::spawn({
        let messenger = messenger.clone();
        let bound_master_node = master_node_name.to_string();
        let master_instance_id = instance_id.to_string();
        async move {
            run_launch_action_loop(
                action,
                node_stack,
                messenger,
                bound_master_node,
                master_instance_id,
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
    bound_master_node: String,
    master_instance_id: String,
    node_stack: Arc<NodeStack>,
    feedback_publisher: TopicPublisher,
    log_file: Arc<StdMutex<File>>,
    log_path: PathBuf,
}

async fn run_launch_action_loop(
    mut action: ActionCreation,
    node_stack: Arc<NodeStack>,
    messenger: MessengerHandle,
    bound_master_node: String,
    master_instance_id: String,
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
                let bound_master_node = bound_master_node.clone();
                let master_instance_id = master_instance_id.clone();
                move |context| {
                    let feedback_publisher = feedback_publisher.clone();
                    let node_stack = Arc::clone(&node_stack);
                    let state = Arc::clone(&state);
                    let messenger = messenger.clone();
                    let bound_master_node = bound_master_node.clone();
                    let master_instance_id = master_instance_id.clone();

                    async move {
                        handle_goal_request(
                            context,
                            feedback_publisher,
                            node_stack,
                            state,
                            messenger,
                            bound_master_node,
                            master_instance_id,
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
                            let bound_master_node = bound_master_node.clone();
                            let master_instance_id = master_instance_id.clone();
                            move |context| {
                                let feedback_publisher = feedback_publisher.clone();
                                let node_stack = Arc::clone(&node_stack);
                                let state = Arc::clone(&state);
                                let messenger = messenger.clone();
                                let bound_master_node = bound_master_node.clone();
                                let master_instance_id = master_instance_id.clone();
                                async move {
                                    handle_goal_request(
                                        context,
                                        feedback_publisher,
                                        node_stack,
                                        state,
                                        messenger,
                                        bound_master_node,
                                        master_instance_id,
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
    bound_master_node: String,
    master_instance_id: String,
) -> PeppyResult<Bytes> {
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

    let goal = match LaunchGoal::decode(&payload.as_bytes()) {
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
        let ctx = ProcessLaunchContext {
            messenger,
            bound_master_node,
            master_instance_id,
            node_stack,
            feedback_publisher,
            log_file,
            log_path: log_path_clone.clone(),
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

async fn process_launch(goal: LaunchGoal, ctx: ProcessLaunchContext) -> LaunchResult {
    // TODO: Implement the launch logic
    // Step 1: goal.peppy_launch_json5 should turn into a PeppyLauncher object
    // Step 2: Clear up the node stack, all nodes and instances in the node stack should be removed
    // Step 3: Call the code in `crates/master-node-internal/src/services/node/info.rs` to retrieve
    //         the info of every node in the `deployments` (reuse the code, don't duplicate)
    // Step 4: Solve the dependencies between the nodes, if they match, continue, if not, raise an error
    // Step 5: Add every node to the node stack using functions from
    //         `crates/master-node-internal/src/services/node/add.rs` (reuse the code, don't duplicate). Add them one by one in the order of dependencies. Stream the console output to the feedback
    // Step 6: Start the instance of all the nodes using functions from
    //         `crates/master-node-internal/src/services/node/start.rs` (reuse the code, don't duplicate). Start them one by one in the order of dependencies. Stream the console output to the feedback
    //         The list of instances and their instance-id can be obtained from PeppyLauncher::deployments::instances
    // Step 7: Done, return a success to the user
    // Notes: Every step returns a feedback with LaunchFeedbackStep::LauncherStep when it's the launcher, LaunchFeedbackStep::AddingNode when a node is being added and LaunchFeedbackStep::StartingNode when a node is starting

    let _ = (&goal, &ctx);

    LaunchResult::failure(&ctx.log_path, "stack_launch action not yet implemented")
}

async fn handle_cancel_request(
    _context: ServiceRequestContext,
    state: Arc<Mutex<LaunchActionState>>,
) -> PeppyResult<Bytes> {
    let state_guard = state.lock().await;
    if matches!(*state_guard, LaunchActionState::Running) {
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
    state: Arc<Mutex<LaunchActionState>>,
) -> PeppyResult<Bytes> {
    let mut state_guard = state.lock().await;

    match std::mem::replace(&mut *state_guard, LaunchActionState::Idle) {
        LaunchActionState::Running => {
            *state_guard = LaunchActionState::Running;
            Ok(Bytes::from_static(
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
            Ok(Bytes::from_static(b"result pending: no result available"))
        }
    }
}
