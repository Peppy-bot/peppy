use super::super::action_loop::{
    GoalEnding, GoalHandler, WorkStop, accept_goal, cancel_reason, expose_daemon_action,
    invalid_payload, refuse_goal, run_action_loop, under_goal_cancel,
};
use super::FeedbackLine;
use super::gate::{COOPERATIVE_TEARDOWN_BUDGET, ConcurrencyGate};
use crate::Result;
use crate::services::raw_calls::RawCallGate;
use config::failure_kind::PeppyFailureKind;
use core_node_api::ActionId;
use core_node_api::DaemonFailureKind;
use core_node_api::encoding::{
    NodeBuildFeedback, NodeBuildGoal, NodeBuildGoalResponse, NodeBuildResult,
};
use daemon_config::consts::PeppyDirs;
use futures::FutureExt;
use node_stack::{ActionLog, Announcer, BuildContext, NodeStack};
use peppylib::MessengerHandle;
use peppylib::messaging::{Failure, PendingGoal};
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
    raw_calls: RawCallGate,
) -> Result<JoinHandle<Result<()>>> {
    let action = expose_daemon_action(
        messenger,
        core_node_name,
        instance_id,
        node_name,
        ActionId::NodeBuild,
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

    let handle = tokio::spawn(async move {
        run_action_loop(action, ActionId::NodeBuild, raw_calls, handler).await
    });
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

/// Writes the message of a failure of `kind` to the build log and returns
/// the failure, so the reason is in the file the client is pointed at.
fn record_build_failure(log: &ActionLog, kind: PeppyFailureKind, message: String) -> Failure {
    log.error(&message);
    Failure::new(kind, message)
}

/// Drives a build for the entity named `(node_name, node_tag)` that is
/// currently in `Added`. The caller supplies the log file/path and the
/// feedback channel; everything else is recovered from the entity itself.
///
/// A node the stack does not hold is `rejected`; a node in another stage, or
/// without the working directory an add stages, is `precondition_failed`.
pub(crate) async fn run_node_build_for_entity(
    node_name: String,
    node_tag: String,
    env_vars: Vec<(String, String)>,
    rebuild: bool,
    action_context: NodeBuildActionContext,
    announcer: Announcer,
) -> std::result::Result<NodeBuildResult, WorkStop> {
    let log = announcer.log().clone();
    let entity_handle = match action_context.node_stack.find(&node_name, &node_tag) {
        Some(handle) => handle,
        None => {
            return Err(WorkStop::Failed(record_build_failure(
                &log,
                PeppyFailureKind::Rejected,
                format!("node `{}:{}` is not in the node stack", node_name, node_tag),
            )));
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
            return Err(WorkStop::Failed(record_build_failure(
                &log,
                PeppyFailureKind::PreconditionFailed,
                msg,
            )));
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

impl NodeBuildGoalHandler {
    async fn handle_goal_request(&self, pending: PendingGoal) {
        let sender_instance_id = pending.instance_id().to_string();

        let goal = match NodeBuildGoal::decode(pending.request_bytes()) {
            Ok(g) => g,
            Err(e) => {
                refuse_goal(pending, invalid_payload(e)).await;
                return;
            }
        };

        // Before the gate, because the gate is per-action and this exclusion is
        // per-machine: a coordinator halfway through replacing this stack must
        // not race a locally-typed `peppy node build`.
        let admission = match self.slice_ownership.admit_node_goal(&goal) {
            Ok(admission) => admission,
            Err(refusal) => {
                refuse_goal(pending, refusal).await;
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
                refuse_goal(
                    pending,
                    Failure::new(
                        DaemonFailureKind::Busy,
                        format!(
                            "action already in progress (times out in {remaining_secs}s), \
                             use `--force` to force building the node"
                        ),
                    ),
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
                refuse_goal(
                    pending,
                    Failure::new(
                        PeppyFailureKind::Rejected,
                        format!(
                            "node `{}:{}` is not in the node stack: run `peppy node add` first",
                            goal.node_name, goal.node_tag
                        ),
                    ),
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
                refuse_goal(
                    pending,
                    Failure::new(PeppyFailureKind::PreconditionFailed, reason),
                )
                .await;
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
                refuse_goal(
                    pending,
                    Failure::new(PeppyFailureKind::AttemptFailed, error_msg),
                )
                .await;
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
                NodeBuildGoalResponse::new(&log_path).encode(),
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
            // Frees the gate slot on every exit: explicitly before the end on
            // the normal path (via `release_then_end` below), or on unwind
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
            // A build stops no instance: its cleanup kills its own build
            // command at once.
            let result = crate::services::node::gate::finish_on_reset(
                work,
                &reset_cancellation,
                COOPERATIVE_TEARDOWN_BUDGET,
                || {
                    Err(WorkStop::Cancelled(Failure::new(
                        PeppyFailureKind::AttemptFailed,
                        "node build cancelled by stack reset",
                    )))
                },
            )
            .await;
            // A build that a cancel stopped ends cancelled, with the reason
            // of the cancel: the goal's caller or its gone sender, a stack
            // reset, or a forced node build that supersedes it.
            let reason = cancel_reason(&goal_ctx, &reset_cancellation)
                .unwrap_or_else(|| "superseded by a forced node build".to_owned());
            let ending = GoalEnding::of(result.map(|built| built.encode()), Some(reason));

            let _ = consumer_handle.await;
            drop(admission);
            slot.release_then_end(&goal_ctx, ending).await;
        });

        self.gate.set_task(task_handle, cancel_token);
    }
}

/// Builds the entity of `run`, or gives the stop of the build: invalid
/// environment variables are `rejected`, an entity that a concurrent add
/// replaced is `precondition_failed`, and a build that fails is
/// `attempt_failed`. A build that the cancel of `run` stops is
/// [`WorkStop::Cancelled`].
async fn run_node_build(run: NodeBuildRun) -> std::result::Result<NodeBuildResult, WorkStop> {
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

    // Set once the entity's working dir is detached, which leaves the entity
    // in `Building`: a panic after that point rolls the entity out of the
    // stack as the normal `Err(e)` path does.
    let working_dir_detached = std::sync::atomic::AtomicBool::new(false);

    match AssertUnwindSafe(async {
        let mut env_vars = match super::validate_goal_env_vars(&goal_env_vars) {
            Ok(v) => v,
            Err(e) => {
                return Err(WorkStop::Failed(record_build_failure(
                    &log,
                    PeppyFailureKind::Rejected,
                    e.to_string(),
                )));
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
                    return Err(WorkStop::Failed(record_build_failure(
                        &log,
                        PeppyFailureKind::PreconditionFailed,
                        format!(
                            "node `{}:{}` was replaced by a concurrent push \
                             (generation {} -> {}); aborting build",
                            node_name, node_tag, captured_generation, current_gen
                        ),
                    )));
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
                Ok(NodeBuildResult::new(artifact_path))
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
                Err(WorkStop::Cancelled(record_build_failure(
                    &log,
                    PeppyFailureKind::AttemptFailed,
                    "build cancelled".to_owned(),
                )))
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
                Err(WorkStop::Failed(record_build_failure(
                    &log,
                    PeppyFailureKind::AttemptFailed,
                    format!("Failed to build node: {}", e),
                )))
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
            Err(WorkStop::Failed(Failure::new(
                PeppyFailureKind::AttemptFailed,
                msg,
            )))
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
    async fn build_entity(name: &str, tag: &str) -> std::result::Result<NodeBuildResult, Failure> {
        let dir = tempfile::tempdir().expect("temp dir");
        let root_config =
            config::node::NodeConfigParser::from_content(ROOT_CONFIG).expect("root config parses");
        let node_stack = Arc::new(NodeStack::new(root_config, None, dir.path()));
        let unstaged = config::node::NodeConfigParser::from_content(UNSTAGED_CONFIG)
            .expect("node config parses");
        node_stack
            .push_config(unstaged, Vec::new(), false, dir.path().join("unstaged"))
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
        .map_err(|stop| match stop {
            WorkStop::Failed(failure) => failure,
            WorkStop::Cancelled(failure) => panic!("nothing cancels the build: {failure}"),
        })
    }

    #[tokio::test]
    async fn a_node_in_another_stage_than_added_is_not_built() {
        let failure = build_entity("core_root", "v1")
            .await
            .expect_err("a root node is not built");
        assert_eq!(failure.kind().as_str(), "precondition_failed");
        assert_eq!(
            failure.message(),
            "node `core_root:v1` is in stage `Root`; cannot build"
        );
    }

    #[tokio::test]
    async fn a_node_without_a_staged_working_dir_is_not_built() {
        let failure = build_entity("unstaged", "v1")
            .await
            .expect_err("an unstaged node is not built");
        assert_eq!(failure.kind().as_str(), "precondition_failed");
        assert_eq!(
            failure.message(),
            "node `unstaged:v1` has no staged working directory"
        );
    }
}
