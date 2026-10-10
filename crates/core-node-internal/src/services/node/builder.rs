use super::super::action_loop::{
    GoalHandler, accept_goal, reject_goal, run_action_loop, under_goal_cancel,
};
use super::FeedbackLine;
use super::gate::{COOPERATIVE_TEARDOWN_BUDGET, ConcurrencyGate};
use crate::Result;
use core_node_api::ActionId;
use core_node_api::encoding::{
    NodeBuildFeedback, NodeBuildGoal, NodeBuildGoalResponse, NodeBuildResult,
};
use core_node_api::names;
use daemon_config::consts::PeppyDirs;
use futures::FutureExt;
use node_stack::{ActionLog, Announcer, BuildContext, NodeStack};
use peppylib::messaging::SenderTarget;
use peppylib::messaging::{ConcurrentAction, PendingGoal};
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyResult};
use std::panic::AssertUnwindSafe;
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
    slice_ownership: Arc<crate::services::federation::SliceOwnership>,
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
        slice_ownership,
    };

    let handle = tokio::spawn(async move { run_action_loop(action, handler).await });
    Ok(handle)
}

/// Why an entity has nothing to build.
enum Unbuildable {
    /// Its stage admits no build; carries the stage's name.
    Stage(&'static str),
    /// It holds no working directory staged by an add.
    NoWorkingDir,
}

/// The staged working dir of `entity` and the generation it was read at,
/// when its stage admits a build. Both come from one read guard, so the
/// generation is the one the working dir belongs to.
fn buildable_working_dir(
    entity: &node_stack::EntityHandle,
) -> std::result::Result<(Arc<node_stack::WorkingDirGuard>, u64), Unbuildable> {
    let guard = entity.read();
    guard
        .stage()
        .ensure_buildable()
        .map_err(Unbuildable::Stage)?;
    let working_dir = guard
        .pending_working_dir()
        .ok_or(Unbuildable::NoWorkingDir)?;
    Ok((working_dir, guard.generation()))
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
    announcer: Announcer,
    /// Signaled when a `--force` build supersedes this one. Threaded into the
    /// build I/O so the subprocess is SIGKILL'd + reaped, and consulted after
    /// the build to roll the entity back to `Added` (re-attaching the working
    /// dir) rather than removing it.
    cancel_token: CancellationToken,
    /// See [`NodeBuildGoal::rebuild`].
    rebuild: bool,
}

/// Drives a build for the entity named `(node_name, node_tag)` that is
/// currently in `Added`. The caller supplies the log file/path and the
/// feedback channel; everything else is recovered from the entity itself.
pub(crate) async fn run_node_build_for_entity(
    node_name: String,
    node_tag: String,
    env_vars: Vec<(String, String)>,
    rebuild: bool,
    action_context: NodeBuildActionContext,
    announcer: Announcer,
) -> NodeBuildResult {
    let log = announcer.log().clone();
    let log_path = log.path();
    let entity_handle = match action_context.node_stack.find(&node_name, &node_tag) {
        Some(handle) => handle,
        None => {
            let msg = format!("node `{}:{}` is not in the node stack", node_name, node_tag);
            log.error(&msg);
            return NodeBuildResult::failure(&log_path, msg);
        }
    };

    let (working_dir_guard, captured_generation) = match buildable_working_dir(&entity_handle) {
        Ok(buildable) => buildable,
        Err(unbuildable) => {
            let msg = match unbuildable {
                Unbuildable::Stage(stage) => format!(
                    "node `{}:{}` is in stage `{}`; cannot build",
                    node_name, node_tag, stage
                ),
                Unbuildable::NoWorkingDir => format!(
                    "node `{}:{}` has no staged working directory",
                    node_name, node_tag
                ),
            };
            log.error(&msg);
            return NodeBuildResult::failure(&log_path, msg);
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
        announcer,
        // This helper drives a fresh build that nothing supersedes.
        cancel_token: CancellationToken::new(),
        rebuild,
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
    /// See `NodeAddGoalHandler::slice_ownership`.
    slice_ownership: Arc<crate::services::federation::SliceOwnership>,
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

        // Before the gate, because the gate is per-action and this exclusion is
        // per-machine: a coordinator halfway through replacing this stack must
        // not race a locally-typed `peppy node build`.
        let admission = match self.slice_ownership.admit_node_goal(&goal) {
            Ok(admission) => admission,
            Err(reason) => {
                reject_goal(pending, encode_rejected_goal(reason)).await;
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

        let (working_dir_guard, captured_generation) = match buildable_working_dir(&entity_handle) {
            Ok(buildable) => buildable,
            Err(unbuildable) => {
                let reason = match unbuildable {
                    Unbuildable::Stage(stage) => format!(
                        "node `{}:{}` is in stage `{}`; cannot build",
                        goal.node_name, goal.node_tag, stage
                    ),
                    Unbuildable::NoWorkingDir => format!(
                        "node `{}:{}` has no staged working directory; \
                         re-run `peppy node add` to stage one",
                        goal.node_name, goal.node_tag
                    ),
                };
                self.gate.clear_running();
                reject_goal(pending, encode_rejected_goal(reason)).await;
                return;
            }
        };

        let log = match ActionLog::for_build(
            &self.context.peppy_dirs,
            self.context.node_stack.log_exporter().clone(),
            &goal.node_name,
            &goal.node_tag,
            goal.launch_id.as_deref(),
        ) {
            Ok(log) => log,
            Err(error_msg) => {
                debug!("{}", error_msg);
                self.gate.clear_running();
                reject_goal(pending, encode_rejected_goal(error_msg)).await;
                return;
            }
        };
        let log_path = log.path();
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
        // Stored in the gate so a later `--force` goal can signal it, signalled
        // by the goal's caller through its cancel request, and threaded into
        // the build so `run_node_build` owns cancellation end-to-end: it
        // SIGKILLs + reaps the build child and rolls the entity back to `Added`
        // (re-attaching the working dir).
        let reset_cancellation = self.slice_ownership.stack.cancellation();
        let cancel_token = reset_cancellation.child_token();
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

            let work = run_node_build(NodeBuildRun {
                node_name: goal.node_name,
                node_tag: goal.node_tag,
                env_vars: goal.env_vars,
                entity_handle,
                working_dir_guard,
                captured_generation,
                action_context,
                announcer: Announcer::new(log, feedback_tx),
                cancel_token: cancel_token_for_task.clone(),
                rebuild: goal.rebuild,
            });
            let work = under_goal_cancel(&goal_ctx, &cancel_token_for_task, work);
            let result =
                crate::services::node::gate::finish_on_reset(work, &reset_cancellation, || {
                    NodeBuildResult::failure(
                        &log_path_clone,
                        "node build cancelled by stack reset".to_owned(),
                    )
                })
                .await;

            let _ = consumer_handle.await;
            drop(admission);
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
        announcer,
        cancel_token,
        rebuild,
    } = run;

    let log = announcer.log().clone();
    let log_path = log.path();

    // Set once the entity's working dir is detached, which leaves the entity
    // in `Building`: a panic after that point rolls the entity out of the
    // stack as the normal `Err(e)` path does.
    let working_dir_detached = std::sync::atomic::AtomicBool::new(false);

    match AssertUnwindSafe(async {
        let mut env_vars = match super::validate_goal_env_vars(&goal_env_vars) {
            Ok(v) => v,
            Err(e) => {
                let msg = e.to_string();
                log.error(&msg);
                return NodeBuildResult::failure(&log_path, msg);
            }
        };

        let language = entity_handle.read().config().execution.language;
        let sccache_injected = super::inject_rust_build_env(&mut env_vars, language);
        if sccache_injected {
            announcer.line("Using sccache for Rust compilation");
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
                    log.error(&msg);
                    return NodeBuildResult::failure(&log_path, msg);
                }
            }
        }

        let working_dir_path = working_dir_guard.path().to_path_buf();

        let build_result = node_stack::NodeEntity::build(
            &entity_handle,
            BuildContext {
                working_dir: &working_dir_path,
                peppy_dirs: &action_context.peppy_dirs,
                announcer: announcer.clone(),
                env_vars: &env_vars,
                cancel_token: cancel_token.clone(),
                rebuild,
                pypi_mirror: action_context.node_stack.pypi_mirror(),
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
                // Cancelled: superseded by a `--force` build or cancelled by
                // the goal's caller (the build I/O returned a cancellation
                // error after SIGKILL'ing + reaping the child). Roll the
                // entity back to `Added` and re-attach the staged working
                // dir, the only surviving copy of the source, so a rebuild
                // can reuse it.
                let _ = action_context.node_stack.rollback_to_added_if_matches(
                    &node_name,
                    &node_tag,
                    &entity_handle,
                    captured_generation,
                    Arc::clone(&working_dir_guard),
                );
                NodeBuildResult::failure(&log_path, "build cancelled".to_string())
            }
            Err(e) => {
                // `NodeEntity::build` leaves the entity in `Building` on
                // failure. Roll it out of the stack so the user can re-add.
                let _ = action_context.node_stack.remove_config_if_matches(
                    &node_name,
                    &node_tag,
                    &entity_handle,
                    captured_generation,
                );
                let msg = format!("Failed to build node: {}", e);
                log.error(&msg);
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
            log.error(&msg);
            // If the working dir was already detached the entity is in
            // `Building` state; roll it out so it doesn't stay stuck.
            if working_dir_detached.load(std::sync::atomic::Ordering::Acquire) {
                let _ = action_context.node_stack.remove_config_if_matches(
                    &node_name,
                    &node_tag,
                    &entity_handle,
                    captured_generation,
                );
            }
            NodeBuildResult::failure(log.path(), msg)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use log_export::{LogExporter, LogKind};

    const ROOT_CONFIG: &str = r#"{
        peppy_schema: "node/v1",
        manifest: { name: "core_root", tag: "v1" },
        execution: { language: "rust", run_cmd: ["true"] },
    }"#;

    const UNSTAGED_CONFIG: &str = r#"{
        peppy_schema: "node/v1",
        manifest: { name: "unstaged", tag: "v1" },
        execution: { language: "rust", build_cmd: ["true"], run_cmd: ["true"] },
    }"#;

    /// Runs a build of `name:tag` in a stack rooted at `core_root:v1` that
    /// also holds `unstaged:v1`, pushed without the working dir an add
    /// stages for it.
    async fn build_entity(name: &str, tag: &str) -> NodeBuildResult {
        let dir = tempfile::tempdir().expect("temp dir");
        let root_config =
            config::node::NodeConfigParser::from_content(ROOT_CONFIG).expect("root config parses");
        let node_stack = Arc::new(NodeStack::new(root_config, None, dir.path()));
        let unstaged = config::node::NodeConfigParser::from_content(UNSTAGED_CONFIG)
            .expect("node config parses");
        node_stack
            .push_config(unstaged, false, dir.path().join("unstaged"))
            .expect("push the unstaged node");
        let (exporter, _records) = LogExporter::channel(None);
        let log = ActionLog::create(dir.path(), "build.log", LogKind::Build, exporter)
            .expect("create the build log");
        let (feedback_tx, _feedback_rx) = mpsc::unbounded_channel();
        let action_context = NodeBuildActionContext {
            node_stack,
            peppy_dirs: PeppyDirs::new(dir.path()),
        };
        run_node_build_for_entity(
            name.to_owned(),
            tag.to_owned(),
            Vec::new(),
            false,
            action_context,
            Announcer::new(log, feedback_tx),
        )
        .await
    }

    #[tokio::test]
    async fn a_node_in_another_stage_than_added_is_not_built() {
        let result = build_entity("core_root", "v1").await;
        assert!(!result.success);
        assert_eq!(
            result.error_message.as_deref(),
            Some("node `core_root:v1` is in stage `Root`; cannot build")
        );
    }

    #[tokio::test]
    async fn a_node_without_a_staged_working_dir_is_not_built() {
        let result = build_entity("unstaged", "v1").await;
        assert!(!result.success);
        assert_eq!(
            result.error_message.as_deref(),
            Some("node `unstaged:v1` has no staged working directory")
        );
    }
}
