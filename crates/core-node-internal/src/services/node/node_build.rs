use super::super::action_loop::{ActionResult, ActionState, GoalHandler, run_action_loop};
use super::{create_action_log_file, write_error_to_log};
use crate::Result;
use crate::encoding::{NodeActionGoalResponse, NodeBuildGoal, NodeBuildResult};
use crate::names;
use chrono::Local;
use config::consts::PeppyDirs;
use futures::FutureExt;
use node_stack::{BuildContext, FeedbackLine, NodeStack};
use parking_lot::Mutex as StdMutex;
use peppylib::messaging::{ServiceRequestContext, TopicPublisher};
use peppylib::types::Payload;
use peppylib::{ActionMessenger, MessengerHandle, PeppyResult};
use std::fs::File;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tracing::debug;

pub async fn listen_for_node_build(
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    peppy_dirs: PeppyDirs,
) -> Result<JoinHandle<Result<()>>> {
    let action = ActionMessenger::expose(
        messenger,
        core_node_name,
        instance_id,
        node_name,
        names::NODE_BUILD_ACTION,
    )
    .await?;

    let handler = NodeBuildGoalHandler {
        context: NodeBuildActionContext {
            node_stack,
            peppy_dirs,
        },
        running_since: Arc::new(Mutex::new(None)),
        running_task: Arc::new(Mutex::new(None)),
    };

    let handle = tokio::spawn(async move { run_action_loop(action, handler).await });
    Ok(handle)
}

impl ActionResult for NodeBuildResult {
    fn identifier() -> &'static str {
        "node_build_result"
    }

    fn encode_result(&self) -> crate::Result<Payload> {
        self.encode()
    }
}

#[derive(Clone)]
pub(crate) struct NodeBuildActionContext {
    pub(crate) node_stack: Arc<NodeStack>,
    pub(crate) peppy_dirs: PeppyDirs,
}

#[derive(Clone)]
struct NodeBuildGoalHandler {
    context: NodeBuildActionContext,
    running_since: Arc<Mutex<Option<(Instant, u64)>>>,
    running_task: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl GoalHandler for NodeBuildGoalHandler {
    type Result = NodeBuildResult;

    async fn handle_goal(
        &self,
        context: ServiceRequestContext,
        feedback_publisher: TopicPublisher,
        state: Arc<Mutex<ActionState<NodeBuildResult>>>,
    ) -> PeppyResult<Payload> {
        handle_goal_request(
            context,
            feedback_publisher,
            state,
            self.context.clone(),
            Arc::clone(&self.running_since),
            Arc::clone(&self.running_task),
        )
        .await
    }
}

fn encode_rejected_goal(reason: impl Into<String>) -> PeppyResult<Payload> {
    NodeActionGoalResponse::rejected(reason)
        .encode()
        .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
            identifier: "node_build_goal".to_string(),
            reason: format!("Failed to encode response: {}", e),
        })
}

async fn handle_goal_request(
    context: ServiceRequestContext,
    feedback_publisher: TopicPublisher,
    state: Arc<Mutex<ActionState<NodeBuildResult>>>,
    action_context: NodeBuildActionContext,
    running_since: Arc<Mutex<Option<(Instant, u64)>>>,
    running_task: Arc<Mutex<Option<JoinHandle<()>>>>,
) -> PeppyResult<Payload> {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    let goal = match NodeBuildGoal::decode(payload.as_ref()) {
        Ok(g) => g,
        Err(e) => return encode_rejected_goal(format!("invalid payload: {}", e)),
    };

    let slot = (running_since.clone(), running_task.clone());
    match super::try_acquire_running_slot(&state, &slot, goal.force, goal.timeout_secs).await {
        super::RunningSlotOutcome::Acquired => {}
        super::RunningSlotOutcome::Rejected { remaining_secs } => {
            return encode_rejected_goal(format!(
                "action already in progress (times out in {remaining_secs}s), \
                 use `--force` to force building the node"
            ));
        }
    }
    if goal.force {
        debug!("Force flag set: aborting previous node_build task");
    }

    debug!(
        "Received `node_build` goal from {sender_instance_id}, {}:{}",
        goal.node_name, goal.node_tag
    );

    // Create the log file before spawning the build task so the goal
    // response can include its path.
    let log_dir = action_context.peppy_dirs.logs_dir_build();
    let timestamp = Local::now().format("%Y%m%d_%H%M%S_%3f").to_string();
    let log_filename = format!("{}_{}_{}.log", goal.node_name, goal.node_tag, timestamp);
    let (log_file, log_path) = match create_action_log_file(&log_dir, &log_filename) {
        Ok(result) => result,
        Err(error_msg) => {
            debug!("{}", error_msg);
            let mut state_guard = state.lock().await;
            *state_guard = ActionState::Rejected;
            return encode_rejected_goal(error_msg);
        }
    };

    debug!("Created log file for node build: {}", log_path.display());

    let state_clone = Arc::clone(&state);
    let log_path_clone = log_path.clone();
    let task_handle = tokio::spawn(async move {
        let (feedback_tx, consumer_handle) =
            super::spawn_node_feedback_consumer(feedback_publisher);

        let result =
            run_node_build(goal, action_context, feedback_tx, log_file, log_path_clone).await;

        let _ = consumer_handle.await;
        let mut state_guard = state_clone.lock().await;
        *state_guard = ActionState::Completed { result };
    });

    *running_task.lock().await = Some(task_handle);

    let response = NodeActionGoalResponse::accepted(&log_path);
    response
        .encode()
        .map_err(|e| peppylib::PeppyError::InvalidServiceRequest {
            identifier: "node_build_goal".to_string(),
            reason: format!("Failed to encode response: {}", e),
        })
}

