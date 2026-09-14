//! The actions that change a stack: the goal a caller sends, the admission it
//! passes through, and the context the change it asked for runs under.

use super::copies::join::join;
use super::copies::remove::remove;
use super::launch::process_launch;
use crate::Result;
use crate::services::action_loop::{GoalHandler, accept_goal, reject_goal, run_action_loop};
use crate::services::node::common::panic_message;
use crate::services::node::gate::{Admission, ConcurrencyGate, finish_on_reset};
use crate::services::node::{DaemonDefaults, HealthMonitorPolicy, RelationshipCoordinators};
use chrono::Local;
use core_node_api::ActionId;
use core_node_api::encoding::{
    LaunchGoal, LaunchGoalResponse, LaunchResult, StackBudgets, StackJoinGoal, StackRemoveGoal,
};
use core_node_api::names;
use daemon_config::consts::PeppyDirs;
use futures::FutureExt;
use node_stack::NodeStack;
use parking_lot::Mutex as StdMutex;
use peppylib::messaging::SenderTarget;
use peppylib::messaging::{ActionFeedbackPublisher, ConcurrentAction, PendingGoal};
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyResult};
use std::fs::File;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::debug;

/// The three actions that change a stack, each answered by one listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::stack) enum StackAction {
    Launch,
    Join,
    Remove,
}

impl TryFrom<ActionId> for StackAction {
    type Error = String;

    fn try_from(id: ActionId) -> std::result::Result<Self, String> {
        match id {
            ActionId::StackLaunch => Ok(Self::Launch),
            ActionId::StackJoin => Ok(Self::Join),
            ActionId::StackRemove => Ok(Self::Remove),
            other => Err(format!("`{}` is not a stack action", other.name())),
        }
    }
}

impl StackAction {
    fn id(self) -> ActionId {
        match self {
            Self::Launch => ActionId::StackLaunch,
            Self::Join => ActionId::StackJoin,
            Self::Remove => ActionId::StackRemove,
        }
    }

    /// How the operator's feedback and the daemon's log name the action.
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Launch => "launch",
            Self::Join => "join",
            Self::Remove => "remove",
        }
    }
}

/// One caller's decoded goal, of whichever action received it.
enum StackRequest {
    Launch(LaunchGoal),
    Join(StackJoinGoal),
    Remove(StackRemoveGoal),
}

impl StackRequest {
    fn decode(action: StackAction, data: &[u8]) -> core_node_api::Result<Self> {
        match action {
            StackAction::Launch => LaunchGoal::decode(data).map(Self::Launch),
            StackAction::Join => StackJoinGoal::decode(data).map(Self::Join),
            StackAction::Remove => StackRemoveGoal::decode(data).map(Self::Remove),
        }
    }

    /// The budgets the request runs under; a removal adds no node and runs
    /// under the defaults.
    fn budgets(&self) -> StackBudgets {
        match self {
            Self::Launch(goal) => goal.budgets.clone(),
            Self::Join(goal) => goal.budgets.clone(),
            Self::Remove(_) => StackBudgets::default(),
        }
    }

    fn process(self, ctx: StackChangeContext) -> futures::future::BoxFuture<'static, LaunchResult> {
        match self {
            Self::Launch(goal) => process_launch(goal, ctx).boxed(),
            Self::Join(goal) => join(goal, ctx).boxed(),
            Self::Remove(goal) => remove(goal, ctx).boxed(),
        }
    }
}

/// Per-phase idle timeouts, sourced from the goal's budgets. Each phase's clock resets
/// only on genuine subprocess/git/http activity (see `spawn_feedback_forwarder`).
#[derive(Clone, Copy)]
pub(super) struct IdleTimeouts {
    pub(super) add: Duration,
    pub(super) build: Duration,
    pub(super) run: Duration,
}

