use super::super::action_loop::{GoalHandler, accept_goal, reject_goal, run_action_loop};
use super::gate::{COOPERATIVE_TEARDOWN_BUDGET, ConcurrencyGate};
use super::write_error_to_log;
use super::{FeedbackLine, FeedbackStream, create_action_log_file};
use crate::Result;
use chrono::Local;
use core_node_api::ActionId;
use core_node_api::encoding::{
    NodeBuildFeedback, NodeBuildGoal, NodeBuildGoalResponse, NodeBuildResult,
};
use core_node_api::names;
use daemon_config::consts::PeppyDirs;
use futures::FutureExt;
use node_stack::{BuildContext, NodeStack};
use parking_lot::Mutex as StdMutex;
use peppylib::messaging::SenderTarget;
use peppylib::messaging::{ConcurrentAction, PendingGoal};
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyResult};
use std::fs::File;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
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
    let action = ConcurrentAction::expose(
        messenger,
        core_node_name,
        instance_id,
        SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
        ActionId::NodeBuild.name(),
        true,
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
    /// Signaled when a `--force` build supersedes this one. Threaded into the
    /// build I/O so the subprocess is SIGKILL'd + reaped, and consulted after
    /// the build to roll the entity back to `Added` (re-attaching the working
    /// dir) rather than removing it.
    cancel_token: CancellationToken,
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
        // This helper drives a fresh build that nothing supersedes.
        cancel_token: CancellationToken::new(),
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
    async fn handle_goal(&self, pending: PendingGoal) {
        self.handle_goal_request(pending).await
    }
}

fn encode_rejected_goal(reason: impl Into<String>) -> PeppyResult<Payload> {
    super::encode_response_or_err(
        "node_build_goal",
        NodeBuildGoalResponse::rejected(reason).encode(),
    )
}

impl NodeBuildGoalHandler {
    async fn handle_goal_request(&self, pending: PendingGoal) {
        let sender_instance_id = pending.instance_id().to_string();

        let goal = match NodeBuildGoal::decode(pending.request_bytes()) {
            Ok(g) => g,
            Err(e) => {
                reject_goal(
                    pending,
                    encode_rejected_goal(format!("invalid payload: {e}")),
                )
                .await;
                return;
            }
        };

        if goal.force {
            debug!("Force flag set: superseding any previous node_build task");
        }
        let (generation, superseded) = match self.gate.try_admit(goal.timeout_secs, goal.force) {
            super::gate::Admission::Admitted {
                generation,
                superseded,
            } => (generation, superseded),
            super::gate::Admission::AlreadyRunning { remaining_secs } => {
                reject_goal(
                    pending,
                    encode_rejected_goal(format!(
                        "action already in progress (times out in {remaining_secs}s), \
                         use `--force` to force building the node"
                    )),
                )
                .await;
                return;
            }
        };

        // Drive the superseded build's cooperative teardown to completion before
        // re-reading the entity below. Its cancel token was already signaled in
        // `try_admit`, so awaiting the handle lets it SIGKILL + reap the build
        // child and roll the entity back to `Added` with its working dir
        // re-attached, leaving it buildable for this goal. Bounded so a wedged
        // old task cannot stall the forced rebuild forever; on timeout the stage
        // re-read rejects transiently rather than wedging permanently.
        if let Some(old_task) = superseded {
            let _ = tokio::time::timeout(COOPERATIVE_TEARDOWN_BUDGET, old_task).await;
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
                self.gate.clear_running();
                reject_goal(
                    pending,
                    encode_rejected_goal(format!(
                        "node `{}:{}` is not in the node stack: run `peppy node add` first",
                        goal.node_name, goal.node_tag
                    )),
                )
                .await;
                return;
            }
        };

        let buildable = {
            let guard = entity_handle.read();
            match guard.stage().ensure_buildable() {
                Ok(()) => Ok((guard.pending_working_dir(), guard.generation())),
                Err(stage) => Err(stage.to_string()),
            }
        };
        let (working_dir_guard, captured_generation) = match buildable {
            Err(stage) => {
                self.gate.clear_running();
                reject_goal(
                    pending,
                    encode_rejected_goal(format!(
                        "node `{}:{}` is in stage `{}`; cannot build",
                        goal.node_name, goal.node_tag, stage
                    )),
                )
                .await;
                return;
            }
            Ok((None, _)) => {
                self.gate.clear_running();
                reject_goal(
                    pending,
                    encode_rejected_goal(format!(
                        "node `{}:{}` has no staged working directory; \
                         re-run `peppy node add` to stage one",
                        goal.node_name, goal.node_tag
                    )),
                )
                .await;
                return;
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
                self.gate.clear_running();
                reject_goal(pending, encode_rejected_goal(error_msg)).await;
                return;
            }
        };
        debug!("Build log file: {}", log_path.display());

        // `accept` registers the per-goal context before replying accepted.
        let Some(goal_ctx) = accept_goal(
            pending,
            super::encode_response_or_err(
                "node_build_goal",
                NodeBuildGoalResponse::accepted(&log_path).encode(),
            ),
        )
        .await
        else {
            self.gate.clear_running();
            return;
        };

        let feedback_publisher = goal_ctx
            .feedback_publisher()
            .expect("node_build declares a feedback topic");
        let action_context = self.context.clone();
        let log_path_clone = log_path.clone();
        // Stored in the gate so a later `--force` goal can signal it, and threaded
        // into the build so `run_node_build` owns cancellation end-to-end: it
        // SIGKILLs + reaps the build child and rolls the entity back to `Added`
        // (re-attaching the working dir) instead of being `abort()`ed mid-flight.
        let cancel_token = CancellationToken::new();
        let cancel_token_for_task = cancel_token.clone();
        let gate_for_task = self.gate.clone();

        let task_handle = tokio::spawn(async move {
            // Frees the gate slot on every exit: explicitly before completion on
            // the normal path (via `release_then_complete` below), or on unwind
            // for a panic. A no-op if a later `--force` goal already took over.
            let slot = gate_for_task.into_slot_guard(generation);
            let (feedback_tx, feedback_rx) = mpsc::unbounded_channel::<FeedbackLine>();
            let consumer_handle =
                super::spawn_feedback_forwarder(feedback_rx, feedback_publisher, |line| {
                    NodeBuildFeedback::from_stream(line.stream, &line.line).encode()
                });

            let result = run_node_build(NodeBuildRun {
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
                cancel_token: cancel_token_for_task,
            })
            .await;

            let _ = consumer_handle.await;
            if let Ok(payload) = result.encode() {
                slot.release_then_complete(&goal_ctx, payload).await;
            }
        });

        self.gate.set_task(task_handle, cancel_token);
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
        cancel_token,
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
                cancel_token: cancel_token.clone(),
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
            Err(_) if cancel_token.is_cancelled() => {
                // Superseded by a `--force` build (the build I/O returned a
                // cancellation error after SIGKILL'ing + reaping the child).
                // Roll the entity back to `Added` and re-attach the staged
                // working dir so the forced rebuild can reuse it, the only
                // surviving copy of the source. (Removing it, as the genuine
                // failure path does, would delete the working dir and make the
                // rebuild impossible.)
                let _ = action_context.node_stack.rollback_to_added_if_matches(
                    &node_name,
                    &node_tag,
                    &entity_handle,
                    expected_generation,
                    Arc::clone(&working_dir_guard),
                );
                NodeBuildResult::failure(&log_path, "build cancelled by --force".to_string())
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
            // `Building` state; roll it out so it doesn't stay stuck.
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
