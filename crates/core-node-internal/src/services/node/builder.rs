use super::super::action_loop::{ActionResult, ActionState, GoalHandler, run_action_loop};
use super::gate::ConcurrencyGate;
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
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
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
        gate: ConcurrencyGate::new(),
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

/// Inputs required to drive a single `node_build` run to completion.
struct NodeBuildRun {
    node_name: String,
    node_tag: String,
    env_vars: Vec<(String, String)>,
    entity_handle: node_stack::EntityHandle,
    working_dir_guard: Arc<node_stack::WorkingDirGuard>,
    captured_generation: u64,
    action_context: NodeBuildActionContext,
    feedback_tx: mpsc::UnboundedSender<FeedbackLine>,
    log_file: Arc<StdMutex<File>>,
    log_path: PathBuf,
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

    let (working_dir_guard, captured_generation) = {
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
            Some(g) => (g, guard.generation()),
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

    run_node_build(NodeBuildRun {
        node_name,
        node_tag,
        env_vars,
        entity_handle,
        working_dir_guard,
        captured_generation,
        action_context,
        feedback_tx,
        log_file,
        log_path,
    })
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
    gate: ConcurrencyGate,
}

impl GoalHandler for NodeBuildGoalHandler {
    type Result = NodeBuildResult;

    async fn handle_goal(
        &self,
        context: ServiceRequestContext,
        feedback_publisher: TopicPublisher,
        state: Arc<Mutex<ActionState<NodeBuildResult>>>,
    ) -> PeppyResult<Payload> {
        self.handle_goal_request(context, feedback_publisher, state)
            .await
    }
}

fn encode_rejected_goal(reason: impl Into<String>) -> PeppyResult<Payload> {
    super::encode_response_or_err(
        "node_build_goal",
        NodeBuildGoalResponse::rejected(reason).encode(),
    )
}

impl NodeBuildGoalHandler {
    async fn handle_goal_request(
        &self,
        context: ServiceRequestContext,
        feedback_publisher: TopicPublisher,
        state: Arc<Mutex<ActionState<NodeBuildResult>>>,
    ) -> PeppyResult<Payload> {
        let sender_instance_id = context.message().instance_id();
        let payload = context.message().payload();

        let goal = match NodeBuildGoal::decode(payload.as_ref()) {
            Ok(g) => g,
            Err(e) => return encode_rejected_goal(format!("invalid payload: {}", e)),
        };

        {
            let mut state_guard = state.lock().await;
            if goal.force && matches!(*state_guard, ActionState::Running { .. }) {
                debug!("Force flag set: aborting previous node_build task");
            }
            if let super::gate::Admission::AlreadyRunning { remaining_secs } =
                self.gate
                    .try_admit(&mut state_guard, goal.timeout_secs, goal.force)
            {
                return encode_rejected_goal(format!(
                    "action already in progress (times out in {remaining_secs}s), \
                     use `--force` to force building the node"
                ));
            }
        }

        debug!(
            "Received `node_build` goal from {sender_instance_id}, target={}:{}",
            goal.node_name, goal.node_tag
        );

        let entity_handle = match self
            .context
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

        let pending = {
            let guard = entity_handle.read();
            match guard.stage().ensure_buildable() {
                Ok(()) => Ok((guard.pending_working_dir(), guard.generation())),
                Err(stage) => Err(stage.to_string()),
            }
        };
        let (working_dir_guard, captured_generation) = match pending {
            Err(stage) => {
                let mut state_guard = state.lock().await;
                *state_guard = ActionState::Rejected;
                return encode_rejected_goal(format!(
                    "node `{}:{}` is in stage `{}`; cannot build",
                    goal.node_name, goal.node_tag, stage
                ));
            }
            Ok((None, _)) => {
                let mut state_guard = state.lock().await;
                *state_guard = ActionState::Rejected;
                return encode_rejected_goal(format!(
                    "node `{}:{}` has no staged working directory; \
                     re-run `peppy node add` to stage one",
                    goal.node_name, goal.node_tag
                ));
            }
            Ok((Some(g), generation)) => (g, generation),
        };

        let log_dir = self.context.peppy_dirs.logs_dir_build();
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
        let action_context = self.context.clone();
        let log_path_clone = log_path.clone();
        let cancel_token = CancellationToken::new();
        let cancel_token_clone = cancel_token.clone();

        // Clones for the cancellation-cleanup branch of `select!`. When a
        // `--force` build aborts the in-flight task, the entity may already
        // be in `Building` state. `remove_config_if_matches` rolls it back
        // so that future builds are not permanently rejected.
        let node_stack_for_cancel = Arc::clone(&self.context.node_stack);
        let node_name_for_cancel = goal.node_name.clone();
        let node_tag_for_cancel = goal.node_tag.clone();
        let entity_handle_for_cancel = entity_handle.clone();
        let generation_for_cancel = captured_generation;
        let log_path_for_cancel = log_path.clone();

        let task_handle = tokio::spawn(async move {
            let (feedback_tx, feedback_rx) = mpsc::unbounded_channel::<FeedbackLine>();
            let consumer_handle =
                super::spawn_feedback_forwarder(feedback_rx, feedback_publisher.clone(), |line| {
                    NodeBuildFeedback::from_stream(line.stream, &line.line).encode()
                });

            let result = tokio::select! {
                biased;
                result = run_node_build(NodeBuildRun {
                    node_name: goal.node_name,
                    node_tag: goal.node_tag,
                    env_vars: goal.env_vars,
                    entity_handle,
                    working_dir_guard,
                    captured_generation,
                    action_context,
                    feedback_tx,
                    log_file,
                    log_path: log_path_clone,
                }) => result,
                _ = cancel_token_clone.cancelled() => {
                    let _ = node_stack_for_cancel.remove_config_if_matches(
                        &node_name_for_cancel,
                        &node_tag_for_cancel,
                        &entity_handle_for_cancel,
                        generation_for_cancel,
                    );
                    NodeBuildResult::failure(
                        &log_path_for_cancel,
                        "build cancelled by --force".to_string(),
                    )
                }
            };

            let _ = consumer_handle.await;
            let mut state_guard = state_clone.lock().await;
            *state_guard = ActionState::Completed { result };
        });

        self.gate.set_task(task_handle, cancel_token);

        super::encode_response_or_err(
            "node_build_goal",
            NodeBuildGoalResponse::accepted(&log_path).encode(),
        )
    }
}

async fn run_node_build(run: NodeBuildRun) -> NodeBuildResult {
    let NodeBuildRun {
        node_name,
        node_tag,
        env_vars: goal_env_vars,
        entity_handle,
        working_dir_guard,
        captured_generation,
        action_context,
        feedback_tx,
        log_file,
        log_path,
    } = run;

    let log_file_for_panic = log_file.clone();
    let log_path_for_panic = log_path.clone();

    // Clones for panic-handler entity cleanup. After
    // `take_pending_working_dir_if_generation` succeeds the entity is in
    // `Building` state; if the async block panics we must roll it out of the
    // stack the same way the normal `Err(e)` path does.
    let node_stack_for_panic = Arc::clone(&action_context.node_stack);
    let node_name_for_panic = node_name.clone();
    let node_tag_for_panic = node_tag.clone();
    let entity_handle_for_panic = entity_handle.clone();
    let generation_for_panic = captured_generation;
    let working_dir_detached = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let working_dir_detached_for_panic = Arc::clone(&working_dir_detached);

    match AssertUnwindSafe(async {
        let mut env_vars = match super::validate_goal_env_vars(&goal_env_vars) {
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
        super::inject_node_runtime_env(&mut env_vars, &node_name, &node_tag);

        // Atomically detach the entity-side working dir AND verify the
        // generation hasn't changed since Phase 1. If a concurrent
        // push_config_impl replaced the entity in-place, the generation
        // will differ and we must abort to avoid building from a stale
        // working dir while holding the new entity's generation.
        {
            let mut guard = entity_handle.write();
            match guard.take_pending_working_dir_if_generation(captured_generation) {
                Ok(_) => {
                    working_dir_detached.store(true, std::sync::atomic::Ordering::Release);
                }
                Err(current_gen) => {
                    let msg = format!(
                        "node `{}:{}` was replaced by a concurrent push \
                         (generation {} -> {}); aborting build",
                        node_name, node_tag, captured_generation, current_gen
                    );
                    write_error_to_log(&log_file, &msg);
                    return NodeBuildResult::failure(&log_path, msg);
                }
            }
        }

        let working_dir_path = working_dir_guard.path().to_path_buf();
        let expected_generation = captured_generation;

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
                    node_name,
                    node_tag,
                    artifact_path.display()
                );
                NodeBuildResult::success(artifact_path, &log_path)
            }
            Err(e) => {
                // `NodeEntity::build` leaves the entity in `Building` on
                // failure. Roll it out of the stack so the user can re-add.
                let _ = action_context.node_stack.remove_config_if_matches(
                    &node_name,
                    &node_tag,
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
            // If the working dir was already detached the entity is in
            // `Building` state — roll it out so it doesn't stay stuck.
            if working_dir_detached_for_panic.load(std::sync::atomic::Ordering::Acquire) {
                let _ = node_stack_for_panic.remove_config_if_matches(
                    &node_name_for_panic,
                    &node_tag_for_panic,
                    &entity_handle_for_panic,
                    generation_for_panic,
                );
            }
            NodeBuildResult::failure(log_path_for_panic, msg)
        }
    }
}