impl IdleTimeouts {
    fn of(budgets: &StackBudgets) -> Self {
        Self {
            add: Duration::from_secs(budgets.node_add_idle_timeout_secs),
            build: Duration::from_secs(budgets.node_build_idle_timeout_secs),
            run: Duration::from_secs(budgets.node_run_idle_timeout_secs),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct StackChangeTimeouts {
    pub node_startup: Duration,
    pub node_start_health: Duration,
    pub health_monitor: HealthMonitorPolicy,
}

/// Daemon-wide defaults the stack launcher applies to every spawned
/// instance. Pairs with launcher overrides (`FrameworkOverrides`) and the
/// per-instance resolved values (`ResolvedFramework`); this struct is the
/// "what the daemon would pick when the instance omits the override" half.
pub(crate) struct StackChangeDefaults {
    pub timeouts: StackChangeTimeouts,
    /// Daemon-resolved defaults (messaging mode, subscriber buffers, liveness
    /// grace, and the `use_sim_time` default) injected into every launched
    /// node. The launch never resolves `use_sim_time` itself: it can place a
    /// node on a machine whose default differs, so the resolution belongs to
    /// whichever daemon spawns the node.
    pub daemon_defaults: DaemonDefaults,
    /// Daemon-shutdown signal, forwarded to each launched node's health monitor
    /// so it stops probing the instant a clean shutdown begins.
    pub shutdown_token: CancellationToken,
    /// Which launch this daemon's slice belongs to. Shared with the federation
    /// endpoints so a reservation and the slice it produces are one authority.
    pub slice_ownership: Arc<crate::services::federation::SliceOwnership>,
    /// This daemon's peppy version, compared against each participant's during
    /// a federated preflight.
    pub peppy_version: String,
}

#[allow(clippy::too_many_arguments)] // Mirrors the other listeners' identity args + two shared handles.
pub(crate) async fn listen_for_stack_action(
    action_id: ActionId,
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    peppy_dirs: PeppyDirs,
    defaults: StackChangeDefaults,
    relationships: RelationshipCoordinators,
) -> Result<JoinHandle<Result<()>>> {
    let stack_action = StackAction::try_from(action_id).map_err(|reason| {
        peppylib::PeppyError::InvalidServiceRequest {
            identifier: action_id.name().to_string(),
            reason,
        }
    })?;
    let action = ConcurrentAction::expose(
        messenger,
        core_node_name,
        instance_id,
        SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
        action_id.name(),
        true,
    )
    .await?;

    let StackChangeDefaults {
        timeouts,
        daemon_defaults,
        shutdown_token,
        slice_ownership,
        peppy_version,
    } = defaults;
    let handler = StackActionHandler {
        action: stack_action,
        context: StackActionContext {
            node_stack,
            messenger: messenger.clone(),
            bound_core_node: core_node_name.to_string(),
            core_instance_id: instance_id.to_string(),
            peppy_dirs,
            timeouts,
            slice_ownership,
            peppy_version,
            daemon_defaults,
            shutdown_token,
            relationships,
        },
        gate: ConcurrencyGate::new(),
    };

    let handle = tokio::spawn(async move { run_action_loop(action, handler).await });

    Ok(handle)
}

/// What one stack action's listener holds between goals.
#[derive(Clone)]
struct StackActionContext {
    node_stack: Arc<NodeStack>,
    messenger: MessengerHandle,
    bound_core_node: String,
    core_instance_id: String,
    peppy_dirs: PeppyDirs,
    timeouts: StackChangeTimeouts,
    daemon_defaults: DaemonDefaults,
    shutdown_token: CancellationToken,
    relationships: RelationshipCoordinators,
    slice_ownership: Arc<crate::services::federation::SliceOwnership>,
    peppy_version: String,
}

#[derive(Clone)]
struct StackActionHandler {
    action: StackAction,
    context: StackActionContext,
    gate: ConcurrencyGate,
}

impl GoalHandler for StackActionHandler {
    async fn handle_goal(&self, pending: PendingGoal) {
        handle_stack_request(
            pending,
            self.context.clone(),
            self.gate.clone(),
            self.action,
        )
        .await
    }
}

/// Names the action whose response could not be encoded.
fn encoding_failure(action: StackAction, error: impl std::fmt::Display) -> peppylib::PeppyError {
    peppylib::PeppyError::InvalidServiceRequest {
        identifier: action.id().name().to_string(),
        reason: format!("Failed to encode response: {error}"),
    }
}

fn encode_rejected(action: StackAction, reason: impl Into<String>) -> PeppyResult<Payload> {
    LaunchGoalResponse::rejected(reason)
        .encode()
        .map_err(|e| encoding_failure(action, e))
}

fn encode_accepted(action: StackAction, log_path: &Path) -> PeppyResult<Payload> {
    LaunchGoalResponse::accepted(log_path)
        .encode()
        .map_err(|e| encoding_failure(action, e))
}

/// The context every stack change runs under: who asked, where it writes, and
/// the budgets and daemon authorities its phases work within.
pub(super) struct StackChangeContext {
    /// Which action the change answers, so its feedback names itself.
    pub(super) action: StackAction,
    pub(super) cancellation: CancellationToken,
    pub(super) messenger: MessengerHandle,
    pub(super) bound_core_node: String,
    pub(super) core_instance_id: String,
    pub(super) node_stack: Arc<NodeStack>,
    pub(super) peppy_dirs: PeppyDirs,
    pub(super) feedback_publisher: ActionFeedbackPublisher,
    pub(super) log_file: Arc<StdMutex<File>>,
    pub(super) log_path: PathBuf,
    /// The caller's forwarded environment. It describes the machine the change
    /// was typed on, so it reaches goals executed on this daemon and stays off
    /// every goal dispatched to a peer.
    pub(super) env_vars: Vec<(String, String)>,
    pub(super) timeouts: StackChangeTimeouts,
    /// Whole-change deadline. `None` means the user did not opt into a max; only idle timeouts
    /// are enforced.
    pub(super) change_deadline: Option<Instant>,
    pub(super) idle_timeouts: IdleTimeouts,
    /// Daemon-resolved defaults (messaging mode, subscriber buffers, liveness grace)
    /// injected into every launched node.
    pub(super) daemon_defaults: DaemonDefaults,
    /// Daemon-shutdown signal, forwarded to each launched node's health monitor.
    pub(super) shutdown_token: CancellationToken,
    /// The daemon authorities forwarded into each instance's relationship
    /// lifecycle work.
    pub(super) relationships: RelationshipCoordinators,
    /// Which launch this daemon's slice belongs to. Recorded once the launch
    /// commits, so `stack list` reports it and a coordinator can rediscover
    /// every participant by query.
    pub(super) slice_ownership: Arc<crate::services::federation::SliceOwnership>,
    /// This daemon's peppy version, compared against each participant's during
    /// preflight so a mixed-version federation is refused before any stack is
    /// touched.
    pub(super) peppy_version: String,
}

async fn handle_stack_request(
    pending: PendingGoal,
    action_context: StackActionContext,
    gate: ConcurrencyGate,
    action: StackAction,
) {
    let sender_instance_id = pending.instance_id().to_string();

    // Decode the goal before admission so we can capture the user-supplied timeouts.
    let goal = match StackRequest::decode(action, pending.request_bytes()) {
        Ok(g) => g,
        Err(e) => {
            reject_goal(
                pending,
                encode_rejected(action, format!("invalid payload: {e}")),
            )
            .await;
            return;
        }
    };

    let mutation = match action_context.slice_ownership.stack.try_begin_change() {
        Ok(mutation) => mutation,
        Err(busy) => {
            reject_goal(pending, encode_rejected(action, busy.to_string())).await;
            return;
        }
    };
    if let Some((launch_id, coordinator)) = action_context.slice_ownership.held_reservation() {
        reject_goal(
            pending,
            encode_rejected(
                action,
                format!(
                    "this daemon is reserved for launch `{launch_id}` by `{coordinator}`; wait \
                     for that operation to finish, or clear the reservation with `peppy stack \
                     reset --core-node {}`",
                    action_context.bound_core_node
                ),
            ),
        )
        .await;
        return;
    }

    let budgets = goal.budgets();

    // `timeout_secs` is gate-reporting only; 0 indicates "no enforced budget"
    // (when --max-timeout-secs is unset).
    let generation = match gate.try_admit(budgets.max_timeout_secs.unwrap_or(0), false) {
        // A stack action never forces, so nothing is ever superseded here.
        Admission::Admitted { generation, .. } => generation,
        Admission::AlreadyRunning { .. } => {
            reject_goal(
                pending,
                encode_rejected(action, "action already in progress"),
            )
            .await;
            return;
        }
    };

    debug!(
        "Received `{}` goal from {sender_instance_id}",
        action.id().name()
    );

    // Create log file with timestamp-based filename
    let log_dir = action_context.peppy_dirs.logs_dir_launch();
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        let error_msg = format!("Failed to create logs directory: {e}");
        debug!("Failed to create logs directory {:?}: {}", log_dir, e);
        gate.clear_running();
        reject_goal(pending, encode_rejected(action, &error_msg)).await;
        return;
    }

    let timestamp = Local::now().format("%Y%m%d_%H%M%S_%3f");
    let log_filename = format!("{}_{}.log", action.label(), timestamp);
    let log_path = log_dir.join(&log_filename);
    let log_file = match File::create(&log_path) {
        Ok(file) => Arc::new(StdMutex::new(file)),
        Err(e) => {
            let error_msg = format!("Failed to create log file: {e}");
            debug!("Failed to create log file {:?}: {}", log_path, e);
            gate.clear_running();
            reject_goal(pending, encode_rejected(action, &error_msg)).await;
            return;
        }
    };

    debug!(
        "Created log file for stack {}: {}",
        action.label(),
        log_path.display()
    );

    // `accept` registers the per-goal context before replying accepted.
    let Some(goal_ctx) = accept_goal(pending, encode_accepted(action, &log_path)).await else {
        gate.clear_running();
        return;
    };

    // Process the change in a separate task to not block the loop.
    let feedback_publisher = goal_ctx
        .feedback_publisher()
        .expect("every stack action declares a feedback topic");
    let log_path_clone = log_path.clone();
    let gate_for_task = gate.clone();
    let cancellation = action_context.slice_ownership.stack.cancellation();
    tokio::spawn(async move {
        let _mutation = mutation;
        // Frees the gate slot on every exit: explicitly before completion on the
        // normal path (via `release_then_complete` below), or on unwind for a
        // panic. A no-op if a later goal already took over.
        let slot = gate_for_task.into_slot_guard(generation);
        let StackActionContext {
            messenger,
            bound_core_node,
            core_instance_id,
            node_stack,
            peppy_dirs,
            timeouts,
            slice_ownership,
            peppy_version,
            daemon_defaults,
            shutdown_token,
            relationships,
        } = action_context;
        // Compute the deadline once. `None` => no overall deadline (idle-only).
        let change_deadline = budgets
            .max_timeout_secs
            .map(|n| Instant::now() + Duration::from_secs(n));
        let idle_timeouts = IdleTimeouts::of(&budgets);
        let ctx = StackChangeContext {
            action,
            cancellation: cancellation.clone(),
            messenger,
            bound_core_node,
            core_instance_id,
            node_stack,
            peppy_dirs,
            feedback_publisher,
            log_file,
            log_path: log_path_clone.clone(),
            env_vars: budgets.env_vars,
            timeouts,
            change_deadline,
            idle_timeouts,
            daemon_defaults,
            shutdown_token,
            relationships,
            slice_ownership,
            peppy_version,
        };
        // Catch panics so a panic inside the change still completes the goal
        // with a failure result, rather than leaving the client to wait out the
        // SDK's retention window for a result that never arrives. Releasing the
        // gate on panic is handled by `slot` above. Mirrors the panic handling
        // in `run_node_run` / `run_node_add` / `run_node_build`.
        let work = finish_on_reset(goal.process(ctx), &cancellation, || {
            LaunchResult::failure(&log_path_clone, "stack operation cancelled by stack reset")
        });
        let result = match AssertUnwindSafe(work).catch_unwind().await {
            Ok(result) => result,
            Err(panic_payload) => {
                let msg = format!(
                    "stack {} task panicked: {}",
                    action.label(),
                    panic_message(&*panic_payload)
                );
                tracing::error!("{}", msg);
                LaunchResult::failure(&log_path_clone, msg)
            }
        };
        drop(_mutation);
        if let Ok(payload) = result.encode() {
            slot.release_then_complete(&goal_ctx, payload).await;
        }
    });
}
