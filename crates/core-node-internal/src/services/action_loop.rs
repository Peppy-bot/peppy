//! Shared accept loop for the built-in core-node actions, and how each of
//! their goals ends.
//!
//! Each built-in action (`node_add`, `node_run`, `node_build`, the stack
//! actions, `stack_benchmark`, `repo_refresh`) is single-goal: it admits one
//! in-flight goal at a time, guarded by a
//! [`super::node::gate::ConcurrencyGate`], and refuses concurrent goals. The
//! loop pulls goals from a [`ConcurrentAction`] and hands each to the
//! action's [`GoalHandler`], which owns the full accept/refuse decision.
//!
//! A goal is refused in its goal envelope, with a failure kind and a
//! message ([`refuse_goal`]). An accepted goal ends with a [`GoalEnding`]:
//! completed with its result, failed with a failure kind and a message, or
//! cancelled with the reason of the cancel. The engine's goal/result
//! rendezvous delivers the ending: the client drains feedback to
//! end-of-stream, then fetches the buffered ending once.
//!
//! The daemon's goals keep the default sender grace of zero: the engine
//! cancels the goals of a sender that is gone at once, with the cause
//! [`peppylib::messaging::CancelCause::SenderGone`], and the work ends them
//! as it ends a cancel of their caller.

use config::failure_kind::PeppyFailureKind;
use peppylib::PeppyResult;
use peppylib::messaging::{ConcurrentAction, Failure, GoalContext, PendingGoal};
use peppylib::types::Payload;
use std::future::Future;
use tokio_util::sync::CancellationToken;
use tracing::debug;

/// The reason of a goal that a stack reset ended.
pub(crate) const STACK_RESET_REASON: &str = "cancelled by a stack reset";

/// Handles a single received goal for a built-in single-goal action.
///
/// The handler decodes the goal from [`PendingGoal::request_bytes`], runs
/// admission through the action's gate, and then either refuses it
/// ([`refuse_goal`]) or does its pre-accept setup (create the log file,
/// build the accepted `GoalResponse`), [`PendingGoal::accept`]s it (which
/// registers the goal and replies in one step), and spawns the work task
/// that drives the goal to its [`GoalEnding`].
pub(crate) trait GoalHandler: Clone + Send + 'static {
    fn handle_goal(&self, pending: PendingGoal) -> impl Future<Output = ()> + Send;
}

/// Accept loop shared by all built-in core-node actions. Runs until the goal
/// service closes (the messenger session is torn down).
pub(crate) async fn run_action_loop<H: GoalHandler>(
    mut action: ConcurrentAction,
    handler: H,
) -> crate::Result<()> {
    while let Some(pending) = action.recv_next_goal().await? {
        handler.handle_goal(pending).await;
    }
    Ok(())
}

/// Accept a pending goal with an encoded `GoalResponse`, returning the per-goal
/// context. Registering the goal and replying both happen inside
/// [`PendingGoal::accept`]. Returns `None` (dropping `pending`, which closes the
/// reply stream so the client's `fire_goal` errors) when the response could not
/// be encoded or the accept reply failed; the caller should then release its
/// admission gate.
pub(crate) async fn accept_goal(
    pending: PendingGoal,
    encoded: PeppyResult<Payload>,
) -> Option<GoalContext> {
    match encoded {
        Ok(payload) => match pending.accept(payload).await {
            Ok(ctx) => Some(ctx),
            Err(err) => {
                debug!("failed to reply accepted goal: {err}");
                None
            }
        },
        Err(err) => {
            debug!("failed to encode goal acceptance: {err}");
            None
        }
    }
}

/// Runs `work` under its goal's cancellation: a cancel request from the
/// goal's caller, or the engine's cancel of a goal whose sender is gone,
/// signals `token`, and the work finishes through its own cancel path.
pub(crate) async fn under_goal_cancel<T>(
    goal_ctx: &GoalContext,
    token: &CancellationToken,
    work: impl Future<Output = T>,
) -> T {
    tokio::pin!(work);
    tokio::select! {
        biased;
        result = work.as_mut() => result,
        _ = goal_ctx.cancel_signal() => {
            token.cancel();
            work.await
        }
    }
}

/// Refuses a pending goal with `failure`, in its goal envelope.
pub(crate) async fn refuse_goal(pending: PendingGoal, failure: Failure) {
    if let Err(err) = pending.refuse(failure).await {
        debug!("failed to reply refused goal: {err}");
    }
}

/// The failure of a goal whose payload does not decode.
pub(crate) fn invalid_payload(error: impl std::fmt::Display) -> Failure {
    Failure::new(
        PeppyFailureKind::Rejected,
        format!("invalid payload: {error}"),
    )
}

/// How an accepted goal ends.
#[derive(Debug)]
pub(crate) enum GoalEnding {
    /// The work gave its result: the goal is a success.
    Completed(Payload),
    /// The work failed, with a failure kind and a message.
    Failed(Failure),
    /// A cancel ended the work, for this reason.
    Cancelled(String),
}

impl GoalEnding {
    /// The ending of a goal whose work gave `outcome`, its result encoded:
    /// a failure that a cancel brought about ends the goal cancelled, with
    /// the reason of the cancel (`cancel_reason`); a work that completed
    /// after a cancel completes.
    pub(crate) fn of(
        outcome: Result<core_node_api::Result<Payload>, Failure>,
        cancel_reason: Option<String>,
    ) -> Self {
        match (outcome, cancel_reason) {
            (Ok(Ok(result)), _) => Self::Completed(result),
            (Ok(Err(error)), _) => Self::Failed(Failure::new(
                PeppyFailureKind::AttemptFailed,
                format!("failed to encode the result: {error}"),
            )),
            (Err(_), Some(reason)) => Self::Cancelled(reason),
            (Err(failure), None) => Self::Failed(failure),
        }
    }

    /// Ends the goal of `goal_ctx` with this ending. A cancelled goal gives
    /// its reason and no result.
    pub(crate) async fn end(self, goal_ctx: &GoalContext) {
        let ended = match self {
            Self::Completed(result) => goal_ctx.complete(result).await,
            Self::Failed(failure) => goal_ctx.fail(failure).await,
            Self::Cancelled(reason) => goal_ctx.complete_cancelled(reason, Payload::new()).await,
        };
        if let Err(err) = ended {
            debug!("failed to end the goal: {err}");
        }
    }
}

/// Why the goal of `goal_ctx` was cancelled, when it was: its caller
/// cancelled it, or its sender is gone ([`peppylib::messaging::CancelCause`]),
/// or a stack reset (`reset`) ended it.
pub(crate) fn cancel_reason(goal_ctx: &GoalContext, reset: &CancellationToken) -> Option<String> {
    if let Some(cause) = goal_ctx.cancel_cause() {
        return Some(cause.reason().to_owned());
    }
    reset.is_cancelled().then(|| STACK_RESET_REASON.to_owned())
}
