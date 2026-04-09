use super::super::action_loop::{ActionResult, ActionState, GoalHandler, run_action_loop};
use super::{FeedbackLine, FeedbackStream, create_action_log_file, write_error_to_log};
use crate::Result;
use crate::encoding::{NodeBuildFeedback, NodeBuildGoal, NodeBuildGoalResponse, NodeBuildResult};
use crate::names;
use chrono::Local;
use config::consts::PeppyDirs;
use futures::FutureExt;
use node_stack::{BuildContext, NodeStack};
use parking_lot::Mutex as StdMutex;
use peppylib::messaging::{ServiceRequestContext, TopicPublisher};
use peppylib::types::Payload;
use peppylib::{ActionMessenger, MessengerHandle, PeppyResult};
use std::fs::File;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
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
    NodeBuildGoalResponse::rejected(reason)
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

    {
        let mut state_guard = state.lock().await;
        if matches!(*state_guard, ActionState::Running) {
            if !goal.force {
                let running_guard = running_since.lock().await;
                let remaining = running_guard
                    .map(|(started_at, timeout_secs)| {
                        Duration::from_secs(timeout_secs)
                            .saturating_sub(started_at.elapsed())
                            .as_secs()
                    })
                    .unwrap_or(0);
                return encode_rejected_goal(format!(
                    "action already in progress (times out in {remaining}s), \
                     use `--force` to force building the node"
                ));
            }

            debug!("Force flag set: aborting previous node_build task");
            let mut task_guard = running_task.lock().await;
            if let Some(handle) = task_guard.take() {
                handle.abort();
            }
        }
        *state_guard = ActionState::Running;
        *running_since.lock().await = Some((Instant::now(), goal.timeout_secs));
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
        let (feedback_tx, mut feedback_rx) = mpsc::unbounded_channel::<FeedbackLine>();
        let feedback_publisher_for_consumer = feedback_publisher.clone();
        let consumer_handle = tokio::spawn(async move {
            while let Some(line) = feedback_rx.recv().await {
                let feedback = match line.stream {
                    FeedbackStream::Stdout => NodeBuildFeedback::stdout(&line.line),
                    FeedbackStream::Stderr => NodeBuildFeedback::stderr(&line.line),
                    FeedbackStream::Warning => NodeBuildFeedback::warning(&line.line),
                };
                if let Ok(payload) = feedback.encode() {
                    let _ = feedback_publisher_for_consumer.publish(payload).await;
                }
            }
        });

        let result =
            run_node_build(goal, action_context, feedback_tx, log_file, log_path_clone).await;

        let _ = consumer_handle.await;
        let mut state_guard = state_clone.lock().await;
        *state_guard = ActionState::Completed { result };
    });

    *running_task.lock().await = Some(task_handle);

    let response = NodeBuildGoalResponse::accepted(&log_path);
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

    match AssertUnwindSafe(async {
        let entity_handle = match action_context
            .node_stack
            .find(&goal.node_name, &goal.node_tag)
        {
            Some(handle) => handle,
            None => {
                let msg = format!(
                    "node {}:{} is not in the node stack — run `peppy node add` first",
                    goal.node_name, goal.node_tag
                );
                write_error_to_log(&log_file, &msg);
                return NodeBuildResult::failure(&log_path, msg);
            }
        };

        // Take the pending build input under a write lock and record the
        // build log path on the entity. If there is no pending input, the
        // entity is not in a state where we can build it (already built, or
        // never received an add).
        let pending_input = {
            let mut guard = entity_handle.write();
            guard.set_last_add_log_path(log_path.clone());
            match guard.take_pending_build_input() {
                Some(input) => input,
                None => {
                    let msg = format!(
                        "node {}:{} has no pending build input — current stage is {}",
                        goal.node_name,
                        goal.node_tag,
                        guard.stage().name()
                    );
                    write_error_to_log(&log_file, &msg);
                    return NodeBuildResult::failure(&log_path, msg);
                }
            }
        };

        // Capture the entity generation before the build runs so the
        // failure-path cleanup only removes the entity we actually built.
        let expected_generation = entity_handle.read().generation();

        let build_result = node_stack::NodeEntity::build(
            &entity_handle,
            BuildContext {
                working_dir: &pending_input.working_dir,
                peppy_dirs: &action_context.peppy_dirs,
                feedback_tx: &feedback_tx,
                log_file: Arc::clone(&log_file),
                env_vars: &pending_input.env_vars,
            },
        )
        .await;

        let snapshot_path = match build_result {
            Ok(path) => path,
            Err(e) => {
                // `NodeEntity::build` leaves the entity in `Building` on
                // failure. Remove it so the user can re-run `peppy node add`
                // cleanly. The pointer + generation check guards against a
                // concurrent `push_config` racing in between.
                let _ = action_context.node_stack.remove_config_if_matches(
                    &goal.node_name,
                    &goal.node_tag,
                    &entity_handle,
                    expected_generation,
                );
                // Best-effort cleanup of the working dir, since the entity
                // is gone and the failed build never moved the artifact.
                let _ = std::fs::remove_dir_all(&pending_input.working_dir);
                let msg = format!("Failed to build node: {}", e);
                write_error_to_log(&log_file, &msg);
                return NodeBuildResult::failure(&log_path, msg);
            }
        };

        // Working dir is no longer needed; remove it.
        let _ = std::fs::remove_dir_all(&pending_input.working_dir);

        debug!(
            "Built node {}:{} at {}",
            goal.node_name,
            goal.node_tag,
            snapshot_path.display()
        );

        NodeBuildResult::success(snapshot_path, &log_path, goal.node_name, goal.node_tag)
    })
    .catch_unwind()
    .await
    {
        Ok(result) => result,
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
