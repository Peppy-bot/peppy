//! Shared accept loop for the built-in core-node actions.
//!
//! Each built-in action (`node_add`, `node_run`, `node_build`, `stack_launch`,
//! `repo_refresh`) is single-goal: it admits one in-flight goal at a time,
//! guarded by a [`super::node::gate::ConcurrencyGate`], and rejects concurrent
//! goals. The loop pulls goals from a [`ConcurrentAction`] and hands each to the
//! action's [`GoalHandler`], which owns the full accept/reject decision.
//!
//! Results are delivered by the engine's goal/result rendezvous, not the old
//! poll-based `"result pending"` protocol: on acceptance the handler replies and
//! registers a per-goal [`peppylib::messaging::GoalContext`] (both happen inside
//! [`PendingGoal::accept`]), then spawns the work that drives the goal to
//! [`peppylib::messaging::GoalContext::complete`]. The client drains feedback to
//! end-of-stream, then fetches the buffered result once.

use peppylib::PeppyResult;
use peppylib::messaging::{ConcurrentAction, GoalContext, PendingGoal};
use peppylib::types::Payload;
use std::future::Future;
use tracing::debug;

/// Handles a single received goal for a built-in single-goal action.
///
/// The handler decodes the goal from [`PendingGoal::request_bytes`], runs
/// admission through the action's gate, and then either [`PendingGoal::reject`]s
/// it or does its pre-accept setup (create the log file, build the accepted
/// `GoalResponse`), [`PendingGoal::accept`]s it (which registers the goal and
/// replies in one step), and spawns the work task that drives the goal to
/// [`peppylib::messaging::GoalContext::complete`].
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

/// Reject a pending goal with an encoded `GoalResponse`. On an encode failure
/// the pending goal is dropped (closing the reply stream) instead of replying.
pub(crate) async fn reject_goal(pending: PendingGoal, encoded: PeppyResult<Payload>) {
    match encoded {
        Ok(payload) => {
            // The core-node action protocol carries its rejection reason in
            // the structured payload, so the envelope reason stays empty.
            let _ = pending.reject(None, payload).await;
        }
        Err(err) => {
            debug!("failed to encode goal rejection: {err}");
        }
    }
}
