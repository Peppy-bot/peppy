use super::super::action_loop::{ActionResult, ActionState, GoalHandler, run_action_loop};
use super::write_error_to_log;
use super::{FeedbackLine, FeedbackStream, create_action_log_file};
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

    fn encode_result(&self) -> Result<Payload> {
        self.encode()
    }
}

/// Drives a build for the entity named `(node_name, node_tag)` that is
/// currently in `Added`. The caller supplies the log file/path and the
/// feedback channel; everything else is recovered from the entity itself.
pub(crate) async fn run_node_build_for_entity(
    node_name: String,
    node_tag: String,
    env_vars: Vec<(String, String)>,
    action_context: NodeBuildActionContext,
    feedback_tx: mpsc::UnboundedSender<FeedbackLine>,
    log_file: Arc<StdMutex<File>>,
    log_path: PathBuf,
) -> NodeBuildResult {
    let entity_handle = match action_context.node_stack.find(&node_name, &node_tag) {
        Some(handle) => handle,
        None => {
            let msg = format!("node `{}:{}` is not in the node stack", node_name, node_tag);
            write_error_to_log(&log_file, &msg);
            return NodeBuildResult::failure(&log_path, msg);
        }
    };

    let working_dir_guard = {
        let guard = entity_handle.read();
        if let Err(stage) = guard.stage().ensure_buildable() {
            let msg = format!(
                "node `{}:{}` is in stage `{}`; cannot build",
                node_name, node_tag, stage
            );
            write_error_to_log(&log_file, &msg);
            return NodeBuildResult::failure(&log_path, msg);
        }
        match guard.pending_working_dir() {
            Some(g) => g,
            None => {
                let msg = format!(
                    "node `{}:{}` has no staged working directory",
                    node_name, node_tag
                );
                write_error_to_log(&log_file, &msg);
                return NodeBuildResult::failure(&log_path, msg);
            }
        }
    };

    let goal = NodeBuildGoal {
        node_name,
        node_tag,
        env_vars,
        timeout_secs: 0,
        force: false,
    };
    run_node_build(
        goal,
        entity_handle,
        working_dir_guard,
        action_context,
        feedback_tx,
        log_file,
        log_path,
    )
    .await
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
        "Received `node_build` goal from {sender_instance_id}, target={}:{}",
        goal.node_name, goal.node_tag
    );

    // Look up the entity *before* creating the log file so we can fail fast
    // when the user typo'd the node name.
    let entity_handle = match action_context
        .node_stack
        .find(&goal.node_name, &goal.node_tag)
    {
        Some(handle) => handle,
        None => {
            let mut state_guard = state.lock().await;
            *state_guard = ActionState::Rejected;
            return encode_rejected_goal(format!(
                "node `{}:{}` is not in the node stack — run `peppy node add` first",
                goal.node_name, goal.node_tag
            ));
        }
    };

    // Pull the staged working directory from the entity. Reject when the
    // entity is not in `Added` (a build is already in flight or the entity
    // is `Ready`). Bind the read-lock-derived values to local owned values
    // before any `.await`, so the parking_lot guard never crosses an await.
    let pending = {
        let guard = entity_handle.read();
        match guard.stage().ensure_buildable() {
            Ok(()) => Ok(guard.pending_working_dir()),
            Err(stage) => Err(stage.to_string()),
        }
    };
    let working_dir_guard = match pending {
        Err(stage) => {
            let mut state_guard = state.lock().await;
            *state_guard = ActionState::Rejected;
            return encode_rejected_goal(format!(
                "node `{}:{}` is in stage `{}`; cannot build",
                goal.node_name, goal.node_tag, stage
            ));
        }
        Ok(None) => {
            let mut state_guard = state.lock().await;
            *state_guard = ActionState::Rejected;
            return encode_rejected_goal(format!(
                "node `{}:{}` has no staged working directory; \
                 re-run `peppy node add` to stage one",
                goal.node_name, goal.node_tag
            ));
        }
        Ok(Some(g)) => g,
    };

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
    debug!("Build log file: {}", log_path.display());

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

        let result = run_node_build(
            goal,
            entity_handle,
            working_dir_guard,
            action_context,
            feedback_tx,
            log_file,
            log_path_clone,
        )
        .await;

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

async fn run_node_build(
    goal: NodeBuildGoal,
    entity_handle: Arc<parking_lot::RwLock<node_stack::NodeEntity>>,
    working_dir_guard: Arc<node_stack::WorkingDirGuard>,
    action_context: NodeBuildActionContext,
    feedback_tx: mpsc::UnboundedSender<FeedbackLine>,
    log_file: Arc<StdMutex<File>>,
    log_path: PathBuf,
) -> NodeBuildResult {
    let log_file_for_panic = log_file.clone();
    let log_path_for_panic = log_path.clone();

    match AssertUnwindSafe(async {
        // Validate + inject env vars at build time so callers can pass a
        // fresh environment per build.
        let mut env_vars = match super::validate_goal_env_vars(&goal.env_vars) {
            Ok(v) => v,
            Err(e) => {
                let msg = e.to_string();
                write_error_to_log(&log_file, &msg);
                return NodeBuildResult::failure(&log_path, msg);
            }
        };

        let language = entity_handle.read().config().execution.language;
        let sccache_injected = super::inject_rust_build_env(&mut env_vars, language);
        if sccache_injected {
            let _ = feedback_tx.send(FeedbackLine {
                stream: FeedbackStream::Stdout,
                line: "Using sccache for Rust compilation".to_string(),
            });
        }
        super::inject_node_runtime_env(&mut env_vars, &goal.node_name, &goal.node_tag);

        // Take the working dir from the entity so a concurrent (rejected)
        // build cannot reuse it once we start mutating it.
        {
            let mut guard = entity_handle.write();
            // Drop the entity-side reference; the local `working_dir_guard`
            // keeps the dir alive for the duration of the build, and the
            // shared Arc means cleanup happens once the last clone drops.
            let _ = guard.take_pending_working_dir();
        }

        let working_dir_path = working_dir_guard.path().to_path_buf();
        let expected_generation = entity_handle.read().generation();

        let build_result = node_stack::NodeEntity::build(
            &entity_handle,
            BuildContext {
                working_dir: &working_dir_path,
                peppy_dirs: &action_context.peppy_dirs,
                feedback_tx: &feedback_tx,
                log_file: Arc::clone(&log_file),
                env_vars: &env_vars,
            },
        )
        .await;

        match build_result {
            Ok(artifact_path) => {
                debug!(
                    "Built node {}:{} at {}",
                    goal.node_name,
                    goal.node_tag,
                    artifact_path.display()
                );
                NodeBuildResult::success(artifact_path, &log_path, goal.node_name, goal.node_tag)
            }
            Err(e) => {
                // `NodeEntity::build` leaves the entity in `Building` on
                // failure. Roll it out of the stack so the user can re-add.
                let _ = action_context.node_stack.remove_config_if_matches(
                    &goal.node_name,
                    &goal.node_tag,
                    &entity_handle,
                    expected_generation,
                );
                let msg = format!("Failed to build node: {}", e);
                write_error_to_log(&log_file, &msg);
                NodeBuildResult::failure(&log_path, msg)
            }
        }
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