/// Drives the build for an already-`Added` entity. Looks up the entity,
/// takes the pending build input stashed by `node_add`, and runs
/// [`NodeEntity::build`]. On any failure the entity is removed from the
/// stack so the user can re-run `peppy node add` cleanly.
pub(crate) async fn run_node_build(
    goal: NodeBuildGoal,
    action_context: NodeBuildActionContext,
    feedback_tx: mpsc::UnboundedSender<FeedbackLine>,
    log_file: Arc<StdMutex<File>>,
    log_path: PathBuf,
) -> NodeBuildResult {
    let log_file_for_panic = log_file.clone();
    let log_path_for_panic = log_path.clone();

    let fut = AssertUnwindSafe(try_run_build(
        &goal,
        &action_context,
        &feedback_tx,
        &log_file,
        &log_path,
    ));
    match fut.catch_unwind().await {
        Ok(Ok(snapshot_path)) => {
            debug!(
                "Built node {}:{} at {}",
                goal.node_name,
                goal.node_tag,
                snapshot_path.display()
            );
            NodeBuildResult::success(snapshot_path, &log_path)
        }
        Ok(Err(msg)) => {
            write_error_to_log(&log_file, &msg);
            NodeBuildResult::failure(&log_path, msg)
        }
        Err(panic_payload) => {
            let msg = format!(
                "node_build task panicked: {}",
                super::panic_message(&*panic_payload)
            );
            tracing::error!("{}", msg);
            write_error_to_log(&log_file_for_panic, &msg);
            NodeBuildResult::failure(log_path_for_panic, msg)
        }
    }
}

async fn try_run_build(
    goal: &NodeBuildGoal,
    action_context: &NodeBuildActionContext,
    feedback_tx: &mpsc::UnboundedSender<FeedbackLine>,
    log_file: &Arc<StdMutex<File>>,
    log_path: &Path,
) -> std::result::Result<PathBuf, String> {
    let entity_handle = action_context
        .node_stack
        .find(&goal.node_name, &goal.node_tag)
        .ok_or_else(|| {
            format!(
                "node {}:{} is not in the node stack — run `peppy node add` first",
                goal.node_name, goal.node_tag
            )
        })?;

    // Take the pending build input under a write lock. If there is no
    // pending input, the entity is not in a state where we can build it —
    // bail without mutating `last_add_log_path` so a rejected build doesn't
    // overwrite the previous successful add's recorded log.
    let pending_input = {
        let mut guard = entity_handle.write();
        let Some(input) = guard.take_pending_build_input() else {
            return Err(format!(
                "node {}:{} has no pending build input — current stage is {}",
                goal.node_name,
                goal.node_tag,
                guard.stage().name()
            ));
        };
        guard.set_last_add_log_path(log_path.to_path_buf());
        input
    };

    // Capture the entity generation before the build runs so the
    // failure-path cleanup only removes the entity we actually built.
    let expected_generation = entity_handle.read().generation();

    let build_result = node_stack::NodeEntity::build(
        &entity_handle,
        BuildContext {
            working_dir: pending_input.working_dir.path(),
            peppy_dirs: &action_context.peppy_dirs,
            feedback_tx,
            log_file: Arc::clone(log_file),
            env_vars: &pending_input.env_vars,
        },
    )
    .await;

    // `pending_input` drops at end of this scope; its `WorkingDir` RAII
    // handle removes the temp dir automatically.

    build_result.map_err(|e| {
        // `NodeEntity::build` leaves the entity in `Building` on failure.
        // Remove it so the user can re-run `peppy node add` cleanly.
        let _ = action_context.node_stack.remove_config_if_matches(
            &goal.node_name,
            &goal.node_tag,
            &entity_handle,
            expected_generation,
        );
        format!("Failed to build node: {}", e)
    })
}
